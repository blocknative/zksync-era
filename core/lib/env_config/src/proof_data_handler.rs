use zksync_config::configs::ProofDataHandlerConfig;

use crate::{envy_load, FromEnv};

impl FromEnv for ProofDataHandlerConfig {
    fn from_env() -> anyhow::Result<Self> {
        Ok(Self {
            tee_config: envy_load("proof_data_handler.tee", "PROOF_DATA_HANDLER_")?,
            ..envy_load("proof_data_handler", "PROOF_DATA_HANDLER_")?
        })
    }
}

#[cfg(test)]
mod tests {
    use zksync_basic_types::L1BatchNumber;
    use zksync_config::configs::TeeConfig;

    use super::*;
    use crate::test_utils::EnvMutex;

    static MUTEX: EnvMutex = EnvMutex::new();

    fn expected_config() -> ProofDataHandlerConfig {
        ProofDataHandlerConfig {
            http_port: 3320,
            api_url: "2342".to_string(),
            api_poll_duration_in_secs: 123,
            proof_generation_timeout_in_secs: 18000,
            retry_connection_interval_in_secs: 123,
            subscribe_for_zero_chain_id: false,
            tee_config: TeeConfig {
                tee_support: true,
                first_tee_processed_batch: L1BatchNumber(1337),
                tee_proof_generation_timeout_in_secs: 600,
                tee_batch_permanently_ignored_timeout_in_hours: 240,
            },
        }
    }

    #[test]
    fn from_env() {
        let config = r#"
            PROOF_DATA_HANDLER_PROOF_GENERATION_TIMEOUT_IN_SECS="18000"
            PROOF_DATA_HANDLER_HTTP_PORT="3320"
            PROOF_DATA_HANDLER_API_POLL_DURATION_IN_SECS="123"
            PROOF_DATA_HANDLER_RETRY_CONNECTION_INTERVAL_IN_SECS="123"
            PROOF_DATA_HANDLER_API_URL="2342"
            PROOF_DATA_HANDLER_TEE_SUPPORT="true"
            PROOF_DATA_HANDLER_FIRST_TEE_PROCESSED_BATCH="1337"
            PROOF_DATA_HANDLER_TEE_PROOF_GENERATION_TIMEOUT_IN_SECS="600"
            PROOF_DATA_HANDLER_TEE_BATCH_PERMANENTLY_IGNORED_TIMEOUT_IN_HOURS="240"
            PROOF_DATA_HANDLER_SUBSCRIBE_FOR_ZERO_CHAIN_ID="false"
        "#;
        let mut lock = MUTEX.lock();
        lock.set_env(config);
        let actual = ProofDataHandlerConfig::from_env().unwrap();
        assert_eq!(actual, expected_config());
    }
}
