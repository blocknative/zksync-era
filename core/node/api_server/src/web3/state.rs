use std::{
    future::Future,
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc,
    },
    time::Instant,
};

use anyhow::Context as _;
use futures::TryFutureExt;
use lru::LruCache;
use tokio::sync::{Mutex, RwLock};
use vise::GaugeGuard;
use zksync_config::{
    configs::{
        api::Web3JsonRpcConfig,
        contracts::{ecosystem::L1SpecificContracts, ChainSpecificContracts},
    },
    GenesisConfig,
};
use zksync_dal::{Connection, ConnectionPool, Core, CoreDal, DalError};
use zksync_metadata_calculator::api_server::TreeApiClient;
use zksync_node_sync::SyncState;
use zksync_system_constants::ETHEREUM_ADDRESS;
use zksync_types::{
    api, api::BridgeAddresses, commitment::L1BatchCommitmentMode, l2::L2Tx,
    transaction_request::CallRequest, Address, L1BatchNumber, L1ChainId, L2BlockNumber, L2ChainId,
    H256, U256, U64,
};
use zksync_web3_decl::{
    client::{DynClient, L2},
    error::Web3Error,
    types::Filter,
};

use super::{
    backend_jsonrpsee::MethodTracer,
    mempool_cache::MempoolCache,
    metrics::{FilterType, FILTER_METRICS},
    TypedFilter,
};
use crate::{
    execution_sandbox::{BlockArgs, BlockArgsError, BlockStartInfo},
    tx_sender::{tx_sink::TxSink, TxSender},
};

#[derive(Debug)]
pub(super) enum PruneQuery {
    BlockId(api::BlockId),
    L1Batch(L1BatchNumber),
}

impl From<api::BlockId> for PruneQuery {
    fn from(id: api::BlockId) -> Self {
        Self::BlockId(id)
    }
}

impl From<L2BlockNumber> for PruneQuery {
    fn from(number: L2BlockNumber) -> Self {
        Self::BlockId(api::BlockId::Number(number.0.into()))
    }
}

impl From<L1BatchNumber> for PruneQuery {
    fn from(number: L1BatchNumber) -> Self {
        Self::L1Batch(number)
    }
}

impl From<BlockArgsError> for Web3Error {
    fn from(value: BlockArgsError) -> Self {
        match value {
            BlockArgsError::Pruned(l2_block) => Web3Error::PrunedBlock(l2_block),
            BlockArgsError::Missing => Web3Error::NoBlock,
            BlockArgsError::Database(error) => Web3Error::InternalError(error),
        }
    }
}

impl BlockStartInfo {
    pub(super) async fn ensure_not_pruned(
        &self,
        query: impl Into<PruneQuery>,
        storage: &mut Connection<'_, Core>,
    ) -> Result<(), Web3Error> {
        match query.into() {
            PruneQuery::BlockId(id) => Ok(self.ensure_not_pruned_block(id, storage).await?),
            PruneQuery::L1Batch(number) => {
                let first_l1_batch = self.first_l1_batch(storage).await?;
                if number < first_l1_batch {
                    return Err(Web3Error::PrunedL1Batch(first_l1_batch));
                }
                Ok(())
            }
        }
    }
}

/// Configuration values for the API.
/// This structure is detached from `ZkSyncConfig`, since different node types (main, external, etc.)
/// may require different configuration layouts.
/// The intention is to only keep the actually used information here.
#[derive(Debug, Clone)]
pub struct InternalApiConfigBuilder {
    /// Chain ID of the L1 network. Note, that it may be different from the chain id of the settlement layer.
    pub l1_chain_id: L1ChainId,
    pub l2_chain_id: L2ChainId,
    pub dummy_verifier: bool,
    pub l1_batch_commit_data_generator_mode: L1BatchCommitmentMode,
    pub max_tx_size: Option<usize>,
    pub estimate_gas_scale_factor: Option<f64>,
    pub estimate_gas_acceptable_overestimation: Option<u32>,
    pub estimate_gas_optimize_search: Option<bool>,
    pub bridge_addresses: Option<BridgeAddresses>,
    pub l1_bytecodes_supplier_addr: Option<Address>,
    pub l1_wrapped_base_token_store: Option<Address>,
    pub l1_bridgehub_proxy_addr: Option<Address>,
    pub l1_state_transition_proxy_addr: Option<Address>,
    pub l1_transparent_proxy_admin_addr: Option<Address>,
    pub l1_diamond_proxy_addr: Option<Address>,
    pub l2_testnet_paymaster_addr: Option<Address>,
    pub base_token_address: Option<Address>,
    pub timestamp_asserter_address: Option<Address>,
    pub l1_server_notifier_addr: Option<Address>,
    pub req_entities_limit: Option<usize>,
    pub fee_history_limit: Option<u64>,
    pub filters_disabled: Option<bool>,
}

