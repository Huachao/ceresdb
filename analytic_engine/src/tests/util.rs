// Copyright 2022 CeresDB Project Authors. Licensed under Apache-2.0.

//! Test utils.

use std::{collections::HashMap, future::Future, sync::Arc};

use common_types::{
    datum::Datum,
    record_batch::RecordBatch,
    row::{Row, RowGroup},
    table::DEFAULT_SHARD_ID,
    time::Timestamp,
};
use common_util::{
    config::{ReadableDuration, ReadableSize},
    runtime,
};
use futures::stream::StreamExt;
use log::info;
use table_engine::{
    engine::{
        CreateTableRequest, DropTableRequest, EngineRuntimes, OpenTableRequest,
        Result as EngineResult, TableEngineRef,
    },
    table::{
        AlterSchemaRequest, FlushRequest, GetRequest, ReadOrder, ReadRequest, Result, SchemaId,
        TableId, TableRef, WriteRequest,
    },
};
use tempfile::TempDir;

use crate::{
    setup::{EngineBuilder, MemWalEngineBuilder, RocksDBWalEngineBuilder},
    storage_options::{LocalOptions, ObjectStoreOptions, StorageOptions},
    tests::table::{self, FixedSchemaTable, RowTuple},
    Config, ObkvWalConfig, WalStorageConfig,
};

const DAY_MS: i64 = 24 * 60 * 60 * 1000;

/// Helper struct to create a null datum.
pub struct Null;

impl From<Null> for Datum {
    fn from(_data: Null) -> Datum {
        Datum::Null
    }
}

