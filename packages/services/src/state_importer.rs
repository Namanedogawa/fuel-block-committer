use async_trait::async_trait;
use ports::{
    fuel::FuelBlock,
    storage::Storage,
    types::{StateFragment, StateSubmission},
};
use tracing::info;
use validator::Validator;

use crate::{Result, Runner};

pub struct StateImporter<Db, Api, BlockValidator> {
    storage: Db,
    fuel_adapter: Api,
    block_validator: BlockValidator,
}

impl<Db, Api, BlockValidator> StateImporter<Db, Api, BlockValidator> {
    pub fn new(storage: Db, fuel_adapter: Api, block_validator: BlockValidator) -> Self {
        Self {
            storage,
            fuel_adapter,
            block_validator,
        }
    }
}

impl<Db, Api, BlockValidator> StateImporter<Db, Api, BlockValidator>
where
    Db: Storage,
    Api: ports::fuel::Api,
    BlockValidator: Validator,
{
    async fn fetch_latest_block(&self) -> Result<FuelBlock> {
        let latest_block = self.fuel_adapter.latest_block().await?;
        self.block_validator.validate(&latest_block)?;
        Ok(latest_block)
    }

    async fn check_if_stale(&self, block_height: u32) -> Result<bool> {
        if let Some(submitted_height) = self.last_submitted_block_height().await? {
            return Ok(submitted_height >= block_height);
        }
        Ok(false)
    }

    async fn last_submitted_block_height(&self) -> Result<Option<u32>> {
        self.storage
            .state_submission_w_latest_block()
            .await
            .map(|submission| submission.map(|s| s.block_height))
    }

    fn block_to_state_submission(
        &self,
        block: FuelBlock,
    ) -> Result<(StateSubmission, Vec<StateFragment>)> {
        use itertools::Itertools;

        let fragments = block
            .transactions
            .iter()
            .flat_map(|tx| tx.iter())
            .chunks(StateFragment::MAX_FRAGMENT_SIZE)
            .into_iter()
            .enumerate()
            .map(|(index, chunk)| StateFragment {
                id: None,
                submission_id: None,
                fragment_idx: index as u32,
                data: chunk.copied().collect(),
                created_at: ports::types::Utc::now(),
            })
            .collect();

        let submission = StateSubmission {
            id: None,
            block_hash: *block.id,
            block_height: block.header.height,
        };

        Ok((submission, fragments))
    }

    async fn import_state(&self, block: FuelBlock) -> Result<()> {
        let (submission, fragments) = self.block_to_state_submission(block)?;
        self.storage
            .insert_state_submission(submission, fragments)
            .await?;
        Ok(())
    }
}

#[async_trait]
impl<Db, Api, BlockValidator> Runner for StateImporter<Db, Api, BlockValidator>
where
    Db: Storage,
    Api: ports::fuel::Api + Send + Sync,
    BlockValidator: Validator,
{
    async fn run(&mut self) -> Result<()> {
        let block = self.fetch_latest_block().await?;

        if self.check_if_stale(block.header.height).await? || block.transactions.is_empty() {
            return Ok(());
        }

        let block_id = block.id;
        let block_height = block.header.height;
        self.import_state(block).await?;
        info!(
            "Imported state from Fuel block: height: {}, id: {}",
            block_height, block_id
        );

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use fuel_crypto::{Message, SecretKey, Signature};
    use ports::fuel::{FuelBlock, FuelBlockId, FuelConsensus, FuelHeader, FuelPoAConsensus};
    use rand::{rngs::StdRng, SeedableRng};
    use storage::PostgresProcess;
    use validator::BlockValidator;

    use super::*;

    fn given_secret_key() -> SecretKey {
        SecretKey::random(&mut StdRng::seed_from_u64(42))
    }

    fn given_a_block(height: u32, secret_key: &SecretKey) -> FuelBlock {
        let header = given_header(height);

        let mut hasher = fuel_crypto::Hasher::default();
        hasher.input(header.prev_root.as_ref());
        hasher.input(header.height.to_be_bytes());
        hasher.input(header.time.0.to_be_bytes());
        hasher.input(header.application_hash.as_ref());

        let id = FuelBlockId::from(hasher.digest());
        let id_message = Message::from_bytes(*id);
        let signature = Signature::sign(secret_key, &id_message);

        FuelBlock {
            id,
            header,
            consensus: FuelConsensus::PoAConsensus(FuelPoAConsensus { signature }),
            transactions: vec![[2u8; 32].into()],
            block_producer: Some(secret_key.public_key()),
        }
    }

    fn given_header(height: u32) -> FuelHeader {
        let application_hash = "0x8b96f712e293e801d53da77113fec3676c01669c6ea05c6c92a5889fce5f649d"
            .parse()
            .unwrap();

        FuelHeader {
            id: Default::default(),
            da_height: Default::default(),
            consensus_parameters_version: Default::default(),
            state_transition_bytecode_version: Default::default(),
            transactions_count: 1,
            message_receipt_count: Default::default(),
            transactions_root: Default::default(),
            message_outbox_root: Default::default(),
            event_inbox_root: Default::default(),
            height,
            prev_root: Default::default(),
            time: tai64::Tai64(0),
            application_hash,
        }
    }

    fn given_fetcher(block: FuelBlock) -> ports::fuel::MockApi {
        let mut fetcher = ports::fuel::MockApi::new();

        fetcher
            .expect_latest_block()
            .returning(move || Ok(block.clone()));

        fetcher
    }

    #[tokio::test]
    async fn test_import_state() -> Result<()> {
        // given
        let secret_key = given_secret_key();
        let block = given_a_block(1, &secret_key);
        let fuel_mock = given_fetcher(block);
        let block_validator = BlockValidator::new(*secret_key.public_key().hash());

        let process = PostgresProcess::shared().await.unwrap();
        let db = process.create_random_db().await?;
        let mut importer = StateImporter::new(db.clone(), fuel_mock, block_validator);

        // when
        importer.run().await.unwrap();

        // then
        let fragments = db.get_unsubmitted_fragments().await?;
        let latest_submission = db.state_submission_w_latest_block().await?.unwrap();
        assert_eq!(fragments.len(), 1);
        assert_eq!(fragments[0].submission_id, latest_submission.id);

        Ok(())
    }
}