impl InternalApiConfigBuilder {
    pub fn from_genesis(genesis: &GenesisConfig) -> Self {
        Self {
            l1_chain_id: genesis.l1_chain_id,
            l2_chain_id: genesis.l2_chain_id,
            dummy_verifier: genesis.dummy_verifier,
            l1_batch_commit_data_generator_mode: genesis.l1_batch_commit_data_generator_mode,
            max_tx_size: None,
            estimate_gas_scale_factor: None,
            estimate_gas_acceptable_overestimation: None,
            estimate_gas_optimize_search: None,
            bridge_addresses: None,
            l1_bytecodes_supplier_addr: None,
            l1_wrapped_base_token_store: None,
            l1_bridgehub_proxy_addr: None,
            l1_state_transition_proxy_addr: None,
            l1_transparent_proxy_admin_addr: None,
            l1_diamond_proxy_addr: None,
            l2_testnet_paymaster_addr: None,
            req_entities_limit: None,
            fee_history_limit: None,
            base_token_address: None,
            filters_disabled: None,
            timestamp_asserter_address: None,
            l1_server_notifier_addr: None,
        }
    }

    pub fn with_web3_config(mut self, web3_config: Web3JsonRpcConfig) -> Self {
        self.max_tx_size = Some(web3_config.max_tx_size);
        self.estimate_gas_scale_factor = Some(web3_config.estimate_gas_scale_factor);
        self.estimate_gas_acceptable_overestimation =
            Some(web3_config.estimate_gas_acceptable_overestimation);
        self.estimate_gas_optimize_search = Some(web3_config.estimate_gas_optimize_search);
        self.req_entities_limit = Some(web3_config.req_entities_limit());
        self.fee_history_limit = Some(web3_config.fee_history_limit());
        self.filters_disabled = Some(web3_config.filters_disabled);
        self
    }

    pub fn with_contracts(
        mut self,
        contracts_config: ChainSpecificContracts,
        l1_ecosystem_contracts: L1SpecificContracts,
    ) -> Self {
        self.base_token_address = l1_ecosystem_contracts.base_token_address;
        self.bridge_addresses = Some(BridgeAddresses {
            l1_erc20_default_bridge: l1_ecosystem_contracts.erc_20_bridge,
            l2_erc20_default_bridge: contracts_config.l2_contracts.erc20_default_bridge,
            l1_shared_default_bridge: l1_ecosystem_contracts.shared_bridge,
            l2_shared_default_bridge: contracts_config.l2_contracts.shared_bridge_addr,
            // WETH bridge is not available
            l1_weth_bridge: None,
            l2_weth_bridge: None,
            l2_legacy_shared_bridge: contracts_config.l2_contracts.legacy_shared_bridge_addr,
        });
        self.l1_bridgehub_proxy_addr = l1_ecosystem_contracts.bridge_hub;

        self.l1_state_transition_proxy_addr = contracts_config
            .ecosystem_contracts
            .state_transition_proxy_addr;

        self.l1_bytecodes_supplier_addr = l1_ecosystem_contracts.bytecodes_supplier_addr;
        self.l1_wrapped_base_token_store = l1_ecosystem_contracts.wrapped_base_token_store;
        self.l1_diamond_proxy_addr = Some(l1_ecosystem_contracts.l1_diamond_proxy);
        self.l2_testnet_paymaster_addr = contracts_config.l2_contracts.testnet_paymaster_addr;
        self
    }

