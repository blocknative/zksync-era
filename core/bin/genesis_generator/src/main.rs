//! Each protocol upgrade required to update genesis config values.
//! This tool generates the new correct genesis file that could be used for the new chain
//! Please note, this tool update only yaml file, if you still use env based configuration,
//! update env values correspondingly

use std::{
    fs,
    io::{BufReader, BufWriter},
    path::Path,
};

use anyhow::Context as _;
use clap::Parser;
use zksync_config::{
    configs::{genesis::PersistedGenesisConfig, DatabaseSecrets},
    full_config_schema,
    sources::ConfigFilePaths,
    ConfigRepository, GenesisConfig, ParseResultExt,
};
use zksync_contracts::BaseSystemContracts;
use zksync_dal::{ConnectionPool, Core, CoreDal};
use zksync_node_genesis::{insert_genesis_batch, GenesisParams};
use zksync_types::{
    protocol_version::ProtocolSemanticVersion, url::SensitiveUrl, ProtocolVersionId,
};

const DEFAULT_GENESIS_FILE_PATH: &str = "./etc/env/file_based/genesis.yaml";

#[derive(Debug, Parser)]
#[command(author = "Matter Labs", version, about = "Genesis config generator", long_about = None)]
struct Cli {
    #[arg(long)]
    config_path: Option<std::path::PathBuf>,
    #[arg(long, default_value = "false")]
    check: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let opt = Cli::parse();

    let config_file_paths = ConfigFilePaths {
        secrets: opt.config_path,
        ..ConfigFilePaths::default()
    };
    let config_sources =
        tokio::task::spawn_blocking(|| config_file_paths.into_config_sources("")).await??;

    let schema = full_config_schema(false);
    let repo = ConfigRepository::new(&schema).with_all(config_sources);
    let database_secrets: DatabaseSecrets = repo.single()?.parse().log_all_errors()?;

    let original_genesis: GenesisConfig =
        read_genesis_config(DEFAULT_GENESIS_FILE_PATH.as_ref()).await?;
    let db_url = database_secrets.master_url()?;
    let new_genesis = generate_new_config(db_url, original_genesis.clone()).await?;
    if opt.check {
        assert_eq!(original_genesis, new_genesis);
        println!("Genesis config is up to date");
        return Ok(());
    }
    write_genesis_config(DEFAULT_GENESIS_FILE_PATH.as_ref(), new_genesis).await?;
    println!("Genesis successfully generated");
    Ok(())
}

async fn read_genesis_config(path: &'static Path) -> anyhow::Result<GenesisConfig> {
    tokio::task::spawn_blocking(move || {
        let file = fs::File::open(path)
            .with_context(|| format!("failed opening genesis config file at {:?}", path))?;
        let config: PersistedGenesisConfig = serde_yaml::from_reader(BufReader::new(file))
            .context("failed deserializing genesis config")?;
        config.try_into().context("malformed genesis config")
    })
    .await?
}

async fn write_genesis_config(path: &'static Path, config: GenesisConfig) -> anyhow::Result<()> {
    let config =
        PersistedGenesisConfig::try_from(config).context("genesis config is incomplete")?;
    tokio::task::spawn_blocking(move || {
        let file = fs::File::create(path)
            .with_context(|| format!("failed creating genesis config file at {:?}", path))?;
        serde_yaml::to_writer(BufWriter::new(file), &config)
            .context("failed serializing config to YAML")
    })
    .await?
}

async fn generate_new_config(
    db_url: SensitiveUrl,
    genesis_config: GenesisConfig,
) -> anyhow::Result<GenesisConfig> {
    let pool = ConnectionPool::<Core>::singleton(db_url)
        .build()
        .await
        .context("failed to build connection_pool")?;

    let mut storage = pool.connection().await.context("connection()")?;
    let mut transaction = storage.start_transaction().await?;

    if !transaction.blocks_dal().is_genesis_needed().await? {
        anyhow::bail!("Please cleanup database for regenerating genesis")
    }

    let base_system_contracts = BaseSystemContracts::load_from_disk().hashes();
    let mut updated_genesis = GenesisConfig {
        protocol_version: Some(ProtocolSemanticVersion {
            minor: ProtocolVersionId::latest(),
            patch: 0.into(), // genesis generator proposes some new valid config, so patch 0 works here.
        }),
        genesis_root_hash: None,
        rollup_last_leaf_index: None,
        genesis_commitment: None,
        bootloader_hash: Some(base_system_contracts.bootloader),
        default_aa_hash: Some(base_system_contracts.default_aa),
        evm_emulator_hash: base_system_contracts.evm_emulator,
        ..genesis_config
    };

    // This tool doesn't really insert the batch. It doesn't commit the transaction,
    // so the database is clean after using the tool
    let params = GenesisParams::load_genesis_params(updated_genesis.clone())?;
    let batch_params = insert_genesis_batch(&mut transaction, &params).await?;

    updated_genesis.genesis_commitment = Some(batch_params.commitment);
    updated_genesis.genesis_root_hash = Some(batch_params.root_hash);
    updated_genesis.rollup_last_leaf_index = Some(batch_params.rollup_last_leaf_index);

    Ok(updated_genesis)
}
