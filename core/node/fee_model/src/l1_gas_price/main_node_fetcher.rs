use std::{
    sync::{Arc, RwLock},
    time::Duration,
};

use tokio::sync::watch::Receiver;
use zksync_types::fee_model::{
    BatchFeeInput, FeeModelConfigV1, FeeParams, FeeParamsV1, FeeParamsV2,
};
use zksync_web3_decl::{
    client::{DynClient, L2},
    error::ClientRpcContext,
    namespaces::ZksNamespaceClient,
};

use crate::BatchFeeModelInputProvider;

const SLEEP_INTERVAL: Duration = Duration::from_secs(5);

/// This structure maintains the known L1 gas price by periodically querying
/// the main node.
/// It is required since the main node doesn't only observe the current L1 gas price,
/// but also applies adjustments to it in order to smooth out the spikes.
/// The same algorithm cannot be consistently replicated on the external node side,
/// since it relies on the configuration, which may change.
#[derive(Debug)]
pub struct MainNodeFeeParamsFetcher {
    client: Box<DynClient<L2>>,
    main_node_fee_params: RwLock<FeeParams>,
    main_node_batch_fee_input: RwLock<Option<BatchFeeInput>>,
}

impl MainNodeFeeParamsFetcher {
    pub fn new(client: Box<DynClient<L2>>) -> Self {
        Self {
            client: client.for_component("fee_params_fetcher"),
            main_node_fee_params: RwLock::new(FeeParams::sensible_v1_default()),
            main_node_batch_fee_input: RwLock::new(None),
        }
    }

    pub async fn run(self: Arc<Self>, mut stop_receiver: Receiver<bool>) -> anyhow::Result<()> {
        while !*stop_receiver.borrow_and_update() {
            let fetch_result = self
                .client
                .get_fee_params()
                .rpc_context("get_fee_params")
                .await;
            let main_node_fee_params = match fetch_result {
                Ok(price) => price,
                Err(err) => {
                    tracing::warn!("Unable to get the gas price: {}", err);
                    // A delay to avoid spamming the main node with requests.
                    if tokio::time::timeout(SLEEP_INTERVAL, stop_receiver.changed())
                        .await
                        .is_ok()
                    {
                        break;
                    }
                    continue;
                }
            };
            *self.main_node_fee_params.write().unwrap() = main_node_fee_params;

            let fetch_result = self
                .client
                .get_batch_fee_input()
                .rpc_context("get_batch_fee_input")
                .await;
            let main_node_fee_params = match fetch_result {
                Ok(price) => price,
                Err(err) => {
                    tracing::warn!("Unable to get the batch fee input: {}", err);
                    // A delay to avoid spamming the main node with requests.
                    if tokio::time::timeout(SLEEP_INTERVAL, stop_receiver.changed())
                        .await
                        .is_ok()
                    {
                        break;
                    }
                    continue;
                }
            };
            *self.main_node_batch_fee_input.write().unwrap() =
                Some(BatchFeeInput::PubdataIndependent(main_node_fee_params));

            if tokio::time::timeout(SLEEP_INTERVAL, stop_receiver.changed())
                .await
                .is_ok()
            {
                break;
            }
        }

        tracing::info!("Stop signal received, MainNodeFeeParamsFetcher is shutting down");
        Ok(())
    }
}

impl BatchFeeModelInputProvider for MainNodeFeeParamsFetcher {
    fn get_fee_model_params(&self) -> FeeParams {
        let fee_params = *self.main_node_fee_params.read().unwrap();
        let batch_fee_input = *self.main_node_batch_fee_input.read().unwrap();
        let Some(batch_fee_input) = batch_fee_input else {
            return fee_params;
        };
        match fee_params {
            FeeParams::V1(..) => FeeParams::V1(FeeParamsV1 {
                config: FeeModelConfigV1 {
                    minimal_l2_gas_price: batch_fee_input.fair_l2_gas_price(),
                },
                l1_gas_price: batch_fee_input.l1_gas_price(),
            }),
            FeeParams::V2(params) => {
                return FeeParams::V2(FeeParamsV2::new(
                    params.config(),
                    batch_fee_input.l1_gas_price(),
                    batch_fee_input.fair_pubdata_price(),
                    params.conversion_ratio(),
                ));
            }
        }
    }
}