    pub fn build(self) -> InternalApiConfig {
        InternalApiConfig {
            l1_chain_id: self.l1_chain_id,
            l2_chain_id: self.l2_chain_id,
            max_tx_size: self.max_tx_size.unwrap(),
            estimate_gas_scale_factor: self.estimate_gas_scale_factor.unwrap(),
            estimate_gas_acceptable_overestimation: self
                .estimate_gas_acceptable_overestimation
                .unwrap(),
            estimate_gas_optimize_search: self.estimate_gas_optimize_search.unwrap(),
            bridge_addresses: self.bridge_addresses.unwrap(),
            l1_bytecodes_supplier_addr: self.l1_bytecodes_supplier_addr,
            l1_wrapped_base_token_store: self.l1_wrapped_base_token_store,
            l1_bridgehub_proxy_addr: self.l1_bridgehub_proxy_addr,
            l1_state_transition_proxy_addr: self.l1_state_transition_proxy_addr,
            l1_transparent_proxy_admin_addr: self.l1_transparent_proxy_admin_addr,
            l2_testnet_paymaster_addr: self.l2_testnet_paymaster_addr,
            base_token_address: self.base_token_address,
            l1_diamond_proxy_addr: self.l1_diamond_proxy_addr.unwrap(),
            req_entities_limit: self.req_entities_limit.unwrap(),
            fee_history_limit: self.fee_history_limit.unwrap(),
            filters_disabled: self.filters_disabled.unwrap(),
            dummy_verifier: self.dummy_verifier,
            l1_batch_commit_data_generator_mode: self.l1_batch_commit_data_generator_mode,
            timestamp_asserter_address: self.timestamp_asserter_address,
            l1_server_notifier_addr: self.l1_server_notifier_addr,
        }
    }
}

/// Configuration values for the API.
/// This structure is detached from `ZkSyncConfig`, since different node types (main, external, etc.)
/// may require different configuration layouts.
/// The intention is to only keep the actually used information here.
#[derive(Debug, Clone)]
pub struct InternalApiConfig {
    /// Chain ID of the L1 network. Note, that it may be different from the chain id of the settlement layer.
    pub l1_chain_id: L1ChainId,
    pub l2_chain_id: L2ChainId,
    pub max_tx_size: usize,
    pub estimate_gas_scale_factor: f64,
    pub estimate_gas_acceptable_overestimation: u32,
    pub estimate_gas_optimize_search: bool,
    pub bridge_addresses: api::BridgeAddresses,
    pub l1_bytecodes_supplier_addr: Option<Address>,
    pub l1_wrapped_base_token_store: Option<Address>,
    pub l1_bridgehub_proxy_addr: Option<Address>,
    pub l1_state_transition_proxy_addr: Option<Address>,
    pub l1_transparent_proxy_admin_addr: Option<Address>,
    pub l1_diamond_proxy_addr: Address,
    pub l2_testnet_paymaster_addr: Option<Address>,
    pub req_entities_limit: usize,
    pub fee_history_limit: u64,
    pub base_token_address: Option<Address>,
    pub filters_disabled: bool,
    pub dummy_verifier: bool,
    pub l1_batch_commit_data_generator_mode: L1BatchCommitmentMode,
    pub timestamp_asserter_address: Option<Address>,
    pub l1_server_notifier_addr: Option<Address>,
}

