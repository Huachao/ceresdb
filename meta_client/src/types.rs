// Copyright 2022 CeresDB Project Authors. Licensed under Apache-2.0.

use std::{collections::HashMap, sync::Arc};

use ceresdbproto::{cluster as cluster_pb, meta_service as meta_service_pb};
use common_types::{
    schema::{SchemaId, SchemaName},
    table::{TableId, TableName},
};
use common_util::config::ReadableDuration;
use serde_derive::Deserialize;
use snafu::OptionExt;

use crate::{Error, MissingShardInfo, MissingTableInfo, Result};

pub type ShardId = u32;
pub type ShardVersion = u64;
pub type ClusterNodesRef = Arc<Vec<NodeShard>>;

#[derive(Debug, Clone)]
pub struct RequestHeader {
    pub node: String,
    pub cluster_name: String,
}

#[derive(Debug)]
pub struct AllocSchemaIdRequest {
    pub name: String,
}

#[derive(Debug)]
pub struct AllocSchemaIdResponse {
    pub name: String,
    pub id: SchemaId,
}

#[derive(Clone, Debug)]
pub struct CreateTableRequest {
    pub schema_name: String,
    pub name: String,
    pub encoded_schema: Vec<u8>,
    pub engine: String,
    pub create_if_not_exist: bool,
    pub options: HashMap<String, String>,
}

#[derive(Clone, Debug)]
pub struct CreateTableResponse {
    pub created_table: TableInfo,
    pub shard_info: ShardInfo,
}

#[derive(Debug, Clone)]
pub struct DropTableRequest {
    pub schema_name: String,
    pub name: String,
}

#[derive(Debug, Clone)]
pub struct DropTableResponse {
    /// The dropped table.
    ///
    /// And it will be None if drop a non-exist table.
    pub dropped_table: Option<TableInfo>,
}

#[derive(Clone, Debug)]
pub struct GetTablesOfShardsRequest {
    pub shard_ids: Vec<ShardId>,
}

#[derive(Clone, Debug)]
pub struct GetTablesOfShardsResponse {
    pub tables_by_shard: HashMap<ShardId, TablesOfShard>,
}

#[derive(Clone, Debug)]
pub struct TableInfo {
    pub id: TableId,
    pub name: String,
    pub schema_id: SchemaId,
    pub schema_name: String,
}

impl From<meta_service_pb::TableInfo> for TableInfo {
    fn from(pb_table_info: meta_service_pb::TableInfo) -> Self {
        TableInfo {
            id: pb_table_info.id,
            name: pb_table_info.name,
            schema_id: pb_table_info.schema_id,
            schema_name: pb_table_info.schema_name,
        }
    }
}