pub async fn check_read_with_order<T: EngineContext>(
    test_ctx: &TestContext<T>,
    fixed_schema_table: &FixedSchemaTable,
    msg: &str,
    table_name: &str,
    rows: &[RowTuple<'_>],
    read_order: ReadOrder,
) {
    for read_opts in table::read_opts_list() {
        info!("{}, opts:{:?}", msg, read_opts);

        let record_batches = test_ctx
            .read_table(
                table_name,
                fixed_schema_table.new_read_all_request(read_opts, read_order),
            )
            .await;

        fixed_schema_table.assert_batch_eq_to_rows(&record_batches, rows);
    }
}

pub async fn check_read<T: EngineContext>(
    test_ctx: &TestContext<T>,
    fixed_schema_table: &FixedSchemaTable,
    msg: &str,
    table_name: &str,
    rows: &[RowTuple<'_>],
) {
    check_read_with_order(
        test_ctx,
        fixed_schema_table,
        msg,
        table_name,
        rows,
        ReadOrder::None,
    )
    .await
}

pub async fn check_get<T: EngineContext>(
    test_ctx: &TestContext<T>,
    fixed_schema_table: &FixedSchemaTable,
    msg: &str,
    table_name: &str,
    rows: &[RowTuple<'_>],
) {
    for row_data in rows {
        let request = fixed_schema_table.new_get_request_from_row(*row_data);

        info!("{}, request:{:?}, row_data:{:?}", msg, request, row_data);

        let row = test_ctx.get_from_table(table_name, request).await.unwrap();

        fixed_schema_table.assert_row_eq(*row_data, row);
    }
}

pub struct TestContext<T: EngineContext> {
    pub config: Config,
    runtimes: Arc<EngineRuntimes>,
    builder: T::EngineBuilder,
    pub engine: Option<TableEngineRef>,
    pub schema_id: SchemaId,
    last_table_seq: u32,

    name_to_tables: HashMap<String, TableRef>,
}

impl<T: EngineContext> TestContext<T> {
    pub async fn open(&mut self) {
        let engine = self
            .builder
            .build(self.config.clone(), self.runtimes.clone())
            .await
            .unwrap();
        self.engine = Some(engine);
    }

    pub async fn reopen(&mut self) {
        {
            // Close all tables.
            self.name_to_tables.clear();

            // Close engine.
            let engine = self.engine.take().unwrap();
            engine.close().await.unwrap();
        }

        self.open().await;
    }

    pub async fn reopen_with_tables(&mut self, tables: &[&str]) {
        let table_infos: Vec<_> = tables
            .iter()
            .map(|name| {
                let table_id = self.name_to_tables.get(*name).unwrap().id();
                (table_id, *name)
            })
            .collect();
        {
            // Close all tables.
            self.name_to_tables.clear();

            // Close engine.
            let engine = self.engine.take().unwrap();
            engine.close().await.unwrap();
        }

        self.open().await;

        for (id, name) in table_infos {
            self.open_table(id, name).await;
        }
    }

    async fn open_table(&mut self, table_id: TableId, table_name: &str) {
        let table = self
            .engine()
            .open_table(OpenTableRequest {
                catalog_name: "ceresdb".to_string(),
                schema_name: "public".to_string(),
                schema_id: self.schema_id,
                table_name: table_name.to_string(),
                table_id,
                engine: table_engine::ANALYTIC_ENGINE_TYPE.to_string(),
                shard_id: DEFAULT_SHARD_ID,
            })
            .await
            .unwrap()
            .unwrap();

        self.name_to_tables.insert(table_name.to_string(), table);
    }

    pub async fn try_open_table(
        &mut self,
        table_id: TableId,
        table_name: &str,
    ) -> EngineResult<Option<TableRef>> {
        let table_opt = self
            .engine()
            .open_table(OpenTableRequest {
                catalog_name: "ceresdb".to_string(),
                schema_name: "public".to_string(),
                schema_id: self.schema_id,
                table_name: table_name.to_string(),
                table_id,
                engine: table_engine::ANALYTIC_ENGINE_TYPE.to_string(),
                shard_id: DEFAULT_SHARD_ID,
            })
            .await?;

        let table = match table_opt {
            Some(v) => v,
            None => return Ok(None),
        };

        self.name_to_tables
            .insert(table_name.to_string(), table.clone());

        Ok(Some(table))
    }

    pub async fn drop_table(&mut self, table_name: &str) -> bool {
        let request = DropTableRequest {
            catalog_name: "ceresdb".to_string(),
            schema_name: "public".to_string(),
            schema_id: self.schema_id,
            table_name: table_name.to_string(),
            engine: table_engine::ANALYTIC_ENGINE_TYPE.to_string(),
        };

        let ret = self.engine().drop_table(request).await.unwrap();

        self.name_to_tables.remove(table_name);

        ret
    }

    /// 3 days ago.
    pub fn start_ms(&self) -> i64 {
        Timestamp::now().as_i64() - 3 * DAY_MS
    }

    pub async fn create_fixed_schema_table(&mut self, table_name: &str) -> FixedSchemaTable {
        let fixed_schema_table = FixedSchemaTable::builder()
            .schema_id(self.schema_id)
            .table_name(table_name.to_string())
            .table_id(self.next_table_id())
            .ttl("7d".parse::<ReadableDuration>().unwrap())
            .build_fixed();

        self.create_table(fixed_schema_table.create_request().clone())
            .await;

        fixed_schema_table
    }

    async fn create_table(&mut self, create_request: CreateTableRequest) {
        let table_name = create_request.table_name.clone();
        let table = self.engine().create_table(create_request).await.unwrap();

        self.name_to_tables.insert(table_name.to_string(), table);
    }

    pub async fn write_to_table(&self, table_name: &str, row_group: RowGroup) {
        let table = self.table(table_name);

        table.write(WriteRequest { row_group }).await.unwrap();
    }

    pub async fn read_table(
        &self,
        table_name: &str,
        read_request: ReadRequest,
    ) -> Vec<RecordBatch> {
        let table = self.table(table_name);

        let mut stream = table.read(read_request).await.unwrap();
        let mut record_batches = Vec::new();
        while let Some(batch) = stream.next().await {
            let batch = batch.unwrap();

            record_batches.push(batch);
        }

        record_batches
    }

    pub async fn partitioned_read_table(
        &self,
        table_name: &str,
        read_request: ReadRequest,
    ) -> Vec<RecordBatch> {
        let table = self.table(table_name);

        let streams = table.partitioned_read(read_request).await.unwrap();
        let mut record_batches = Vec::new();

        for mut stream in streams.streams {
            while let Some(batch) = stream.next().await {
                let batch = batch.unwrap();

                record_batches.push(batch);
            }
        }

        record_batches
    }

    pub async fn get_from_table(&self, table_name: &str, request: GetRequest) -> Option<Row> {
        let table = self.table(table_name);

        table.get(request).await.unwrap()
    }

    pub async fn flush_table(&self, table_name: &str) {
        let table = self.table(table_name);

        table.flush(FlushRequest::default()).await.unwrap();
    }

    pub async fn flush_table_with_request(&self, table_name: &str, request: FlushRequest) {
        let table = self.table(table_name);

        table.flush(request).await.unwrap();
    }

    pub async fn compact_table(&self, table_name: &str) {
        let table = self.table(table_name);

        table.compact().await.unwrap();
    }

    pub async fn try_alter_schema(
        &self,
        table_name: &str,
        request: AlterSchemaRequest,
    ) -> Result<usize> {
        let table = self.table(table_name);

        table.alter_schema(request).await
    }

    pub async fn try_alter_options(
        &self,
        table_name: &str,
        opts: HashMap<String, String>,
    ) -> Result<usize> {
        let table = self.table(table_name);

        table.alter_options(opts).await
    }

    pub fn table(&self, table_name: &str) -> TableRef {
        self.name_to_tables.get(table_name).cloned().unwrap()
    }

    #[inline]
    pub fn engine(&self) -> &TableEngineRef {
        self.engine.as_ref().unwrap()
    }

    fn next_table_id(&mut self) -> TableId {
        self.last_table_seq += 1;
        table::new_table_id(2, self.last_table_seq)
    }
}

impl<T: EngineContext> TestContext<T> {
    pub fn clone_engine(&self) -> TableEngineRef {
        self.engine.clone().unwrap()
    }
}

pub struct TestEnv {
    _dir: TempDir,
    pub config: Config,
    pub runtimes: Arc<EngineRuntimes>,
}

impl TestEnv {
    pub fn builder() -> Builder {
        Builder::default()
    }

    pub fn new_context<T: EngineContext>(&self, engine_context: T) -> TestContext<T> {
        TestContext {
            config: engine_context.config(),
            runtimes: self.runtimes.clone(),
            builder: engine_context.engine_builder(),
            engine: None,
            schema_id: SchemaId::from_u32(100),
            last_table_seq: 1,
            name_to_tables: HashMap::new(),
        }
    }

    pub fn block_on<F: Future>(&self, future: F) -> F::Output {
        self.runtimes.bg_runtime.block_on(future)
    }
}

pub struct Builder {
    num_workers: usize,
}

impl Builder {
    pub fn build(self) -> TestEnv {
        let dir = tempfile::tempdir().unwrap();

        let config = Config {
            storage: StorageOptions {
                mem_cache_capacity: ReadableSize::mb(0),
                mem_cache_partition_bits: 0,
                object_store: ObjectStoreOptions::Local(LocalOptions {
                    data_path: dir.path().to_str().unwrap().to_string(),
                }),
            },
            wal_path: dir.path().to_str().unwrap().to_string(),
            ..Default::default()
        };

        let runtime = Arc::new(
            runtime::Builder::default()
                .worker_threads(self.num_workers)
                .enable_all()
                .build()
                .unwrap(),
        );

        TestEnv {
            _dir: dir,
            config,
            runtimes: Arc::new(EngineRuntimes {
                read_runtime: runtime.clone(),
                write_runtime: runtime.clone(),
                meta_runtime: runtime.clone(),
                bg_runtime: runtime,
            }),
        }
    }
}

impl Default for Builder {
    fn default() -> Self {
        Self { num_workers: 2 }
    }
}

pub trait EngineContext: Clone {
    type EngineBuilder: EngineBuilder;

    fn engine_builder(&self) -> Self::EngineBuilder;
    fn config(&self) -> Config;
}

pub struct RocksDBEngineContext {
    config: Config,
}

impl Default for RocksDBEngineContext {
    fn default() -> Self {
        let dir = tempfile::tempdir().unwrap();

        let config = Config {
            storage: StorageOptions {
                mem_cache_capacity: ReadableSize::mb(0),
                mem_cache_partition_bits: 0,
                object_store: ObjectStoreOptions::Local(LocalOptions {
                    data_path: dir.path().to_str().unwrap().to_string(),
                }),
            },

            wal_path: dir.path().to_str().unwrap().to_string(),
            wal_storage: WalStorageConfig::RocksDB,
            ..Default::default()
        };

        Self { config }
    }
}

impl Clone for RocksDBEngineContext {
    fn clone(&self) -> Self {
        let mut config = self.config.clone();

        let dir = tempfile::tempdir().unwrap();
        let storage = StorageOptions {
            mem_cache_capacity: ReadableSize::mb(0),
            mem_cache_partition_bits: 0,
            object_store: ObjectStoreOptions::Local(LocalOptions {
                data_path: dir.path().to_str().unwrap().to_string(),
            }),
        };

        config.storage = storage;
        config.wal_path = dir.path().to_str().unwrap().to_string();

        Self { config }
    }
}

impl EngineContext for RocksDBEngineContext {
    type EngineBuilder = RocksDBWalEngineBuilder;

    fn engine_builder(&self) -> Self::EngineBuilder {
        RocksDBWalEngineBuilder::default()
    }

    fn config(&self) -> Config {
        self.config.clone()
    }
}

#[derive(Clone)]
pub struct MemoryEngineContext {
    config: Config,
}

impl Default for MemoryEngineContext {
    fn default() -> Self {
        let dir = tempfile::tempdir().unwrap();

        let config = Config {
            storage: StorageOptions {
                mem_cache_capacity: ReadableSize::mb(0),
                mem_cache_partition_bits: 0,
                object_store: ObjectStoreOptions::Local(LocalOptions {
                    data_path: dir.path().to_str().unwrap().to_string(),
                }),
            },
            wal_path: dir.path().to_str().unwrap().to_string(),
            wal_storage: WalStorageConfig::Obkv(Box::new(ObkvWalConfig::default())),
            ..Default::default()
        };

        Self { config }
    }
}

impl EngineContext for MemoryEngineContext {
    type EngineBuilder = MemWalEngineBuilder;

    fn engine_builder(&self) -> Self::EngineBuilder {
        MemWalEngineBuilder::default()
    }

    fn config(&self) -> Config {
        self.config.clone()
    }
}