impl InternalApiConfig {
    pub fn new(
        web3_config: &Web3JsonRpcConfig,
        contracts_config: &ChainSpecificContracts,
        l1_ecosystem_contracts: &L1SpecificContracts,
        genesis_config: &GenesisConfig,
    ) -> Self {
        Self {
            l1_chain_id: genesis_config.l1_chain_id,
            l2_chain_id: genesis_config.l2_chain_id,
            max_tx_size: web3_config.max_tx_size,
            estimate_gas_scale_factor: web3_config.estimate_gas_scale_factor,
            estimate_gas_acceptable_overestimation: web3_config
                .estimate_gas_acceptable_overestimation,
            estimate_gas_optimize_search: web3_config.estimate_gas_optimize_search,
            bridge_addresses: api::BridgeAddresses {
                l1_erc20_default_bridge: l1_ecosystem_contracts.erc_20_bridge,
                l2_erc20_default_bridge: contracts_config.l2_contracts.erc20_default_bridge,
                l1_shared_default_bridge: l1_ecosystem_contracts.shared_bridge,
                l2_shared_default_bridge: contracts_config.l2_contracts.shared_bridge_addr,
                // WETH bridge is not available
                l1_weth_bridge: None,
                l2_weth_bridge: None,
                l2_legacy_shared_bridge: contracts_config.l2_contracts.legacy_shared_bridge_addr,
            },
            l1_bridgehub_proxy_addr: l1_ecosystem_contracts.bridge_hub,
            l1_state_transition_proxy_addr: contracts_config
                .ecosystem_contracts
                .state_transition_proxy_addr,
            l1_transparent_proxy_admin_addr: None,
            l1_bytecodes_supplier_addr: l1_ecosystem_contracts.bytecodes_supplier_addr,
            l1_wrapped_base_token_store: l1_ecosystem_contracts.wrapped_base_token_store,
            l1_diamond_proxy_addr: l1_ecosystem_contracts.l1_diamond_proxy,
            l2_testnet_paymaster_addr: contracts_config.l2_contracts.testnet_paymaster_addr,
            req_entities_limit: web3_config.req_entities_limit(),
            fee_history_limit: web3_config.fee_history_limit(),
            base_token_address: Some(
                l1_ecosystem_contracts
                    .base_token_address
                    .unwrap_or(ETHEREUM_ADDRESS),
            ),
            filters_disabled: web3_config.filters_disabled,
            dummy_verifier: genesis_config.dummy_verifier,
            l1_batch_commit_data_generator_mode: genesis_config.l1_batch_commit_data_generator_mode,
            timestamp_asserter_address: contracts_config.l2_contracts.timestamp_asserter_addr,
            l1_server_notifier_addr: contracts_config.ecosystem_contracts.server_notifier_addr,
        }
    }
}

/// Thread-safe updatable information about the last sealed L2 block number.
///
/// The information may be temporarily outdated and thus should only be used where this is OK
/// (e.g., for metrics reporting). The value is updated by [`Self::diff()`] and [`Self::diff_with_block_args()`].
#[derive(Debug, Clone, Default)]
pub struct SealedL2BlockNumber(Arc<AtomicU32>);

impl SealedL2BlockNumber {
    /// Potentially updates the last sealed L2 block number by comparing it to the provided
    /// sealed L2 block number (not necessarily the last one).
    ///
    /// Returns the last sealed L2 block number after the update.
    pub fn update(&self, maybe_newer_l2_block_number: L2BlockNumber) -> L2BlockNumber {
        let prev_value = self
            .0
            .fetch_max(maybe_newer_l2_block_number.0, Ordering::Relaxed);
        L2BlockNumber(prev_value).max(maybe_newer_l2_block_number)
    }

    pub fn diff(&self, l2_block_number: L2BlockNumber) -> u32 {
        let sealed_l2_block_number = self.update(l2_block_number);
        sealed_l2_block_number.0.saturating_sub(l2_block_number.0)
    }

    /// Returns the difference between the latest L2 block number and the resolved L2 block number
    /// from `block_args`.
    pub(crate) fn diff_with_block_args(&self, block_args: &BlockArgs) -> u32 {
        // We compute the difference in any case, since it may update the stored value.
        let diff = self.diff(block_args.resolved_block_number());

        if block_args.resolves_to_latest_sealed_l2_block() {
            0 // Overwrite potentially inaccurate value
        } else {
            diff
        }
    }
}

#[derive(Debug, Clone)]
pub struct BridgeAddressesHandle(Arc<RwLock<api::BridgeAddresses>>);

impl BridgeAddressesHandle {
    pub fn new(bridge_addresses: api::BridgeAddresses) -> Self {
        Self(Arc::new(RwLock::new(bridge_addresses)))
    }

    pub async fn update(&self, bridge_addresses: api::BridgeAddresses) {
        *self.0.write().await = bridge_addresses;
    }

    pub async fn update_l1_shared_bridge(&self, l1_shared_bridge: Address) {
        self.0.write().await.l1_shared_default_bridge = Some(l1_shared_bridge);
    }

