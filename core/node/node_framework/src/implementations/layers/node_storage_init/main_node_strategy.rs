use std::{num::NonZeroUsize, sync::Arc};

use zksync_config::{
    configs::object_store::ObjectStoreMode, ContractsConfig, GenesisConfig, ObjectStoreConfig,
};
use zksync_node_storage_init::{
    external_node::NodeRecovery, main_node::MainNodeGenesis, InitializeStorage,
    NodeInitializationStrategy, SnapshotRecoveryConfig,
};

use super::NodeInitializationStrategyResource;
use crate::{
    implementations::resources::{
        blob_client::BlobClientResource,
        eth_interface::EthInterfaceResource,
        healthcheck::AppHealthCheckResource,
        pools::{MasterPool, PoolResource},
    },
    wiring_layer::{WiringError, WiringLayer},
    FromContext, IntoContext,
};

/// Wiring layer for main node initialization strategy.
#[derive(Debug)]
pub struct MainNodeInitStrategyLayer {
    pub genesis: GenesisConfig,
    pub l1_recovery_enabled: bool,
    pub contracts: ContractsConfig,
}

#[derive(Debug, FromContext)]
#[context(crate = crate)]
pub struct Input {
    pub master_pool: PoolResource<MasterPool>,
    pub eth_interface: EthInterfaceResource,
    pub app_health: AppHealthCheckResource,
    pub blob_client_resource: BlobClientResource,
}

#[derive(Debug, IntoContext)]
#[context(crate = crate)]
pub struct Output {
    pub strategy: NodeInitializationStrategyResource,
}

#[async_trait::async_trait]
impl WiringLayer for MainNodeInitStrategyLayer {
    type Input = Input;
    type Output = Output;

    fn layer_name(&self) -> &'static str {
        "main_node_role_layer"
    }

    async fn wire(self, input: Self::Input) -> Result<Self::Output, WiringError> {
        let pool = input.master_pool.get().await?;
        let EthInterfaceResource(l1_client) = input.eth_interface;
        let genesis = Arc::new(MainNodeGenesis {
            contracts: self.contracts.clone(),
            genesis: self.genesis,
            l1_client: l1_client.clone(),
            pool: pool.clone(),
        });

        let snapshot_recovery = if self.l1_recovery_enabled {
            // Add a connection for checking whether the storage is initialized.
            let recovery_pool = input.master_pool.get_custom(10).await?;
            let recovery: Arc<dyn InitializeStorage> = Arc::new(NodeRecovery {
                main_node_client: None,
                l1_client: l1_client.clone(),
                pool: recovery_pool,
                max_concurrency: NonZeroUsize::new(5).unwrap(),
                recovery_config: SnapshotRecoveryConfig {
                    recover_from_l1: true,
                    recover_main_node_components: true,
                    snapshot_l1_batch_override: None,
                    drop_storage_key_preimages: false,
                    object_store_config: Some(ObjectStoreConfig {
                        mode: ObjectStoreMode::FileBacked {
                            file_backed_base_path: "l1-recovery-main-node-snapshots"
                                .parse()
                                .unwrap(),
                        },
                        max_retries: 0,
                        local_mirror_path: None,
                    }),
                },
                app_health: input.app_health.0,
                diamond_proxy_addr: self.contracts.diamond_proxy_addr,
                blob_client: Some(input.blob_client_resource.0),
            });
            Some(recovery)
        } else {
            None
        };
        let strategy = NodeInitializationStrategy {
            genesis,
            snapshot_recovery,
            block_reverter: None,
        };

        Ok(Output {
            strategy: strategy.into(),
        })
    }
}
