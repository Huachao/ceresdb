// Copyright 2022 CeresDB Project Authors. Licensed under Apache-2.0.

//! Setup the analytic engine

use std::{path::Path, pin::Pin, sync::Arc};

use async_trait::async_trait;
use common_types::table::DEFAULT_SHARD_ID;
use common_util::define_result;
use futures::Future;
use message_queue::kafka::kafka_impl::KafkaImpl;
use object_store::{
    aliyun::AliyunOSS, cache::CachedStore, mem_cache::MemCacheStore, LocalFileSystem,
    ObjectStoreRef,
};
use snafu::{Backtrace, ResultExt, Snafu};
use table_engine::engine::{EngineRuntimes, TableEngineRef};
use table_kv::{memory::MemoryImpl, obkv::ObkvImpl, TableKv};
use wal::{
    manager::{self, RegionId, WalManagerRef},
    message_queue_impl::wal::MessageQueueImpl,
    rocks_impl::manager::Builder as WalBuilder,
    table_kv_impl::{wal::WalNamespaceImpl, WalRuntimes},
};

use crate::{
    context::OpenContext,
    engine::TableEngineImpl,
    instance::{Instance, InstanceRef},
    meta::{
        details::{ManifestImpl, Options as ManifestOptions},
        ManifestRef,
    },
    sst::{
        factory::FactoryImpl,
        meta_cache::{MetaCache, MetaCacheRef},
    },
    storage_options::{ObjectStoreOptions, StorageOptions},
    Config, ObkvWalConfig, WalStorageConfig,
};

#[derive(Debug, Snafu)]
pub enum Error {
    #[snafu(display("Failed to open engine instance, err:{}", source))]
    OpenInstance {
        source: crate::instance::engine::Error,
    },

    #[snafu(display("Failed to open wal, err:{}", source))]
    OpenWal { source: manager::error::Error },

    #[snafu(display(
        "Failed to open with the invalid config, msg:{}.\nBacktrace:\n{}",
        msg,
        backtrace
    ))]
    InvalidWalConfig { msg: String, backtrace: Backtrace },

    #[snafu(display("Failed to open wal for manifest, err:{}", source))]
    OpenManifestWal { source: manager::error::Error },

    #[snafu(display("Failed to open manifest, err:{}", source))]
    OpenManifest { source: crate::meta::details::Error },

    #[snafu(display("Failed to open obkv, err:{}", source))]
    OpenObkv { source: table_kv::obkv::Error },

    #[snafu(display("Failed to execute in runtime, err:{}", source))]
    RuntimeExec { source: common_util::runtime::Error },

    #[snafu(display("Failed to open object store, err:{}", source))]
    OpenObjectStore {
        source: object_store::ObjectStoreError,
    },

    #[snafu(display("Failed to create dir for {}, err:{}", path, source))]
    CreateDir {
        path: String,
        source: std::io::Error,
    },

    #[snafu(display("Failed to open kafka, err:{}", source))]
    OpenKafka {
        source: message_queue::kafka::kafka_impl::Error,
    },
}

define_result!(Error);

const WAL_DIR_NAME: &str = "wal";
const MANIFEST_DIR_NAME: &str = "manifest";
const STORE_DIR_NAME: &str = "store";

/// Analytic engine builder.
#[async_trait]
pub trait EngineBuilder: Send + Sync + Default {
    /// Build the analytic engine from `config` and `engine_runtimes`.
    async fn build(
        &self,
        config: Config,
        engine_runtimes: Arc<EngineRuntimes>,
    ) -> Result<TableEngineRef> {
        let (wal, manifest) = self
            .open_wal_and_manifest(config.clone(), engine_runtimes.clone())
            .await?;
        let store = open_storage(config.storage.clone()).await?;
        let instance = open_instance(config.clone(), engine_runtimes, wal, manifest, store).await?;
        Ok(Arc::new(TableEngineImpl::new(instance)))
    }

    async fn open_wal_and_manifest(
        &self,
        config: Config,
        engine_runtimes: Arc<EngineRuntimes>,
    ) -> Result<(WalManagerRef, ManifestRef)>;
}

/// [RocksEngine] builder.
#[derive(Default)]
pub struct RocksDBWalEngineBuilder;