    pub async fn update_l2_bridges(&self, l2_shared_bridge: Address) {
        self.0.write().await.l2_shared_default_bridge = Some(l2_shared_bridge);
        self.0.write().await.l2_erc20_default_bridge = Some(l2_shared_bridge);
    }

    pub async fn read(&self) -> api::BridgeAddresses {
        self.0.read().await.clone()
    }
}

/// Holder for the data required for the API to be functional.
#[derive(Debug, Clone)]
pub(crate) struct RpcState {
    pub(super) current_method: Arc<MethodTracer>,
    pub(super) installed_filters: Option<Arc<Mutex<Filters>>>,
    pub(super) connection_pool: ConnectionPool<Core>,
    pub(super) tree_api: Option<Arc<dyn TreeApiClient>>,
    pub(super) tx_sender: TxSender,
    pub(super) sync_state: Option<SyncState>,
    pub(super) api_config: InternalApiConfig,
    /// Number of the first locally available L2 block / L1 batch. May differ from 0 if the node state was recovered
    /// from a snapshot.
    pub(super) start_info: BlockStartInfo,
    pub(super) mempool_cache: Option<MempoolCache>,
    pub(super) last_sealed_l2_block: SealedL2BlockNumber,
    pub(super) bridge_addresses_handle: BridgeAddressesHandle,
    pub(super) l2_l1_log_proof_handler: Option<Box<DynClient<L2>>>,
}

impl RpcState {
    pub fn parse_transaction_bytes(
        &self,
        bytes: &[u8],
        block_args: &BlockArgs,
    ) -> Result<(L2Tx, H256), Web3Error> {
        let chain_id = self.api_config.l2_chain_id;
        let (tx_request, hash) = api::TransactionRequest::from_bytes(bytes, chain_id)?;
        Ok((
            L2Tx::from_request(
                tx_request,
                self.api_config.max_tx_size,
                block_args.use_evm_emulator(),
            )?,
            hash,
        ))
    }

    pub fn u64_to_block_number(n: U64) -> L2BlockNumber {
        if n.as_u64() > u32::MAX as u64 {
            L2BlockNumber(u32::MAX)
        } else {
            L2BlockNumber(n.as_u32())
        }
    }

    pub(crate) fn tx_sink(&self) -> &dyn TxSink {
        self.tx_sender.0.tx_sink.as_ref()
    }

    /// Acquires a DB connection mapping possible errors.
    // `track_caller` is necessary to correctly record call location. `async fn`s don't support it yet,
    // thus manual de-sugaring.
    #[track_caller]
    pub(crate) fn acquire_connection(
        &self,
    ) -> impl Future<Output = Result<Connection<'static, Core>, Web3Error>> + '_ {
        self.connection_pool
            .connection_tagged("api")
            .map_err(|err| err.generalize().into())
    }

    /// Resolves the specified block ID to a block number, which is guaranteed to be present in the node storage.
    pub(crate) async fn resolve_block(
        &self,
        connection: &mut Connection<'_, Core>,
        block: api::BlockId,
    ) -> Result<L2BlockNumber, Web3Error> {
        self.start_info.ensure_not_pruned(block, connection).await?;
        connection
            .blocks_web3_dal()
            .resolve_block_id(block)
            .await
            .map_err(DalError::generalize)?
            .ok_or(Web3Error::NoBlock)
    }

    /// Resolves the specified block ID to a block number, which is **not** guaranteed to be present in the node storage.
    /// Returns `None` if the block is known to not be present in the storage (e.g., it's a "finalized" block ID and no blocks
    /// were finalized yet).
    ///
    /// This method is more efficient than [`Self::resolve_block()`] (it doesn't query the storage if block ID maps to a known
    /// block number), but is more difficult to reason about. You should use it only if the block number consumer correctly handles
    /// non-existing blocks.
    pub(crate) async fn resolve_block_unchecked(
        &self,
        connection: &mut Connection<'_, Core>,
        block: api::BlockId,
    ) -> Result<Option<L2BlockNumber>, Web3Error> {
        self.start_info.ensure_not_pruned(block, connection).await?;
        match block {
            api::BlockId::Number(api::BlockNumber::Number(number)) => {
                Ok(u32::try_from(number).ok().map(L2BlockNumber))
            }
            api::BlockId::Number(api::BlockNumber::Earliest) => Ok(Some(L2BlockNumber(0))),
            _ => Ok(connection
                .blocks_web3_dal()
                .resolve_block_id(block)
                .await
                .map_err(DalError::generalize)?),
        }
    }

