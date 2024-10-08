use std::{path::Path, time::Duration};

use anyhow::Context;
use ports::types::Address;
use url::Url;

#[derive(Default)]
pub struct Committer {
    show_logs: bool,
    main_key_arn: Option<String>,
    blob_key_arn: Option<String>,
    state_contract_address: Option<String>,
    eth_rpc: Option<Url>,
    fuel_rpc: Option<Url>,
    fuel_block_producer_addr: Option<String>,
    db_port: Option<u16>,
    db_name: Option<String>,
    kms_url: Option<String>,
}

impl Committer {
    pub async fn start(self) -> anyhow::Result<CommitterProcess> {
        let config =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../configurations/development/config.toml");

        macro_rules! get_field {
            ($field:ident) => {
                self.$field
                    .ok_or_else(|| anyhow::anyhow!(concat!(stringify!($field), " not provided")))?
            };
        }
        let unused_port = portpicker::pick_unused_port()
            .ok_or_else(|| anyhow::anyhow!("No free port to start fuel-block-committer"))?;

        let kms_url = get_field!(kms_url);
        let mut cmd = tokio::process::Command::new("fuel-block-committer");
        cmd.arg(config)
            .env("E2E_TEST_AWS_ENDPOINT", kms_url)
            .env("AWS_REGION", "us-east-1")
            .env("AWS_ACCESS_KEY_ID", "test")
            .env("AWS_SECRET_ACCESS_KEY", "test")
            .env("COMMITTER__ETH__MAIN_KEY_ARN", get_field!(main_key_arn))
            .env("COMMITTER__ETH__RPC", get_field!(eth_rpc).as_str())
            .env(
                "COMMITTER__ETH__STATE_CONTRACT_ADDRESS",
                get_field!(state_contract_address),
            )
            .env(
                "COMMITTER__FUEL__GRAPHQL_ENDPOINT",
                get_field!(fuel_rpc).as_str(),
            )
            .env(
                "COMMITTER__FUEL__BLOCK_PRODUCER_ADDRESS",
                get_field!(fuel_block_producer_addr),
            )
            .env("COMMITTER__APP__DB__PORT", get_field!(db_port).to_string())
            .env("COMMITTER__APP__DB__DATABASE", get_field!(db_name))
            .env("COMMITTER__APP__PORT", unused_port.to_string())
            .current_dir(Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap())
            .kill_on_drop(true);

        if let Some(blob_wallet_key_arn) = self.blob_key_arn {
            cmd.env("COMMITTER__ETH__BLOB_POOL_KEY_ARN", blob_wallet_key_arn);
        }

        let sink = if self.show_logs {
            std::process::Stdio::inherit
        } else {
            std::process::Stdio::null
        };
        cmd.stdout(sink()).stderr(sink());

        let child = cmd.spawn().with_context(||"couldn't find `fuel-block-committer` on PATH. Either use `run_tests.sh` or place the binary on the PATH.")?;

        Ok(CommitterProcess {
            _child: child,
            port: unused_port,
        })
    }

    pub fn with_main_key_arn(mut self, wallet_arn: String) -> Self {
        self.main_key_arn = Some(wallet_arn);
        self
    }

    pub fn with_kms_url(mut self, kms_url: String) -> Self {
        self.kms_url = Some(kms_url);
        self
    }

    pub fn with_blob_key_arn(mut self, blob_wallet_arn: String) -> Self {
        self.blob_key_arn = Some(blob_wallet_arn);
        self
    }

    pub fn with_state_contract_address(mut self, state_contract_address: Address) -> Self {
        self.state_contract_address = Some(hex::encode(state_contract_address));
        self
    }

    pub fn with_eth_rpc(mut self, eth_rpc: Url) -> Self {
        self.eth_rpc = Some(eth_rpc);
        self
    }

    pub fn with_fuel_rpc(mut self, fuel_rpc: Url) -> Self {
        self.fuel_rpc = Some(fuel_rpc);
        self
    }

    pub fn with_fuel_block_producer_addr(mut self, fuel_block_producer_addr: [u8; 32]) -> Self {
        self.fuel_block_producer_addr = Some(hex::encode(fuel_block_producer_addr));
        self
    }

    pub fn with_db_port(mut self, db_port: u16) -> Self {
        self.db_port = Some(db_port);
        self
    }

    pub fn with_db_name(mut self, db_name: String) -> Self {
        self.db_name = Some(db_name);
        self
    }

    pub fn with_show_logs(mut self, show_logs: bool) -> Self {
        self.show_logs = show_logs;
        self
    }
}

pub struct CommitterProcess {
    _child: tokio::process::Child,
    port: u16,
}

impl CommitterProcess {
    pub async fn wait_for_committed_block(&self, height: u64) -> anyhow::Result<()> {
        loop {
            match self.fetch_latest_committed_block().await {
                Ok(current_height) if current_height >= height => break,
                _ => {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }
            }
        }
        Ok(())
    }

    pub async fn wait_for_committed_blob(&self) -> anyhow::Result<()> {
        loop {
            match self.fetch_latest_blob_block().await {
                Ok(_) => break,
                _ => {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }
            }
        }
        Ok(())
    }

    async fn fetch_latest_committed_block(&self) -> anyhow::Result<u64> {
        self.fetch_metric_value("latest_committed_block").await
    }

    async fn fetch_latest_blob_block(&self) -> anyhow::Result<u64> {
        self.fetch_metric_value("last_eth_block_w_blob").await
    }

    async fn fetch_metric_value(&self, metric_name: &str) -> anyhow::Result<u64> {
        let response = reqwest::get(format!("http://localhost:{}/metrics", self.port))
            .await?
            .error_for_status()?
            .text()
            .await?;

        let height_line = response
            .lines()
            .find(|line| line.starts_with(metric_name))
            .ok_or_else(|| anyhow::anyhow!("couldn't find {} metric", metric_name))?;

        Ok(height_line
            .split_whitespace()
            .last()
            .expect("metric format to be in the format 'NAME VAL'")
            .parse()?)
    }
}