#[async_trait]
impl EngineBuilder for RocksDBWalEngineBuilder {
    async fn open_wal_and_manifest(
        &self,
        config: Config,
        engine_runtimes: Arc<EngineRuntimes>,
    ) -> Result<(WalManagerRef, ManifestRef)> {
        match &config.wal_storage {
            WalStorageConfig::RocksDB => {}
            _ => {
                return InvalidWalConfig {
                    msg: format!(
                        "invalid wal storage config while opening rocksDB wal, config:{:?}",
                        config.wal_storage
                    ),
                }
                .fail();
            }
        }

        let default_region_id = DEFAULT_SHARD_ID as RegionId;

        let write_runtime = engine_runtimes.write_runtime.clone();
        let data_path = Path::new(&config.wal_path);
        let wal_path = data_path.join(WAL_DIR_NAME);
        let wal_manager = WalBuilder::with_default_rocksdb_config(
            wal_path,
            write_runtime.clone(),
            default_region_id,
        )
        .build()
        .context(OpenWal)?;

        let manifest_path = data_path.join(MANIFEST_DIR_NAME);
        let manifest_wal = WalBuilder::with_default_rocksdb_config(
            manifest_path,
            write_runtime,
            default_region_id,
        )
        .build()
        .context(OpenManifestWal)?;

        let manifest = ManifestImpl::open(Arc::new(manifest_wal), config.manifest.clone())
            .await
            .context(OpenManifest)?;

        Ok((Arc::new(wal_manager), Arc::new(manifest)))
    }
}

/// [ReplicatedEngine] builder.
#[derive(Default)]
pub struct ObkvWalEngineBuilder;

#[async_trait]
impl EngineBuilder for ObkvWalEngineBuilder {
    async fn open_wal_and_manifest(
        &self,
        config: Config,
        engine_runtimes: Arc<EngineRuntimes>,
    ) -> Result<(WalManagerRef, ManifestRef)> {
        let obkv_wal_config = match &config.wal_storage {
            WalStorageConfig::Obkv(config) => config.clone(),
            _ => {
                return InvalidWalConfig {
                    msg: format!(
                        "invalid wal storage config while opening obkv wal, config:{:?}",
                        config.wal_storage
                    ),
                }
                .fail();
            }
        };

        // Notice the creation of obkv client may block current thread.
        let obkv_config = obkv_wal_config.obkv.clone();
        let obkv = engine_runtimes
            .write_runtime
            .spawn_blocking(move || ObkvImpl::new(obkv_config).context(OpenObkv))
            .await
            .context(RuntimeExec)??;

        open_wal_and_manifest_with_table_kv(
            *obkv_wal_config,
            config.manifest.clone(),
            engine_runtimes,
            obkv,
        )
        .await
    }
}

/// [MemWalEngine] builder.
///
/// All engine built by this builder share same [MemoryImpl] instance, so the
/// data wrote by the engine still remains after the engine dropped.
#[derive(Default)]
pub struct MemWalEngineBuilder {
    table_kv: MemoryImpl,
}

#[async_trait]
impl EngineBuilder for MemWalEngineBuilder {
    async fn open_wal_and_manifest(
        &self,
        config: Config,
        engine_runtimes: Arc<EngineRuntimes>,
    ) -> Result<(WalManagerRef, ManifestRef)> {
        let obkv_wal_config = match &config.wal_storage {
            WalStorageConfig::Obkv(config) => config.clone(),
            _ => {
                return InvalidWalConfig {
                    msg: format!(
                        "invalid wal storage config while opening memory wal, config:{:?}",
                        config.wal_storage
                    ),
                }
                .fail();
            }
        };

        open_wal_and_manifest_with_table_kv(
            *obkv_wal_config,
            config.manifest.clone(),
            engine_runtimes,
            self.table_kv.clone(),
        )
        .await
    }
}

#[derive(Default)]
pub struct KafkaWalEngineBuilder;