    pub(crate) async fn resolve_block_args(
        &self,
        connection: &mut Connection<'_, Core>,
        block: api::BlockId,
    ) -> Result<BlockArgs, Web3Error> {
        BlockArgs::new(connection, block, &self.start_info)
            .await
            .map_err(|err| match err {
                BlockArgsError::Pruned(number) => Web3Error::PrunedBlock(number),
                BlockArgsError::Missing => Web3Error::NoBlock,
                BlockArgsError::Database(err) => Web3Error::InternalError(err),
            })
    }

    pub async fn resolve_filter_block_number(
        &self,
        block_number: Option<api::BlockNumber>,
    ) -> Result<L2BlockNumber, Web3Error> {
        if let Some(api::BlockNumber::Number(number)) = block_number {
            return Ok(Self::u64_to_block_number(number));
        }

        let block_number = block_number.unwrap_or(api::BlockNumber::Latest);
        let block_id = api::BlockId::Number(block_number);
        let mut conn = self.acquire_connection().await?;
        Ok(self.resolve_block(&mut conn, block_id).await.unwrap())
        // ^ `unwrap()` is safe: `resolve_block_id(api::BlockId::Number(_))` can only return `None`
        // if called with an explicit number, and we've handled this case earlier.
    }

    pub async fn resolve_filter_block_range(
        &self,
        filter: &Filter,
    ) -> Result<(L2BlockNumber, L2BlockNumber), Web3Error> {
        let from_block = self.resolve_filter_block_number(filter.from_block).await?;
        let to_block = self.resolve_filter_block_number(filter.to_block).await?;
        Ok((from_block, to_block))
    }

    /// If filter has `block_hash` then it resolves block number by hash and sets it to `from_block` and `to_block`.
    pub async fn resolve_filter_block_hash(&self, filter: &mut Filter) -> Result<(), Web3Error> {
        match (filter.block_hash, filter.from_block, filter.to_block) {
            (Some(block_hash), None, None) => {
                let mut storage = self.acquire_connection().await?;
                let block_number = self
                    .resolve_block(&mut storage, api::BlockId::Hash(block_hash))
                    .await?;
                filter.from_block = Some(api::BlockNumber::Number(block_number.0.into()));
                filter.to_block = Some(api::BlockNumber::Number(block_number.0.into()));
                Ok(())
            }
            (Some(_), _, _) => Err(Web3Error::InvalidFilterBlockHash),
            (None, _, _) => Ok(()),
        }
    }

    /// Returns initial `from_block` for filter.
    /// It is equal to max(filter.from_block, PENDING_BLOCK).
    pub async fn get_filter_from_block(&self, filter: &Filter) -> Result<L2BlockNumber, Web3Error> {
        let mut connection = self.acquire_connection().await?;
        let pending_block = self
            .resolve_block_unchecked(
                &mut connection,
                api::BlockId::Number(api::BlockNumber::Pending),
            )
            .await?
            .context("Pending block number shouldn't be None")?;
        let block_number = match filter.from_block {
            Some(api::BlockNumber::Number(number)) => {
                let block_number = Self::u64_to_block_number(number);
                block_number.max(pending_block)
            }
            _ => pending_block,
        };
        Ok(block_number)
    }

    pub(crate) async fn set_nonce_for_call_request(
        &self,
        call_request: &mut CallRequest,
    ) -> Result<(), Web3Error> {
        if call_request.nonce.is_some() {
            return Ok(());
        }
        let mut connection = self.acquire_connection().await?;
        let latest_block_id = api::BlockId::Number(api::BlockNumber::Latest);
        let latest_block_number = self.resolve_block(&mut connection, latest_block_id).await?;

        let from = call_request.from.unwrap_or_default();
        let address_historical_nonce = connection
            .storage_web3_dal()
            .get_address_historical_nonce(from, latest_block_number)
            .await
            .map_err(DalError::generalize)?;
        call_request.nonce = Some(address_historical_nonce);
        Ok(())
    }
}

