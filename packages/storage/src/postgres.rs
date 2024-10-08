use ports::types::{
    BlockSubmission, StateFragment, StateSubmission, SubmissionTx, TransactionState,
};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

use super::error::{Error, Result};
use crate::tables;

#[derive(Clone)]
pub struct Postgres {
    connection_pool: sqlx::Pool<sqlx::Postgres>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct DbConfig {
    /// The hostname or IP address of the `PostgreSQL` server.
    pub host: String,
    /// The port number on which the `PostgreSQL` server is listening.
    pub port: u16,
    /// The username used to authenticate with the `PostgreSQL` server.
    pub username: String,
    /// The password used to authenticate with the `PostgreSQL` server.
    pub password: String,
    /// The name of the database to connect to on the `PostgreSQL` server.
    pub database: String,
    /// The maximum number of connections allowed in the connection pool.
    pub max_connections: u32,
    /// Whether to use SSL when connecting to the `PostgreSQL` server.
    pub use_ssl: bool,
}

impl Postgres {
    pub async fn connect(opt: &DbConfig) -> ports::storage::Result<Self> {
        let ssl_mode = if opt.use_ssl {
            sqlx::postgres::PgSslMode::Require
        } else {
            sqlx::postgres::PgSslMode::Disable
        };

        let options = PgConnectOptions::new()
            .ssl_mode(ssl_mode)
            .username(&opt.username)
            .password(&opt.password)
            .database(&opt.database)
            .host(&opt.host)
            .port(opt.port);

        let connection_pool = PgPoolOptions::new()
            .max_connections(opt.max_connections)
            .connect_with(options)
            .await
            .map_err(Error::from)?;

        Ok(Self { connection_pool })
    }

    #[cfg(feature = "test-helpers")]
    pub fn db_name(&self) -> String {
        self.connection_pool
            .connect_options()
            .get_database()
            .expect("database name to be set")
            .to_owned()
    }

    #[cfg(feature = "test-helpers")]
    pub fn port(&self) -> u16 {
        self.connection_pool.connect_options().get_port()
    }

    /// Close only when shutting down the application. Will close the connection pool even if it is
    /// shared.
    pub async fn close(self) {
        self.connection_pool.close().await;
    }

    pub async fn migrate(&self) -> ports::storage::Result<()> {
        sqlx::migrate!()
            .run(&self.connection_pool)
            .await
            .map_err(Error::from)?;
        Ok(())
    }

    #[cfg(feature = "test-helpers")]
    pub(crate) async fn execute(&self, query: &str) -> Result<()> {
        sqlx::query(query).execute(&self.connection_pool).await?;
        Ok(())
    }

    pub(crate) async fn insert_submission(&self, submission: BlockSubmission) -> Result<()> {
        let row = tables::L1FuelBlockSubmission::from(submission);
        sqlx::query!(
            "INSERT INTO l1_fuel_block_submission (fuel_block_hash, fuel_block_height, completed, submittal_height) VALUES ($1, $2, $3, $4)",
            row.fuel_block_hash,
            row.fuel_block_height,
            row.completed,
            row.submittal_height
        )
        .execute(&self.connection_pool)
        .await?;
        Ok(())
    }

    pub(crate) async fn get_latest_submission(&self) -> Result<Option<BlockSubmission>> {
        sqlx::query_as!(
            tables::L1FuelBlockSubmission,
            "SELECT * FROM l1_fuel_block_submission ORDER BY fuel_block_height DESC LIMIT 1"
        )
        .fetch_optional(&self.connection_pool)
        .await?
        .map(BlockSubmission::try_from)
        .transpose()
    }

    pub(crate) async fn mark_submission_completed(
        &self,
        fuel_block_hash: [u8; 32],
    ) -> Result<BlockSubmission> {
        let updated_row = sqlx::query_as!(
            tables::L1FuelBlockSubmission,
            "UPDATE l1_fuel_block_submission SET completed = true WHERE fuel_block_hash = $1 RETURNING *",
            fuel_block_hash.as_slice(),
        )
        .fetch_optional(&self.connection_pool)
        .await?;

        updated_row
            .map(BlockSubmission::try_from)
            .transpose()?
            .ok_or_else(|| {
                let hash = hex::encode(fuel_block_hash);
                Error::Database(format!(
                    "Cannot mark submission as completed! Submission of block `{hash}` not found in DB."
                ))
            })
    }

