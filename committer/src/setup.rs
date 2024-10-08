use std::{num::NonZeroU32, time::Duration};

use eth::AwsConfig;
use metrics::{prometheus::Registry, HealthChecker, RegistersMetrics};
use ports::storage::Storage;
use services::{BlockCommitter, CommitListener, Runner, WalletBalanceTracker};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};
use validator::BlockValidator;

use crate::{config, errors::Result, AwsClient, Database, FuelApi, L1};

pub fn wallet_balance_tracker(
    internal_config: &config::Internal,
    registry: &Registry,
    l1: L1,
    cancel_token: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    let wallet_balance_tracker = WalletBalanceTracker::new(l1);

    wallet_balance_tracker.register_metrics(registry);

    schedule_polling(
        internal_config.balance_update_interval,
        wallet_balance_tracker,
        "Wallet Balance Tracker",
        cancel_token,
    )
}

pub fn l1_event_listener(
    internal_config: &config::Internal,
    l1: L1,
    storage: Database,
    registry: &Registry,
    cancel_token: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    let commit_listener_service = CommitListener::new(l1, storage, cancel_token.clone());
    commit_listener_service.register_metrics(registry);

    schedule_polling(
        internal_config.between_eth_event_stream_restablishing_attempts,
        commit_listener_service,
        "Commit Listener",
        cancel_token,
    )
}

pub fn block_committer(
    commit_interval: NonZeroU32,
    l1: L1,
    storage: impl Storage + 'static,
    fuel: FuelApi,
    config: &config::Config,
    registry: &Registry,
    cancel_token: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    let validator = BlockValidator::new(*config.fuel.block_producer_address);

    let block_committer = BlockCommitter::new(l1, storage, fuel, validator, commit_interval);

    block_committer.register_metrics(registry);

    schedule_polling(
        config.app.block_check_interval,
        block_committer,
        "Block Committer",
        cancel_token,
    )
}

pub fn state_committer(
    l1: L1,
    storage: impl Storage + 'static,
    cancel_token: CancellationToken,
    config: &config::Config,
) -> tokio::task::JoinHandle<()> {
    let state_committer = services::StateCommitter::new(l1, storage);

    schedule_polling(
        config.app.block_check_interval,
        state_committer,
        "State Committer",
        cancel_token,
    )
}

pub fn state_importer(
    fuel: FuelApi,
    storage: impl Storage + 'static,
    cancel_token: CancellationToken,
    config: &config::Config,
) -> tokio::task::JoinHandle<()> {
    let validator = BlockValidator::new(*config.fuel.block_producer_address);
    let state_importer = services::StateImporter::new(storage, fuel, validator);

    schedule_polling(
        config.app.block_check_interval,
        state_importer,
        "State Importer",
        cancel_token,
    )
}

pub fn state_listener(
    l1: L1,
    storage: impl Storage + 'static,
    cancel_token: CancellationToken,
    registry: &Registry,
    config: &config::Config,
) -> tokio::task::JoinHandle<()> {
    let state_listener =
        services::StateListener::new(l1, storage, config.app.num_blocks_to_finalize_tx);

    state_listener.register_metrics(registry);

    schedule_polling(
        config.app.block_check_interval,
        state_listener,
        "State Listener",
        cancel_token,
    )
}

pub async fn l1_adapter(
    config: &config::Config,
    internal_config: &config::Internal,
    registry: &Registry,
) -> Result<(L1, HealthChecker)> {
    let aws_config = AwsConfig::from_env().await;

    let aws_client = AwsClient::new(aws_config).await;

    let l1 = L1::connect(
        config.eth.rpc.clone(),
        config.eth.state_contract_address,
        config.eth.main_key_arn.clone(),
        config.eth.blob_pool_key_arn.clone(),
        internal_config.eth_errors_before_unhealthy,
        aws_client,
    )
    .await?;

    l1.register_metrics(registry);

    let health_check = l1.connection_health_checker();

    Ok((l1, health_check))
}

fn schedule_polling(
    polling_interval: Duration,
    mut runner: impl Runner + 'static,
    name: &'static str,
    cancel_token: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            if let Err(e) = runner.run().await {
                error!("{name} encountered an error: {e}");
            }

            if cancel_token.is_cancelled() {
                break;
            }

            tokio::time::sleep(polling_interval).await;
        }

        info!("{name} stopped");
    })
}

pub fn fuel_adapter(
    config: &config::Config,
    internal_config: &config::Internal,
    registry: &Registry,
) -> (FuelApi, HealthChecker) {
    let fuel_adapter = FuelApi::new(
        &config.fuel.graphql_endpoint,
        internal_config.fuel_errors_before_unhealthy,
    );
    fuel_adapter.register_metrics(registry);

    let fuel_connection_health = fuel_adapter.connection_health_checker();

    (fuel_adapter, fuel_connection_health)
}

pub fn logger() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_level(true)
        .with_line_number(true)
        .json()
        .init();
}

pub async fn storage(config: &config::Config) -> Result<Database> {
    let postgres = Database::connect(&config.app.db).await?;
    postgres.migrate().await?;

    Ok(postgres)
}

pub async fn shut_down(
    cancel_token: CancellationToken,
    handles: Vec<JoinHandle<()>>,
    storage: Database,
) -> Result<()> {
    cancel_token.cancel();

    for handle in handles {
        handle.await?;
    }

    storage.close().await;
    Ok(())
}