#[async_trait]
impl EngineBuilder for KafkaWalEngineBuilder {
    async fn open_wal_and_manifest(
        &self,
        config: Config,
        engine_runtimes: Arc<EngineRuntimes>,
    ) -> Result<(WalManagerRef, ManifestRef)> {
        let kafka_wal_config = match &config.wal_storage {
            WalStorageConfig::Kafka(config) => config.clone(),
            _ => {
                return InvalidWalConfig {
                    msg: format!(
                        "invalid wal storage config while opening kafka wal, config:{:?}",
                        config
                    ),
                }
                .fail();
            }
        };

        let bg_runtime = &engine_runtimes.bg_runtime;

        let kafka = KafkaImpl::new(kafka_wal_config.kafka_config.clone())
            .await
            .context(OpenKafka)?;
        let wal_manager = MessageQueueImpl::new(
            WAL_DIR_NAME.to_string(),
            kafka,
            bg_runtime.clone(),
            kafka_wal_config.wal_config.clone(),
        );

        let kafka_for_manifest = KafkaImpl::new(kafka_wal_config.kafka_config.clone())
            .await
            .context(OpenKafka)?;
        let manifest_wal = MessageQueueImpl::new(
            MANIFEST_DIR_NAME.to_string(),
            kafka_for_manifest,
            bg_runtime.clone(),
            kafka_wal_config.wal_config,
        );

        let manifest = ManifestImpl::open(Arc::new(manifest_wal), config.manifest)
            .await
            .context(OpenManifest)?;

        Ok((Arc::new(wal_manager), Arc::new(manifest)))
    }
}

async fn open_wal_and_manifest_with_table_kv<T: TableKv>(
    config: ObkvWalConfig,
    manifest_opts: ManifestOptions,
    engine_runtimes: Arc<EngineRuntimes>,
    table_kv: T,
) -> Result<(WalManagerRef, ManifestRef)> {
    let runtimes = WalRuntimes {
        read_runtime: engine_runtimes.read_runtime.clone(),
        write_runtime: engine_runtimes.write_runtime.clone(),
        bg_runtime: engine_runtimes.bg_runtime.clone(),
    };

    let wal_manager = WalNamespaceImpl::open(
        table_kv.clone(),
        runtimes.clone(),
        WAL_DIR_NAME,
        config.wal.clone(),
    )
    .await
    .context(OpenWal)?;

    let manifest_wal = WalNamespaceImpl::open(
        table_kv,
        runtimes,
        MANIFEST_DIR_NAME,
        config.manifest.clone(),
    )
    .await
    .context(OpenManifestWal)?;
    let manifest = ManifestImpl::open(Arc::new(manifest_wal), manifest_opts)
        .await
        .context(OpenManifest)?;

    Ok((Arc::new(wal_manager), Arc::new(manifest)))
}

async fn open_instance(
    config: Config,
    engine_runtimes: Arc<EngineRuntimes>,
    wal_manager: WalManagerRef,
    manifest: ManifestRef,
    store: ObjectStoreRef,
) -> Result<InstanceRef> {
    let meta_cache: Option<MetaCacheRef> = config
        .sst_meta_cache_cap
        .map(|cap| Arc::new(MetaCache::new(cap)));

    let open_ctx = OpenContext {
        config,
        runtimes: engine_runtimes,
        meta_cache,
    };

    let instance = Instance::open(
        open_ctx,
        manifest,
        wal_manager,
        store,
        Arc::new(FactoryImpl::default()),
    )
    .await
    .context(OpenInstance)?;
    Ok(instance)
}

fn open_storage(
    opts: StorageOptions,
) -> Pin<Box<dyn Future<Output = Result<ObjectStoreRef>> + Send>> {
    Box::pin(async move {
        let underlying_store = match opts.object_store {
            ObjectStoreOptions::Local(local_opts) => {
                let data_path = Path::new(&local_opts.data_path);
                let sst_path = data_path.join(STORE_DIR_NAME);
                tokio::fs::create_dir_all(&sst_path)
                    .await
                    .context(CreateDir {
                        path: sst_path.to_string_lossy().into_owned(),
                    })?;
                let store = LocalFileSystem::new_with_prefix(sst_path).context(OpenObjectStore)?;
                Arc::new(store) as _
            }
            ObjectStoreOptions::Aliyun(aliyun_opts) => Arc::new(AliyunOSS::new(
                aliyun_opts.key_id,
                aliyun_opts.key_secret,
                aliyun_opts.endpoint,
                aliyun_opts.bucket,
            )) as _,
            ObjectStoreOptions::Cache(cache_opts) => {
                let local_store = open_storage(*cache_opts.local_store).await?;
                let remote_store = open_storage(*cache_opts.remote_store).await?;
                let store = CachedStore::init(local_store, remote_store, cache_opts.cache_opts)
                    .await
                    .context(OpenObjectStore)?;
                Arc::new(store) as _
            }
        };

        if opts.mem_cache_capacity.as_bytes() == 0 {
            return Ok(underlying_store);
        }

        let store = Arc::new(MemCacheStore::new(
            opts.mem_cache_partition_bits,
            opts.mem_cache_capacity.as_bytes() as usize,
            underlying_store,
        )) as _;

        Ok(store)
    })
}