#[derive(Clone, Debug)]
pub struct TablesOfShard {
    pub shard_info: ShardInfo,
    pub tables: Vec<TableInfo>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct NodeMetaInfo {
    pub addr: String,
    pub port: u16,
    pub zone: String,
    pub idc: String,
    pub binary_version: String,
}

impl NodeMetaInfo {
    pub fn endpoint(&self) -> String {
        format!("{}:{}", self.addr, self.port)
    }
}

#[derive(Debug, Clone)]
pub struct NodeInfo {
    pub node_meta_info: NodeMetaInfo,
    pub shard_infos: Vec<ShardInfo>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ShardInfo {
    pub id: ShardId,
    pub role: ShardRole,
    pub version: ShardVersion,
}

impl ShardInfo {
    #[inline]
    pub fn is_leader(&self) -> bool {
        self.role == ShardRole::Leader
    }
}

#[derive(Debug, Default, Copy, Clone, Eq, PartialEq)]
pub enum ShardRole {
    #[default]
    Leader,
    Follower,
    PendingLeader,
    PendingFollower,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct MetaClientConfig {
    pub cluster_name: String,
    pub meta_addr: String,
    pub meta_members_url: String,
    pub lease: ReadableDuration,
    pub timeout: ReadableDuration,
    pub cq_count: usize,
}

impl Default for MetaClientConfig {
    fn default() -> Self {
        Self {
            cluster_name: String::new(),
            meta_addr: "127.0.0.1:8080".to_string(),
            meta_members_url: "ceresmeta/members".to_string(),
            lease: ReadableDuration::secs(10),
            timeout: ReadableDuration::secs(5),
            cq_count: 8,
        }
    }
}

impl From<NodeInfo> for meta_service_pb::NodeInfo {
    fn from(node_info: NodeInfo) -> Self {
        let shard_infos = node_info
            .shard_infos
            .into_iter()
            .map(meta_service_pb::ShardInfo::from)
            .collect();

        Self {
            endpoint: node_info.node_meta_info.endpoint(),
            zone: node_info.node_meta_info.zone,
            binary_version: node_info.node_meta_info.binary_version,
            shard_infos,
            lease: 0,
        }
    }
}

impl From<ShardInfo> for meta_service_pb::ShardInfo {
    fn from(shard_info: ShardInfo) -> Self {
        let role = cluster_pb::ShardRole::from(shard_info.role);

        Self {
            id: shard_info.id,
            role: role as i32,
            version: shard_info.version,
        }
    }
}

impl From<meta_service_pb::ShardInfo> for ShardInfo {
    fn from(pb_shard_info: meta_service_pb::ShardInfo) -> Self {
        ShardInfo {
            id: pb_shard_info.id,
            role: pb_shard_info.role().into(),
            version: pb_shard_info.version,
        }
    }
}

impl From<ShardRole> for cluster_pb::ShardRole {
    fn from(shard_role: ShardRole) -> Self {
        match shard_role {
            ShardRole::Leader => cluster_pb::ShardRole::Leader,
            ShardRole::Follower => cluster_pb::ShardRole::Follower,
            ShardRole::PendingLeader => cluster_pb::ShardRole::PendingLeader,
            ShardRole::PendingFollower => cluster_pb::ShardRole::PendingFollower,
        }
    }
}

impl From<cluster_pb::ShardRole> for ShardRole {
    fn from(pb_role: cluster_pb::ShardRole) -> Self {
        match pb_role {
            cluster_pb::ShardRole::Leader => ShardRole::Leader,
            cluster_pb::ShardRole::Follower => ShardRole::Follower,
            cluster_pb::ShardRole::PendingLeader => ShardRole::PendingLeader,
            cluster_pb::ShardRole::PendingFollower => ShardRole::PendingFollower,
        }
    }
}

impl From<GetTablesOfShardsRequest> for meta_service_pb::GetTablesOfShardsRequest {
    fn from(req: GetTablesOfShardsRequest) -> Self {
        Self {
            header: None,
            shard_ids: req.shard_ids,
        }
    }
}

impl TryFrom<meta_service_pb::GetTablesOfShardsResponse> for GetTablesOfShardsResponse {
    type Error = Error;

    fn try_from(pb_resp: meta_service_pb::GetTablesOfShardsResponse) -> Result<Self> {
        let tables_by_shard = pb_resp
            .tables_by_shard
            .into_iter()
            .map(|(k, v)| Ok((k, TablesOfShard::try_from(v)?)))
            .collect::<Result<HashMap<_, _>>>()?;

        Ok(Self { tables_by_shard })
    }
}

impl TryFrom<meta_service_pb::TablesOfShard> for TablesOfShard {
    type Error = Error;

    fn try_from(pb_tables_of_shard: meta_service_pb::TablesOfShard) -> Result<Self> {
        let shard_info = pb_tables_of_shard
            .shard_info
            .with_context(|| MissingShardInfo {
                msg: "in meta_service_pb::TablesOfShard",
            })?;
        Ok(Self {
            shard_info: ShardInfo::from(shard_info),
            tables: pb_tables_of_shard
                .tables
                .into_iter()
                .map(Into::into)
                .collect(),
        })
    }
}

impl From<RequestHeader> for meta_service_pb::RequestHeader {
    fn from(req: RequestHeader) -> Self {
        Self {
            node: req.node,
            cluster_name: req.cluster_name,
        }
    }
}

impl From<AllocSchemaIdRequest> for meta_service_pb::AllocSchemaIdRequest {
    fn from(req: AllocSchemaIdRequest) -> Self {
        Self {
            header: None,
            name: req.name,
        }
    }
}

impl From<meta_service_pb::AllocSchemaIdResponse> for AllocSchemaIdResponse {
    fn from(pb_resp: meta_service_pb::AllocSchemaIdResponse) -> Self {
        Self {
            name: pb_resp.name,
            id: pb_resp.id,
        }
    }
}

impl From<CreateTableRequest> for meta_service_pb::CreateTableRequest {
    fn from(req: CreateTableRequest) -> Self {
        Self {
            header: None,
            schema_name: req.schema_name,
            name: req.name,
            encoded_schema: req.encoded_schema,
            engine: req.engine,
            create_if_not_exist: req.create_if_not_exist,
            options: req.options,
        }
    }
}

impl TryFrom<meta_service_pb::CreateTableResponse> for CreateTableResponse {
    type Error = Error;