    pub(crate) async fn insert_state_submission(
        &self,
        state: StateSubmission,
        fragments: Vec<StateFragment>,
    ) -> Result<()> {
        if fragments.is_empty() {
            return Err(Error::Database("Cannot insert state with no fragments".to_string()));
        }

        let state_row = tables::L1StateSubmission::from(state);
        let fragment_rows: Vec<_> = fragments.into_iter().map(tables::L1StateFragment::from).collect();

        let mut transaction = self.connection_pool.begin().await?;

        let submission_id = sqlx::query!(
            "INSERT INTO l1_submissions (fuel_block_hash, fuel_block_height) VALUES ($1, $2) RETURNING id",
            state_row.fuel_block_hash,
            state_row.fuel_block_height
        )
        .fetch_one(&mut *transaction)
        .await?.id;

        for fragment_row in fragment_rows {
            sqlx::query!(
                "INSERT INTO l1_fragments (fragment_idx, submission_id, data, created_at) VALUES ($1, $2, $3, $4)",
                fragment_row.fragment_idx,
                submission_id,
                fragment_row.data,
                fragment_row.created_at
            )
            .execute(&mut *transaction)
            .await?;
        }

        transaction.commit().await?;
        Ok(())
    }

    pub(crate) async fn get_unsubmitted_fragments(&self) -> Result<Vec<StateFragment>> {
        const BLOB_LIMIT: i64 = 6;
        let rows = sqlx::query_as!(
            tables::L1StateFragment,
            "SELECT l1_fragments.*
            FROM l1_fragments
            WHERE l1_fragments.id NOT IN (
                SELECT l1_fragments.id
                FROM l1_fragments
                JOIN l1_transaction_fragments ON l1_fragments.id = l1_transaction_fragments.fragment_id
                JOIN l1_transactions ON l1_transaction_fragments.transaction_id = l1_transactions.id
                WHERE l1_transactions.state IN ($1, $2)
            )
            ORDER BY l1_fragments.created_at
            LIMIT $3;",
            TransactionState::Finalized.into_i16(),
            TransactionState::Pending.into_i16(),
            BLOB_LIMIT
        )
        .fetch_all(&self.connection_pool)
        .await?
        .into_iter()
        .map(StateFragment::try_from);

        rows.collect::<Result<Vec<_>>>()
    }

    pub(crate) async fn record_pending_tx(
        &self,
        tx_hash: [u8; 32],
        fragment_ids: Vec<u32>,
    ) -> Result<()> {
        let mut transaction = self.connection_pool.begin().await?;

        let transaction_id = sqlx::query!(
            "INSERT INTO l1_transactions (hash, state) VALUES ($1, $2) RETURNING id",
            tx_hash.as_slice(),
            TransactionState::Pending.into_i16(),
        )
        .fetch_one(&mut *transaction)
        .await?
        .id;

        for fragment_id in fragment_ids {
            sqlx::query!(
                "INSERT INTO l1_transaction_fragments (transaction_id, fragment_id) VALUES ($1, $2)",
                transaction_id,
                fragment_id as i64
            )
            .execute(&mut *transaction)
            .await?;
        }

        transaction.commit().await?;
        Ok(())
    }

    pub(crate) async fn has_pending_txs(&self) -> Result<bool> {
        Ok(sqlx::query!(
            "SELECT EXISTS (SELECT 1 FROM l1_transactions WHERE state = $1) AS has_pending_transactions;",
            TransactionState::Pending.into_i16()
        )
        .fetch_one(&self.connection_pool)
        .await?
        .has_pending_transactions.unwrap_or(false))
    }

    pub(crate) async fn get_pending_txs(&self) -> Result<Vec<SubmissionTx>> {
        sqlx::query_as!(
            tables::L1SubmissionTx,
            "SELECT * FROM l1_transactions WHERE state = $1",
            TransactionState::Pending.into_i16()
        )
        .fetch_all(&self.connection_pool)
        .await?
        .into_iter()
        .map(SubmissionTx::try_from)
        .collect::<Result<Vec<_>>>()
    }

    pub(crate) async fn get_latest_state_submission(
        &self,
    ) -> Result<Option<StateSubmission>> {
        sqlx::query_as!(
            tables::L1StateSubmission,
            "SELECT * FROM l1_submissions ORDER BY fuel_block_height DESC LIMIT 1"
        )
        .fetch_optional(&self.connection_pool)
        .await?
        .map(StateSubmission::try_from)
        .transpose()
    }

    pub(crate) async fn update_submission_tx_state(
        &self,
        hash: [u8; 32],
        state: TransactionState,
    ) -> Result<()> {
        sqlx::query!(
            "UPDATE l1_transactions SET state = $1 WHERE hash = $2",
            state.into_i16(),
            hash.as_slice(),
        )
        .execute(&self.connection_pool)
        .await?;
        Ok(())
    }
}