/// Contains mapping from index to `Filter`s with optional location.
#[derive(Debug)]
pub(crate) struct Filters(LruCache<U256, InstalledFilter>);

#[derive(Debug)]
struct InstalledFilter {
    pub filter: TypedFilter,
    _guard: GaugeGuard,
    created_at: Instant,
    last_request: Instant,
    request_count: usize,
}

impl InstalledFilter {
    pub fn new(filter: TypedFilter) -> Self {
        let guard = FILTER_METRICS.filter_count[&FilterType::from(&filter)].inc_guard(1);
        Self {
            filter,
            _guard: guard,
            created_at: Instant::now(),
            last_request: Instant::now(),
            request_count: 0,
        }
    }

    pub fn update_stats(&mut self) {
        let previous_request_timestamp = self.last_request;
        let now = Instant::now();

        self.last_request = now;
        self.request_count += 1;

        let filter_type = FilterType::from(&self.filter);
        FILTER_METRICS.request_frequency[&filter_type].observe(now - previous_request_timestamp);
    }
}

impl Drop for InstalledFilter {
    fn drop(&mut self) {
        let filter_type = FilterType::from(&self.filter);

        FILTER_METRICS.request_count[&filter_type].observe(self.request_count);
        FILTER_METRICS.filter_lifetime[&filter_type].observe(self.created_at.elapsed());
    }
}

impl Filters {
    /// Instantiates `Filters` with given max capacity.
    pub fn new(max_cap: Option<usize>) -> Self {
        let state = match max_cap {
            Some(max_cap) => {
                LruCache::new(max_cap.try_into().expect("Filter capacity should not be 0"))
            }
            None => LruCache::unbounded(),
        };
        Self(state)
    }

    /// Adds filter to the state and returns its key.
    pub fn add(&mut self, filter: TypedFilter) -> U256 {
        let idx = loop {
            let val = H256::random().to_fixed_bytes().into();
            if !self.0.contains(&val) {
                break val;
            }
        };

        self.0.push(idx, InstalledFilter::new(filter));

        idx
    }

    /// Retrieves filter from the state.
    pub fn get_and_update_stats(&mut self, index: U256) -> Option<TypedFilter> {
        let installed_filter = self.0.get_mut(&index)?;

        installed_filter.update_stats();

        Some(installed_filter.filter.clone())
    }

    /// Updates filter in the state.
    pub fn update(&mut self, index: U256, new_filter: TypedFilter) {
        if let Some(installed_filter) = self.0.get_mut(&index) {
            installed_filter.filter = new_filter;
        }
    }

    /// Removes filter from the map.
    pub fn remove(&mut self, index: U256) -> bool {
        self.0.pop(&index).is_some()
    }
}

#[cfg(test)]
mod tests {
    use chrono::NaiveDateTime;

    #[test]
    fn test_filters_functionality() {
        use super::*;

        let mut filters = Filters::new(Some(2));

        let filter1 = TypedFilter::Events(Filter::default(), L2BlockNumber::default());
        let filter2 = TypedFilter::Blocks(L2BlockNumber::default());
        let filter3 = TypedFilter::PendingTransactions(NaiveDateTime::default());

        let idx1 = filters.add(filter1.clone());
        let idx2 = filters.add(filter2);
        let idx3 = filters.add(filter3);

        assert_eq!(filters.0.len(), 2);
        assert!(!filters.0.contains(&idx1));
        assert!(filters.0.contains(&idx2));
        assert!(filters.0.contains(&idx3));

        filters.get_and_update_stats(idx2);

        let idx1 = filters.add(filter1);
        assert_eq!(filters.0.len(), 2);
        assert!(filters.0.contains(&idx1));
        assert!(filters.0.contains(&idx2));
        assert!(!filters.0.contains(&idx3));

        filters.remove(idx1);

        assert_eq!(filters.0.len(), 1);
        assert!(!filters.0.contains(&idx1));
        assert!(filters.0.contains(&idx2));
        assert!(!filters.0.contains(&idx3));
    }
}