    fn try_from(pb_resp: meta_service_pb::CreateTableResponse) -> Result<Self> {
        let pb_table_info = pb_resp.created_table.context(MissingTableInfo {
            msg: "created table is not found in the create table response",
        })?;
        let pb_shard_info = pb_resp.shard_info.context(MissingShardInfo {
            msg: "shard info is not found in the create table response",
        })?;

        Ok(Self {
            created_table: TableInfo::from(pb_table_info),
            shard_info: ShardInfo::from(pb_shard_info),
        })
    }
}

impl From<DropTableRequest> for meta_service_pb::DropTableRequest {
    fn from(req: DropTableRequest) -> Self {
        Self {
            header: None,
            schema_name: req.schema_name,
            name: req.name,
        }
    }
}

impl From<meta_service_pb::DropTableResponse> for DropTableResponse {
    fn from(pb_resp: meta_service_pb::DropTableResponse) -> Self {
        Self {
            dropped_table: pb_resp.dropped_table.map(TableInfo::from),
        }
    }
}

#[derive(Debug, Clone)]
pub struct RouteTablesRequest {
    pub schema_name: SchemaName,
    pub table_names: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeShard {
    pub endpoint: String,
    pub shard_info: ShardInfo,
}

#[derive(Debug, Clone)]
pub struct RouteEntry {
    pub table: TableInfo,
    pub node_shards: Vec<NodeShard>,
}

#[derive(Debug, Clone)]
pub struct RouteTablesResponse {
    pub cluster_topology_version: u64,
    pub entries: HashMap<String, RouteEntry>,
}

impl RouteTablesResponse {
    pub fn contains_all_tables(&self, queried_tables: &[TableName]) -> bool {
        queried_tables
            .iter()
            .all(|table_name| self.entries.contains_key(table_name))
    }
}

impl From<RouteTablesRequest> for meta_service_pb::RouteTablesRequest {
    fn from(req: RouteTablesRequest) -> Self {
        Self {
            header: None,
            schema_name: req.schema_name,
            table_names: req.table_names,
        }
    }
}

impl TryFrom<meta_service_pb::NodeShard> for NodeShard {
    type Error = Error;

    fn try_from(pb: meta_service_pb::NodeShard) -> Result<Self> {
        let pb_shard_info = pb.shard_info.with_context(|| MissingShardInfo {
            msg: "in meta_service_pb::NodeShard",
        })?;
        Ok(NodeShard {
            endpoint: pb.endpoint,
            shard_info: ShardInfo::from(pb_shard_info),
        })
    }
}

impl TryFrom<meta_service_pb::RouteEntry> for RouteEntry {
    type Error = Error;

    fn try_from(pb_entry: meta_service_pb::RouteEntry) -> Result<Self> {
        let mut node_shards = Vec::with_capacity(pb_entry.node_shards.len());
        for pb_node_shard in pb_entry.node_shards {
            let node_shard = NodeShard::try_from(pb_node_shard)?;
            node_shards.push(node_shard);
        }

        let table_info = pb_entry.table.context(MissingTableInfo {
            msg: "table info is missing in route entry",
        })?;
        Ok(RouteEntry {
            table: TableInfo::from(table_info),
            node_shards,
        })
    }
}

impl TryFrom<meta_service_pb::RouteTablesResponse> for RouteTablesResponse {
    type Error = Error;

    fn try_from(pb_resp: meta_service_pb::RouteTablesResponse) -> Result<Self> {
        let mut entries = HashMap::with_capacity(pb_resp.entries.len());
        for (table_name, entry) in pb_resp.entries {
            let route_entry = RouteEntry::try_from(entry)?;
            entries.insert(table_name, route_entry);
        }

        Ok(RouteTablesResponse {
            cluster_topology_version: pb_resp.cluster_topology_version,
            entries,
        })
    }
}

#[derive(Debug, Clone, Default)]
pub struct GetNodesRequest {}

pub struct GetNodesResponse {
    pub cluster_topology_version: u64,
    pub node_shards: Vec<NodeShard>,
}

impl From<GetNodesRequest> for meta_service_pb::GetNodesRequest {
    fn from(_: GetNodesRequest) -> Self {
        meta_service_pb::GetNodesRequest::default()
    }
}

impl TryFrom<meta_service_pb::GetNodesResponse> for GetNodesResponse {
    type Error = Error;

    fn try_from(pb_resp: meta_service_pb::GetNodesResponse) -> Result<Self> {
        let mut node_shards = Vec::with_capacity(pb_resp.node_shards.len());
        for node_shard in pb_resp.node_shards {
            node_shards.push(NodeShard::try_from(node_shard)?);
        }

        Ok(GetNodesResponse {
            cluster_topology_version: pb_resp.cluster_topology_version,
            node_shards,
        })
    }
}
