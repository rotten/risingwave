// Copyright 2023 RisingWave Labs
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::collections::HashMap;
use std::fmt;
use std::fmt::{Debug, Formatter};
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::anyhow;
use async_trait::async_trait;
use either::Either;
use futures::stream::BoxStream;
use itertools::Itertools;
use lru::LruCache;
use risingwave_common::catalog::{CatalogVersion, FunctionId, IndexId, TableId};
use risingwave_common::config::{MetaConfig, MAX_CONNECTION_WINDOW_SIZE};
use risingwave_common::hash::ParallelUnitMapping;
use risingwave_common::system_param::reader::SystemParamsReader;
use risingwave_common::telemetry::report::TelemetryInfoFetcher;
use risingwave_common::util::addr::HostAddr;
use risingwave_common::util::column_index_mapping::ColIndexMapping;
use risingwave_hummock_sdk::compaction_group::StateTableId;
use risingwave_hummock_sdk::{
    CompactionGroupId, HummockEpoch, HummockSstableObjectId, HummockVersionId, LocalSstableInfo,
    SstObjectIdRange,
};
use risingwave_pb::backup_service::backup_service_client::BackupServiceClient;
use risingwave_pb::backup_service::*;
use risingwave_pb::catalog::{
    Connection, PbDatabase, PbFunction, PbIndex, PbSchema, PbSink, PbSource, PbTable, PbView, Table,
};
use risingwave_pb::cloud_service::cloud_service_client::CloudServiceClient;
use risingwave_pb::cloud_service::*;
use risingwave_pb::common::{HostAddress, WorkerNode, WorkerType};
use risingwave_pb::connector_service::sink_coordination_service_client::SinkCoordinationServiceClient;
use risingwave_pb::ddl_service::alter_relation_name_request::Relation;
use risingwave_pb::ddl_service::ddl_service_client::DdlServiceClient;
use risingwave_pb::ddl_service::drop_table_request::SourceId;
use risingwave_pb::ddl_service::*;
use risingwave_pb::hummock::hummock_manager_service_client::HummockManagerServiceClient;
use risingwave_pb::hummock::rise_ctl_update_compaction_config_request::mutable_config::MutableConfig;
use risingwave_pb::hummock::subscribe_compaction_event_request::Register;
use risingwave_pb::hummock::write_limits::WriteLimit;
use risingwave_pb::hummock::*;
use risingwave_pb::meta::add_worker_node_request::Property;
use risingwave_pb::meta::cancel_creating_jobs_request::PbJobs;
use risingwave_pb::meta::cluster_service_client::ClusterServiceClient;
use risingwave_pb::meta::get_reschedule_plan_request::PbPolicy;
use risingwave_pb::meta::heartbeat_request::{extra_info, ExtraInfo};
use risingwave_pb::meta::heartbeat_service_client::HeartbeatServiceClient;
use risingwave_pb::meta::list_actor_states_response::ActorState;
use risingwave_pb::meta::list_fragment_distribution_response::FragmentDistribution;
use risingwave_pb::meta::list_table_fragment_states_response::TableFragmentState;
use risingwave_pb::meta::list_table_fragments_response::TableFragmentInfo;
use risingwave_pb::meta::meta_member_service_client::MetaMemberServiceClient;
use risingwave_pb::meta::notification_service_client::NotificationServiceClient;
use risingwave_pb::meta::scale_service_client::ScaleServiceClient;
use risingwave_pb::meta::serving_service_client::ServingServiceClient;
use risingwave_pb::meta::stream_manager_service_client::StreamManagerServiceClient;
use risingwave_pb::meta::system_params_service_client::SystemParamsServiceClient;
use risingwave_pb::meta::telemetry_info_service_client::TelemetryInfoServiceClient;
use risingwave_pb::meta::update_worker_node_schedulability_request::Schedulability;
use risingwave_pb::meta::*;
use risingwave_pb::stream_plan::StreamFragmentGraph;
use risingwave_pb::user::update_user_request::UpdateField;
use risingwave_pb::user::user_service_client::UserServiceClient;
use risingwave_pb::user::*;
use tokio::sync::mpsc::{unbounded_channel, Receiver, UnboundedSender};
use tokio::sync::oneshot::Sender;
use tokio::sync::{mpsc, oneshot, RwLock};
use tokio::task::JoinHandle;
use tokio::time::{self};
use tokio_retry::strategy::{jitter, ExponentialBackoff};
use tokio_stream::wrappers::UnboundedReceiverStream;
use tonic::transport::Endpoint;
use tonic::{Code, Request, Streaming};

use crate::error::{Result, RpcError};
use crate::hummock_meta_client::{CompactionEventItem, HummockMetaClient};
use crate::tracing::{Channel, TracingInjectedChannelExt};
use crate::{meta_rpc_client_method_impl, ExtraInfoSourceRef};

type ConnectionId = u32;
type DatabaseId = u32;
type SchemaId = u32;

/// Client to meta server. Cloning the instance is lightweight.
#[derive(Clone, Debug)]
pub struct MetaClient {
    worker_id: u32,
    worker_type: WorkerType,
    host_addr: HostAddr,
    inner: GrpcMetaClient,
    meta_config: MetaConfig,
}

impl MetaClient {
    const META_ADDRESS_LOAD_BALANCE_MODE_PREFIX: &'static str = "load-balance+";

    pub fn worker_id(&self) -> u32 {
        self.worker_id
    }

    pub fn host_addr(&self) -> &HostAddr {
        &self.host_addr
    }

    pub fn worker_type(&self) -> WorkerType {
        self.worker_type
    }

    /// Subscribe to notification from meta.
    pub async fn subscribe(
        &self,
        subscribe_type: SubscribeType,
    ) -> Result<Streaming<SubscribeResponse>> {
        let request = SubscribeRequest {
            subscribe_type: subscribe_type as i32,
            host: Some(self.host_addr.to_protobuf()),
            worker_id: self.worker_id(),
        };

        let retry_strategy = GrpcMetaClient::retry_strategy_to_bound(
            Duration::from_secs(self.meta_config.max_heartbeat_interval_secs as u64),
            true,
        );

        tokio_retry::Retry::spawn(retry_strategy, || async {
            let request = request.clone();
            self.inner.subscribe(request).await
        })
        .await
    }

    pub async fn create_connection(
        &self,
        connection_name: String,
        database_id: u32,
        schema_id: u32,
        owner_id: u32,
        req: create_connection_request::Payload,
    ) -> Result<(ConnectionId, CatalogVersion)> {
        let request = CreateConnectionRequest {
            name: connection_name,
            database_id,
            schema_id,
            owner_id,
            payload: Some(req),
        };
        let resp = self.inner.create_connection(request).await?;
        Ok((resp.connection_id, resp.version))
    }

    pub async fn list_connections(&self, _name: Option<&str>) -> Result<Vec<Connection>> {
        let request = ListConnectionsRequest {};
        let resp = self.inner.list_connections(request).await?;
        Ok(resp.connections)
    }

    pub async fn drop_connection(&self, connection_id: ConnectionId) -> Result<CatalogVersion> {
        let request = DropConnectionRequest { connection_id };
        let resp = self.inner.drop_connection(request).await?;
        Ok(resp.version)
    }

    pub(crate) fn parse_meta_addr(meta_addr: &str) -> Result<MetaAddressStrategy> {
        if meta_addr.starts_with(Self::META_ADDRESS_LOAD_BALANCE_MODE_PREFIX) {
            let addr = meta_addr
                .strip_prefix(Self::META_ADDRESS_LOAD_BALANCE_MODE_PREFIX)
                .unwrap();

            let addr = addr.split(',').exactly_one().map_err(|_| {
                RpcError::Internal(anyhow!(
                    "meta address {} in load-balance mode should be exactly one",
                    addr
                ))
            })?;

            let _url = url::Url::parse(addr).map_err(|e| {
                RpcError::Internal(anyhow!("could not parse meta address {}, {}", addr, e))
            })?;

            Ok(MetaAddressStrategy::LoadBalance(addr.to_string()))
        } else {
            let addrs: Vec<_> = meta_addr.split(',').map(str::to_string).collect();

            if addrs.is_empty() {
                return Err(RpcError::Internal(anyhow!(
                    "empty meta addresses {:?}",
                    addrs
                )));
            }

            for addr in &addrs {
                let _url = url::Url::parse(addr).map_err(|e| {
                    RpcError::Internal(anyhow!("could not parse meta address {}, {}", addr, e))
                })?;
            }

            Ok(MetaAddressStrategy::List(addrs))
        }
    }

    /// Register the current node to the cluster and set the corresponding worker id.
    pub async fn register_new(
        meta_addr: &str,
        worker_type: WorkerType,
        addr: &HostAddr,
        property: Property,
        meta_config: &MetaConfig,
    ) -> Result<(Self, SystemParamsReader)> {
        let addr_strategy = Self::parse_meta_addr(meta_addr)?;
        tracing::info!("register meta client using strategy: {}", addr_strategy);

        // Retry until reaching `max_heartbeat_interval_secs`
        let retry_strategy = GrpcMetaClient::retry_strategy_to_bound(
            Duration::from_secs(meta_config.max_heartbeat_interval_secs as u64),
            true,
        );

        if property.is_unschedulable {
            tracing::warn!("worker {:?} registered as unschedulable", addr.clone());
        }
        let init_result: Result<_> = tokio_retry::Retry::spawn(retry_strategy, || async {
            let grpc_meta_client = GrpcMetaClient::new(&addr_strategy, meta_config.clone()).await?;

            let add_worker_resp = grpc_meta_client
                .add_worker_node(AddWorkerNodeRequest {
                    worker_type: worker_type as i32,
                    host: Some(addr.to_protobuf()),
                    property: Some(property.clone()),
                })
                .await?;
            if let Some(status) = &add_worker_resp.status
                && status.code() == risingwave_pb::common::status::Code::UnknownWorker {
                tracing::error!("invalid worker: {}", status.message);
                std::process::exit(1);
            }

            let system_params_resp = grpc_meta_client
                .get_system_params(GetSystemParamsRequest {})
                .await?;

            Ok((add_worker_resp, system_params_resp, grpc_meta_client))
        })
        .await;

        let (add_worker_resp, system_params_resp, grpc_meta_client) = init_result?;
        let worker_node = add_worker_resp
            .node
            .expect("AddWorkerNodeResponse::node is empty");

        Ok((
            Self {
                worker_id: worker_node.id,
                worker_type,
                host_addr: addr.clone(),
                inner: grpc_meta_client,
                meta_config: meta_config.to_owned(),
            },
            system_params_resp.params.unwrap().into(),
        ))
    }

    /// Activate the current node in cluster to confirm it's ready to serve.
    pub async fn activate(&self, addr: &HostAddr) -> Result<()> {
        let request = ActivateWorkerNodeRequest {
            host: Some(addr.to_protobuf()),
        };
        let retry_strategy = GrpcMetaClient::retry_strategy_to_bound(
            Duration::from_secs(self.meta_config.max_heartbeat_interval_secs as u64),
            true,
        );
        tokio_retry::Retry::spawn(retry_strategy, || async {
            let request = request.clone();
            self.inner.activate_worker_node(request).await
        })
        .await?;

        Ok(())
    }

    /// Send heartbeat signal to meta service.
    pub async fn send_heartbeat(&self, node_id: u32, info: Vec<extra_info::Info>) -> Result<()> {
        let request = HeartbeatRequest {
            node_id,
            info: info
                .into_iter()
                .map(|info| ExtraInfo { info: Some(info) })
                .collect(),
        };
        let resp = self.inner.heartbeat(request).await?;
        if let Some(status) = resp.status {
            if status.code() == risingwave_pb::common::status::Code::UnknownWorker {
                tracing::error!("worker expired: {}", status.message);
                std::process::exit(1);
            }
        }
        Ok(())
    }

    pub async fn create_database(&self, db: PbDatabase) -> Result<(DatabaseId, CatalogVersion)> {
        let request = CreateDatabaseRequest { db: Some(db) };
        let resp = self.inner.create_database(request).await?;
        // TODO: handle error in `resp.status` here
        Ok((resp.database_id, resp.version))
    }

    pub async fn create_schema(&self, schema: PbSchema) -> Result<(SchemaId, CatalogVersion)> {
        let request = CreateSchemaRequest {
            schema: Some(schema),
        };
        let resp = self.inner.create_schema(request).await?;
        // TODO: handle error in `resp.status` here
        Ok((resp.schema_id, resp.version))
    }

    pub async fn create_materialized_view(
        &self,
        table: PbTable,
        graph: StreamFragmentGraph,
    ) -> Result<(TableId, CatalogVersion)> {
        let request = CreateMaterializedViewRequest {
            materialized_view: Some(table),
            fragment_graph: Some(graph),
        };
        let resp = self.inner.create_materialized_view(request).await?;
        // TODO: handle error in `resp.status` here
        Ok((resp.table_id.into(), resp.version))
    }

    pub async fn drop_materialized_view(
        &self,
        table_id: TableId,
        cascade: bool,
    ) -> Result<CatalogVersion> {
        let request = DropMaterializedViewRequest {
            table_id: table_id.table_id(),
            cascade,
        };

        let resp = self.inner.drop_materialized_view(request).await?;
        Ok(resp.version)
    }

    pub async fn create_source(&self, source: PbSource) -> Result<(u32, CatalogVersion)> {
        let request = CreateSourceRequest {
            source: Some(source),
        };

        let resp = self.inner.create_source(request).await?;
        Ok((resp.source_id, resp.version))
    }

    pub async fn create_sink(
        &self,
        sink: PbSink,
        graph: StreamFragmentGraph,
    ) -> Result<(u32, CatalogVersion)> {
        let request = CreateSinkRequest {
            sink: Some(sink),
            fragment_graph: Some(graph),
        };

        let resp = self.inner.create_sink(request).await?;
        Ok((resp.sink_id, resp.version))
    }

    pub async fn create_function(
        &self,
        function: PbFunction,
    ) -> Result<(FunctionId, CatalogVersion)> {
        let request = CreateFunctionRequest {
            function: Some(function),
        };
        let resp = self.inner.create_function(request).await?;
        Ok((resp.function_id.into(), resp.version))
    }

    pub async fn create_table(
        &self,
        source: Option<PbSource>,
        table: PbTable,
        graph: StreamFragmentGraph,
    ) -> Result<(TableId, CatalogVersion)> {
        let request = CreateTableRequest {
            materialized_view: Some(table),
            fragment_graph: Some(graph),
            source,
        };
        let resp = self.inner.create_table(request).await?;
        // TODO: handle error in `resp.status` here
        Ok((resp.table_id.into(), resp.version))
    }

    pub async fn alter_relation_name(
        &self,
        relation: Relation,
        name: &str,
    ) -> Result<CatalogVersion> {
        let request = AlterRelationNameRequest {
            relation: Some(relation),
            new_name: name.to_string(),
        };
        let resp = self.inner.alter_relation_name(request).await?;
        Ok(resp.version)
    }

    // only adding columns is supported
    pub async fn alter_source_column(&self, source: PbSource) -> Result<CatalogVersion> {
        let request = AlterSourceRequest {
            source: Some(source),
        };
        let resp = self.inner.alter_source(request).await?;
        Ok(resp.version)
    }

    pub async fn replace_table(
        &self,
        source: Option<PbSource>,
        table: PbTable,
        graph: StreamFragmentGraph,
        table_col_index_mapping: ColIndexMapping,
    ) -> Result<CatalogVersion> {
        let request = ReplaceTablePlanRequest {
            source,
            table: Some(table),
            fragment_graph: Some(graph),
            table_col_index_mapping: Some(table_col_index_mapping.to_protobuf()),
        };
        let resp = self.inner.replace_table_plan(request).await?;
        // TODO: handle error in `resp.status` here
        Ok(resp.version)
    }

    pub async fn create_view(&self, view: PbView) -> Result<(u32, CatalogVersion)> {
        let request = CreateViewRequest { view: Some(view) };
        let resp = self.inner.create_view(request).await?;
        // TODO: handle error in `resp.status` here
        Ok((resp.view_id, resp.version))
    }

    pub async fn create_index(
        &self,
        index: PbIndex,
        table: PbTable,
        graph: StreamFragmentGraph,
    ) -> Result<(TableId, CatalogVersion)> {
        let request = CreateIndexRequest {
            index: Some(index),
            index_table: Some(table),
            fragment_graph: Some(graph),
        };
        let resp = self.inner.create_index(request).await?;
        // TODO: handle error in `resp.status` here
        Ok((resp.index_id.into(), resp.version))
    }

    pub async fn drop_table(
        &self,
        source_id: Option<u32>,
        table_id: TableId,
        cascade: bool,
    ) -> Result<CatalogVersion> {
        let request = DropTableRequest {
            source_id: source_id.map(SourceId::Id),
            table_id: table_id.table_id(),
            cascade,
        };

        let resp = self.inner.drop_table(request).await?;
        Ok(resp.version)
    }

    pub async fn drop_view(&self, view_id: u32, cascade: bool) -> Result<CatalogVersion> {
        let request = DropViewRequest { view_id, cascade };
        let resp = self.inner.drop_view(request).await?;
        Ok(resp.version)
    }

    pub async fn drop_source(&self, source_id: u32, cascade: bool) -> Result<CatalogVersion> {
        let request = DropSourceRequest { source_id, cascade };
        let resp = self.inner.drop_source(request).await?;
        Ok(resp.version)
    }

    pub async fn drop_sink(&self, sink_id: u32, cascade: bool) -> Result<CatalogVersion> {
        let request = DropSinkRequest { sink_id, cascade };
        let resp = self.inner.drop_sink(request).await?;
        Ok(resp.version)
    }

    pub async fn drop_index(&self, index_id: IndexId, cascade: bool) -> Result<CatalogVersion> {
        let request = DropIndexRequest {
            index_id: index_id.index_id,
            cascade,
        };
        let resp = self.inner.drop_index(request).await?;
        Ok(resp.version)
    }

    pub async fn drop_function(&self, function_id: FunctionId) -> Result<CatalogVersion> {
        let request = DropFunctionRequest {
            function_id: function_id.0,
        };
        let resp = self.inner.drop_function(request).await?;
        Ok(resp.version)
    }

    pub async fn drop_database(&self, database_id: u32) -> Result<CatalogVersion> {
        let request = DropDatabaseRequest { database_id };
        let resp = self.inner.drop_database(request).await?;
        Ok(resp.version)
    }

    pub async fn drop_schema(&self, schema_id: u32) -> Result<CatalogVersion> {
        let request = DropSchemaRequest { schema_id };
        let resp = self.inner.drop_schema(request).await?;
        Ok(resp.version)
    }

    // TODO: using UserInfoVersion instead as return type.
    pub async fn create_user(&self, user: UserInfo) -> Result<u64> {
        let request = CreateUserRequest { user: Some(user) };
        let resp = self.inner.create_user(request).await?;
        Ok(resp.version)
    }

    pub async fn drop_user(&self, user_id: u32) -> Result<u64> {
        let request = DropUserRequest { user_id };
        let resp = self.inner.drop_user(request).await?;
        Ok(resp.version)
    }

    pub async fn update_user(
        &self,
        user: UserInfo,
        update_fields: Vec<UpdateField>,
    ) -> Result<u64> {
        let request = UpdateUserRequest {
            user: Some(user),
            update_fields: update_fields
                .into_iter()
                .map(|field| field as i32)
                .collect::<Vec<_>>(),
        };
        let resp = self.inner.update_user(request).await?;
        Ok(resp.version)
    }

    pub async fn grant_privilege(
        &self,
        user_ids: Vec<u32>,
        privileges: Vec<GrantPrivilege>,
        with_grant_option: bool,
        granted_by: u32,
    ) -> Result<u64> {
        let request = GrantPrivilegeRequest {
            user_ids,
            privileges,
            with_grant_option,
            granted_by,
        };
        let resp = self.inner.grant_privilege(request).await?;
        Ok(resp.version)
    }

    pub async fn revoke_privilege(
        &self,
        user_ids: Vec<u32>,
        privileges: Vec<GrantPrivilege>,
        granted_by: Option<u32>,
        revoke_by: u32,
        revoke_grant_option: bool,
        cascade: bool,
    ) -> Result<u64> {
        let granted_by = granted_by.unwrap_or_default();
        let request = RevokePrivilegeRequest {
            user_ids,
            privileges,
            granted_by,
            revoke_by,
            revoke_grant_option,
            cascade,
        };
        let resp = self.inner.revoke_privilege(request).await?;
        Ok(resp.version)
    }

    /// Unregister the current node to the cluster.
    pub async fn unregister(&self, addr: HostAddr) -> Result<()> {
        let request = DeleteWorkerNodeRequest {
            host: Some(addr.to_protobuf()),
        };
        self.inner.delete_worker_node(request).await?;
        Ok(())
    }

    pub async fn update_schedulability(
        &self,
        worker_ids: &[u32],
        schedulability: Schedulability,
    ) -> Result<UpdateWorkerNodeSchedulabilityResponse> {
        let request = UpdateWorkerNodeSchedulabilityRequest {
            worker_ids: worker_ids.to_vec(),
            schedulability: schedulability.into(),
        };
        let resp = self
            .inner
            .update_worker_node_schedulability(request)
            .await?;
        Ok(resp)
    }

    pub async fn list_worker_nodes(&self, worker_type: WorkerType) -> Result<Vec<WorkerNode>> {
        let request = ListAllNodesRequest {
            worker_type: worker_type as _,
            include_starting_nodes: true,
        };
        let resp = self.inner.list_all_nodes(request).await?;
        Ok(resp.nodes)
    }

    /// Starts a heartbeat worker.
    ///
    /// When sending heartbeat RPC, it also carries extra info from `extra_info_sources`.
    pub fn start_heartbeat_loop(
        meta_client: MetaClient,
        min_interval: Duration,
        extra_info_sources: Vec<ExtraInfoSourceRef>,
    ) -> (JoinHandle<()>, Sender<()>) {
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel();
        let join_handle = tokio::spawn(async move {
            let mut min_interval_ticker = tokio::time::interval(min_interval);
            loop {
                tokio::select! {
                    biased;
                    // Shutdown
                    _ = &mut shutdown_rx => {
                        tracing::info!("Heartbeat loop is stopped");
                        return;
                    }
                    // Wait for interval
                    _ = min_interval_ticker.tick() => {},
                }
                let mut extra_info = Vec::with_capacity(extra_info_sources.len());
                for extra_info_source in &extra_info_sources {
                    if let Some(info) = extra_info_source.get_extra_info().await {
                        // None means the info is not available at the moment, and won't be sent to
                        // meta.
                        extra_info.push(info);
                    }
                }
                tracing::trace!(target: "events::meta::client_heartbeat", "heartbeat");
                match tokio::time::timeout(
                    // TODO: decide better min_interval for timeout
                    min_interval * 3,
                    meta_client.send_heartbeat(meta_client.worker_id(), extra_info),
                )
                .await
                {
                    Ok(Ok(_)) => {}
                    Ok(Err(err)) => {
                        tracing::warn!("Failed to send_heartbeat: error {}", err);
                    }
                    Err(err) => {
                        tracing::warn!("Failed to send_heartbeat: timeout {}", err);
                    }
                }
            }
        });
        (join_handle, shutdown_tx)
    }

    pub async fn risectl_list_state_tables(&self) -> Result<Vec<PbTable>> {
        let request = RisectlListStateTablesRequest {};
        let resp = self.inner.risectl_list_state_tables(request).await?;
        Ok(resp.tables)
    }

    pub async fn flush(&self, checkpoint: bool) -> Result<HummockSnapshot> {
        let request = FlushRequest { checkpoint };
        let resp = self.inner.flush(request).await?;
        Ok(resp.snapshot.unwrap())
    }

    pub async fn cancel_creating_jobs(&self, jobs: PbJobs) -> Result<Vec<u32>> {
        let request = CancelCreatingJobsRequest { jobs: Some(jobs) };
        let resp = self.inner.cancel_creating_jobs(request).await?;
        Ok(resp.canceled_jobs)
    }

    pub async fn list_table_fragments(
        &self,
        table_ids: &[u32],
    ) -> Result<HashMap<u32, TableFragmentInfo>> {
        let request = ListTableFragmentsRequest {
            table_ids: table_ids.to_vec(),
        };
        let resp = self.inner.list_table_fragments(request).await?;
        Ok(resp.table_fragments)
    }

    pub async fn list_table_fragment_states(&self) -> Result<Vec<TableFragmentState>> {
        let resp = self
            .inner
            .list_table_fragment_states(ListTableFragmentStatesRequest {})
            .await?;
        Ok(resp.states)
    }

    pub async fn list_fragment_distributions(&self) -> Result<Vec<FragmentDistribution>> {
        let resp = self
            .inner
            .list_fragment_distribution(ListFragmentDistributionRequest {})
            .await?;
        Ok(resp.distributions)
    }

    pub async fn list_actor_states(&self) -> Result<Vec<ActorState>> {
        let resp = self
            .inner
            .list_actor_states(ListActorStatesRequest {})
            .await?;
        Ok(resp.states)
    }

    pub async fn pause(&self) -> Result<PauseResponse> {
        let request = PauseRequest {};
        let resp = self.inner.pause(request).await?;
        Ok(resp)
    }

    pub async fn resume(&self) -> Result<ResumeResponse> {
        let request = ResumeRequest {};
        let resp = self.inner.resume(request).await?;
        Ok(resp)
    }

    pub async fn get_cluster_info(&self) -> Result<GetClusterInfoResponse> {
        let request = GetClusterInfoRequest {};
        let resp = self.inner.get_cluster_info(request).await?;
        Ok(resp)
    }

    pub async fn reschedule(
        &self,
        reschedules: HashMap<u32, PbReschedule>,
        revision: u64,
        resolve_no_shuffle_upstream: bool,
    ) -> Result<(bool, u64)> {
        let request = RescheduleRequest {
            reschedules,
            revision,
            resolve_no_shuffle_upstream,
        };
        let resp = self.inner.reschedule(request).await?;
        Ok((resp.success, resp.revision))
    }

    pub async fn get_reschedule_plan(
        &self,
        policy: PbPolicy,
        revision: u64,
    ) -> Result<GetReschedulePlanResponse> {
        let request = GetReschedulePlanRequest {
            revision,
            policy: Some(policy),
        };
        let resp = self.inner.get_reschedule_plan(request).await?;
        Ok(resp)
    }

    pub async fn risectl_get_pinned_versions_summary(
        &self,
    ) -> Result<RiseCtlGetPinnedVersionsSummaryResponse> {
        let request = RiseCtlGetPinnedVersionsSummaryRequest {};
        self.inner
            .rise_ctl_get_pinned_versions_summary(request)
            .await
    }

    pub async fn risectl_get_pinned_snapshots_summary(
        &self,
    ) -> Result<RiseCtlGetPinnedSnapshotsSummaryResponse> {
        let request = RiseCtlGetPinnedSnapshotsSummaryRequest {};
        self.inner
            .rise_ctl_get_pinned_snapshots_summary(request)
            .await
    }

    pub async fn risectl_get_checkpoint_hummock_version(
        &self,
    ) -> Result<RiseCtlGetCheckpointVersionResponse> {
        let request = RiseCtlGetCheckpointVersionRequest {};
        self.inner.rise_ctl_get_checkpoint_version(request).await
    }

    pub async fn risectl_pause_hummock_version_checkpoint(
        &self,
    ) -> Result<RiseCtlPauseVersionCheckpointResponse> {
        let request = RiseCtlPauseVersionCheckpointRequest {};
        self.inner.rise_ctl_pause_version_checkpoint(request).await
    }

    pub async fn risectl_resume_hummock_version_checkpoint(
        &self,
    ) -> Result<RiseCtlResumeVersionCheckpointResponse> {
        let request = RiseCtlResumeVersionCheckpointRequest {};
        self.inner.rise_ctl_resume_version_checkpoint(request).await
    }

    pub async fn init_metadata_for_replay(
        &self,
        tables: Vec<PbTable>,
        compaction_groups: Vec<CompactionGroupInfo>,
    ) -> Result<()> {
        let req = InitMetadataForReplayRequest {
            tables,
            compaction_groups,
        };
        let _resp = self.inner.init_metadata_for_replay(req).await?;
        Ok(())
    }

    pub async fn replay_version_delta(
        &self,
        version_delta: HummockVersionDelta,
    ) -> Result<(HummockVersion, Vec<CompactionGroupId>)> {
        let req = ReplayVersionDeltaRequest {
            version_delta: Some(version_delta),
        };
        let resp = self.inner.replay_version_delta(req).await?;
        Ok((resp.version.unwrap(), resp.modified_compaction_groups))
    }

    pub async fn list_version_deltas(
        &self,
        start_id: u64,
        num_limit: u32,
        committed_epoch_limit: HummockEpoch,
    ) -> Result<HummockVersionDeltas> {
        let req = ListVersionDeltasRequest {
            start_id,
            num_limit,
            committed_epoch_limit,
        };
        Ok(self
            .inner
            .list_version_deltas(req)
            .await?
            .version_deltas
            .unwrap())
    }

    pub async fn trigger_compaction_deterministic(
        &self,
        version_id: HummockVersionId,
        compaction_groups: Vec<CompactionGroupId>,
    ) -> Result<()> {
        let req = TriggerCompactionDeterministicRequest {
            version_id,
            compaction_groups,
        };
        self.inner.trigger_compaction_deterministic(req).await?;
        Ok(())
    }

    pub async fn disable_commit_epoch(&self) -> Result<HummockVersion> {
        let req = DisableCommitEpochRequest {};
        Ok(self
            .inner
            .disable_commit_epoch(req)
            .await?
            .current_version
            .unwrap())
    }

    pub async fn pin_specific_snapshot(&self, epoch: HummockEpoch) -> Result<HummockSnapshot> {
        let req = PinSpecificSnapshotRequest {
            context_id: self.worker_id(),
            epoch,
        };
        let resp = self.inner.pin_specific_snapshot(req).await?;
        Ok(resp.snapshot.unwrap())
    }

    pub async fn get_assigned_compact_task_num(&self) -> Result<usize> {
        let req = GetAssignedCompactTaskNumRequest {};
        let resp = self.inner.get_assigned_compact_task_num(req).await?;
        Ok(resp.num_tasks as usize)
    }

    pub async fn risectl_list_compaction_group(&self) -> Result<Vec<CompactionGroupInfo>> {
        let req = RiseCtlListCompactionGroupRequest {};
        let resp = self.inner.rise_ctl_list_compaction_group(req).await?;
        Ok(resp.compaction_groups)
    }

    pub async fn risectl_update_compaction_config(
        &self,
        compaction_groups: &[CompactionGroupId],
        configs: &[MutableConfig],
    ) -> Result<()> {
        let req = RiseCtlUpdateCompactionConfigRequest {
            compaction_group_ids: compaction_groups.to_vec(),
            configs: configs
                .iter()
                .map(
                    |c| rise_ctl_update_compaction_config_request::MutableConfig {
                        mutable_config: Some(c.clone()),
                    },
                )
                .collect(),
        };
        let _resp = self.inner.rise_ctl_update_compaction_config(req).await?;
        Ok(())
    }

    pub async fn backup_meta(&self) -> Result<u64> {
        let req = BackupMetaRequest {};
        let resp = self.inner.backup_meta(req).await?;
        Ok(resp.job_id)
    }

    pub async fn get_backup_job_status(&self, job_id: u64) -> Result<(BackupJobStatus, String)> {
        let req = GetBackupJobStatusRequest { job_id };
        let resp = self.inner.get_backup_job_status(req).await?;
        Ok((resp.job_status(), resp.message))
    }

    pub async fn delete_meta_snapshot(&self, snapshot_ids: &[u64]) -> Result<()> {
        let req = DeleteMetaSnapshotRequest {
            snapshot_ids: snapshot_ids.to_vec(),
        };
        let _resp = self.inner.delete_meta_snapshot(req).await?;
        Ok(())
    }

    pub async fn get_meta_snapshot_manifest(&self) -> Result<MetaSnapshotManifest> {
        let req = GetMetaSnapshotManifestRequest {};
        let resp = self.inner.get_meta_snapshot_manifest(req).await?;
        Ok(resp.manifest.expect("should exist"))
    }

    pub async fn get_telemetry_info(&self) -> Result<TelemetryInfoResponse> {
        let req = GetTelemetryInfoRequest {};
        let resp = self.inner.get_telemetry_info(req).await?;
        Ok(resp)
    }

    pub async fn get_system_params(&self) -> Result<SystemParamsReader> {
        let req = GetSystemParamsRequest {};
        let resp = self.inner.get_system_params(req).await?;
        Ok(resp.params.unwrap().into())
    }

    pub async fn set_system_param(
        &self,
        param: String,
        value: Option<String>,
    ) -> Result<Option<SystemParamsReader>> {
        let req = SetSystemParamRequest { param, value };
        let resp = self.inner.set_system_param(req).await?;
        Ok(resp.params.map(SystemParamsReader::from))
    }

    pub async fn get_ddl_progress(&self) -> Result<Vec<DdlProgress>> {
        let req = GetDdlProgressRequest {};
        let resp = self.inner.get_ddl_progress(req).await?;
        Ok(resp.ddl_progress)
    }

    pub async fn split_compaction_group(
        &self,
        group_id: CompactionGroupId,
        table_ids_to_new_group: &[StateTableId],
    ) -> Result<CompactionGroupId> {
        let req = SplitCompactionGroupRequest {
            group_id,
            table_ids: table_ids_to_new_group.to_vec(),
        };
        let resp = self.inner.split_compaction_group(req).await?;
        Ok(resp.new_group_id)
    }

    pub async fn get_tables(&self, table_ids: &[u32]) -> Result<HashMap<u32, Table>> {
        let req = GetTablesRequest {
            table_ids: table_ids.to_vec(),
        };
        let resp = self.inner.get_tables(req).await?;
        Ok(resp.tables)
    }

    pub async fn list_serving_vnode_mappings(
        &self,
    ) -> Result<HashMap<u32, (u32, ParallelUnitMapping)>> {
        let req = GetServingVnodeMappingsRequest {};
        let resp = self.inner.get_serving_vnode_mappings(req).await?;
        let mappings = resp
            .mappings
            .into_iter()
            .map(|p| {
                (
                    p.fragment_id,
                    (
                        resp.fragment_to_table
                            .get(&p.fragment_id)
                            .cloned()
                            .unwrap_or(0),
                        ParallelUnitMapping::from_protobuf(p.mapping.as_ref().unwrap()),
                    ),
                )
            })
            .collect();
        Ok(mappings)
    }

    pub async fn risectl_list_compaction_status(
        &self,
    ) -> Result<(
        Vec<CompactStatus>,
        Vec<CompactTaskAssignment>,
        Vec<CompactTaskProgress>,
    )> {
        let req = RiseCtlListCompactionStatusRequest {};
        let resp = self.inner.rise_ctl_list_compaction_status(req).await?;
        Ok((
            resp.compaction_statuses,
            resp.task_assignment,
            resp.task_progress,
        ))
    }

    pub async fn list_branched_object(&self) -> Result<Vec<BranchedObject>> {
        let req = ListBranchedObjectRequest {};
        let resp = self.inner.list_branched_object(req).await?;
        Ok(resp.branched_objects)
    }

    pub async fn list_active_write_limit(&self) -> Result<HashMap<u64, WriteLimit>> {
        let req = ListActiveWriteLimitRequest {};
        let resp = self.inner.list_active_write_limit(req).await?;
        Ok(resp.write_limits)
    }

    pub async fn list_hummock_meta_config(&self) -> Result<HashMap<String, String>> {
        let req = ListHummockMetaConfigRequest {};
        let resp = self.inner.list_hummock_meta_config(req).await?;
        Ok(resp.configs)
    }

    pub async fn delete_worker_node(&self, worker: HostAddress) -> Result<()> {
        let _resp = self
            .inner
            .delete_worker_node(DeleteWorkerNodeRequest { host: Some(worker) })
            .await?;

        Ok(())
    }

    pub async fn rw_cloud_validate_source(
        &self,
        source_type: SourceType,
        source_config: HashMap<String, String>,
    ) -> Result<RwCloudValidateSourceResponse> {
        let req = RwCloudValidateSourceRequest {
            source_type: source_type.into(),
            source_config,
        };
        let resp = self.inner.rw_cloud_validate_source(req).await?;
        Ok(resp)
    }

    pub async fn sink_coordinate_client(&self) -> SinkCoordinationRpcClient {
        self.inner.core.read().await.sink_coordinate_client.clone()
    }
}

#[async_trait]
impl HummockMetaClient for MetaClient {
    async fn unpin_version_before(&self, unpin_version_before: HummockVersionId) -> Result<()> {
        let req = UnpinVersionBeforeRequest {
            context_id: self.worker_id(),
            unpin_version_before,
        };
        self.inner.unpin_version_before(req).await?;
        Ok(())
    }

    async fn get_current_version(&self) -> Result<HummockVersion> {
        let req = GetCurrentVersionRequest::default();
        Ok(self
            .inner
            .get_current_version(req)
            .await?
            .current_version
            .unwrap())
    }

    async fn pin_snapshot(&self) -> Result<HummockSnapshot> {
        let req = PinSnapshotRequest {
            context_id: self.worker_id(),
        };
        let resp = self.inner.pin_snapshot(req).await?;
        Ok(resp.snapshot.unwrap())
    }

    async fn get_snapshot(&self) -> Result<HummockSnapshot> {
        let req = GetEpochRequest {};
        let resp = self.inner.get_epoch(req).await?;
        Ok(resp.snapshot.unwrap())
    }

    async fn unpin_snapshot(&self) -> Result<()> {
        let req = UnpinSnapshotRequest {
            context_id: self.worker_id(),
        };
        self.inner.unpin_snapshot(req).await?;
        Ok(())
    }

    async fn unpin_snapshot_before(&self, pinned_epochs: HummockEpoch) -> Result<()> {
        let req = UnpinSnapshotBeforeRequest {
            context_id: self.worker_id(),
            // For unpin_snapshot_before, we do not care about snapshots list but only min epoch.
            min_snapshot: Some(HummockSnapshot {
                committed_epoch: pinned_epochs,
                current_epoch: pinned_epochs,
            }),
        };
        self.inner.unpin_snapshot_before(req).await?;
        Ok(())
    }

    async fn get_new_sst_ids(&self, number: u32) -> Result<SstObjectIdRange> {
        let resp = self
            .inner
            .get_new_sst_ids(GetNewSstIdsRequest { number })
            .await?;
        Ok(SstObjectIdRange::new(resp.start_id, resp.end_id))
    }

    async fn commit_epoch(
        &self,
        _epoch: HummockEpoch,
        _sstables: Vec<LocalSstableInfo>,
    ) -> Result<()> {
        panic!("Only meta service can commit_epoch in production.")
    }

    async fn update_current_epoch(&self, _epoch: HummockEpoch) -> Result<()> {
        panic!("Only meta service can update_current_epoch in production.")
    }

    async fn report_vacuum_task(&self, vacuum_task: VacuumTask) -> Result<()> {
        let req = ReportVacuumTaskRequest {
            vacuum_task: Some(vacuum_task),
        };
        self.inner.report_vacuum_task(req).await?;
        Ok(())
    }

    async fn report_full_scan_task(
        &self,
        filtered_object_ids: Vec<HummockSstableObjectId>,
        total_object_count: u64,
        total_object_size: u64,
    ) -> Result<()> {
        let req = ReportFullScanTaskRequest {
            object_ids: filtered_object_ids,
            total_object_count,
            total_object_size,
        };
        self.inner.report_full_scan_task(req).await?;
        Ok(())
    }

    async fn trigger_manual_compaction(
        &self,
        compaction_group_id: u64,
        table_id: u32,
        level: u32,
        sst_ids: Vec<u64>,
    ) -> Result<()> {
        // TODO: support key_range parameter
        let req = TriggerManualCompactionRequest {
            compaction_group_id,
            table_id,
            // if table_id not exist, manual_compaction will include all the sst
            // without check internal_table_id
            level,
            sst_ids,
            ..Default::default()
        };

        self.inner.trigger_manual_compaction(req).await?;
        Ok(())
    }

    async fn trigger_full_gc(&self, sst_retention_time_sec: u64) -> Result<()> {
        self.inner
            .trigger_full_gc(TriggerFullGcRequest {
                sst_retention_time_sec,
            })
            .await?;
        Ok(())
    }

    async fn subscribe_compaction_event(
        &self,
    ) -> Result<(
        UnboundedSender<SubscribeCompactionEventRequest>,
        BoxStream<'static, CompactionEventItem>,
    )> {
        let (request_sender, request_receiver) =
            unbounded_channel::<SubscribeCompactionEventRequest>();
        request_sender
            .send(SubscribeCompactionEventRequest {
                event: Some(subscribe_compaction_event_request::Event::Register(
                    Register {
                        context_id: self.worker_id(),
                    },
                )),
                create_at: SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .expect("Clock may have gone backwards")
                    .as_millis() as u64,
            })
            .map_err(|err| RpcError::Internal(anyhow!(err.to_string())))?;

        let stream = self
            .inner
            .subscribe_compaction_event(Request::new(UnboundedReceiverStream::new(
                request_receiver,
            )))
            .await?;

        Ok((request_sender, Box::pin(stream)))
    }
}

#[async_trait]
impl TelemetryInfoFetcher for MetaClient {
    async fn fetch_telemetry_info(&self) -> std::result::Result<Option<String>, String> {
        let resp = self.get_telemetry_info().await.map_err(|e| e.to_string())?;
        let tracking_id = resp.get_tracking_id().ok();
        Ok(tracking_id.map(|id| id.to_owned()))
    }
}

pub type SinkCoordinationRpcClient = SinkCoordinationServiceClient<Channel>;

#[derive(Debug, Clone)]
struct GrpcMetaClientCore {
    cluster_client: ClusterServiceClient<Channel>,
    meta_member_client: MetaMemberServiceClient<Channel>,
    heartbeat_client: HeartbeatServiceClient<Channel>,
    ddl_client: DdlServiceClient<Channel>,
    hummock_client: HummockManagerServiceClient<Channel>,
    notification_client: NotificationServiceClient<Channel>,
    stream_client: StreamManagerServiceClient<Channel>,
    user_client: UserServiceClient<Channel>,
    scale_client: ScaleServiceClient<Channel>,
    backup_client: BackupServiceClient<Channel>,
    telemetry_client: TelemetryInfoServiceClient<Channel>,
    system_params_client: SystemParamsServiceClient<Channel>,
    serving_client: ServingServiceClient<Channel>,
    cloud_client: CloudServiceClient<Channel>,
    sink_coordinate_client: SinkCoordinationRpcClient,
}

impl GrpcMetaClientCore {
    pub(crate) fn new(channel: Channel) -> Self {
        let cluster_client = ClusterServiceClient::new(channel.clone());
        let meta_member_client = MetaMemberClient::new(channel.clone());
        let heartbeat_client = HeartbeatServiceClient::new(channel.clone());
        let ddl_client =
            DdlServiceClient::new(channel.clone()).max_decoding_message_size(usize::MAX);
        let hummock_client =
            HummockManagerServiceClient::new(channel.clone()).max_decoding_message_size(usize::MAX);
        let notification_client =
            NotificationServiceClient::new(channel.clone()).max_decoding_message_size(usize::MAX);
        let stream_client =
            StreamManagerServiceClient::new(channel.clone()).max_decoding_message_size(usize::MAX);
        let user_client = UserServiceClient::new(channel.clone());
        let scale_client =
            ScaleServiceClient::new(channel.clone()).max_decoding_message_size(usize::MAX);
        let backup_client = BackupServiceClient::new(channel.clone());
        let telemetry_client =
            TelemetryInfoServiceClient::new(channel.clone()).max_decoding_message_size(usize::MAX);
        let system_params_client = SystemParamsServiceClient::new(channel.clone());
        let serving_client = ServingServiceClient::new(channel.clone());
        let cloud_client = CloudServiceClient::new(channel.clone());
        let sink_coordinate_client = SinkCoordinationServiceClient::new(channel);

        GrpcMetaClientCore {
            cluster_client,
            meta_member_client,
            heartbeat_client,
            ddl_client,
            hummock_client,
            notification_client,
            stream_client,
            user_client,
            scale_client,
            backup_client,
            telemetry_client,
            system_params_client,
            serving_client,
            cloud_client,
            sink_coordinate_client,
        }
    }
}

/// Client to meta server. Cloning the instance is lightweight.
///
/// It is a wrapper of tonic client. See [`crate::rpc_client_method_impl`].
#[derive(Debug, Clone)]
struct GrpcMetaClient {
    member_monitor_event_sender: mpsc::Sender<Sender<Result<()>>>,
    core: Arc<RwLock<GrpcMetaClientCore>>,
}

#[derive(Debug, Eq, PartialEq)]
pub enum MetaAddressStrategy {
    LoadBalance(String),
    List(Vec<String>),
}

impl fmt::Display for MetaAddressStrategy {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            MetaAddressStrategy::LoadBalance(addr) => {
                write!(f, "LoadBalance({})", addr)?;
            }
            MetaAddressStrategy::List(addrs) => {
                write!(f, "List({:?})", addrs)?;
            }
        }
        Ok(())
    }
}

type MetaMemberClient = MetaMemberServiceClient<Channel>;

struct MetaMemberGroup {
    members: LruCache<String, Option<MetaMemberClient>>,
}

struct MetaMemberManagement {
    core_ref: Arc<RwLock<GrpcMetaClientCore>>,
    members: Either<MetaMemberClient, MetaMemberGroup>,
    current_leader: String,
    meta_config: MetaConfig,
}

impl MetaMemberManagement {
    const META_MEMBER_REFRESH_PERIOD: Duration = Duration::from_secs(5);

    fn host_address_to_url(addr: HostAddress) -> String {
        format!("http://{}:{}", addr.host, addr.port)
    }

    async fn recreate_core(&self, channel: Channel) {
        let mut core = self.core_ref.write().await;
        *core = GrpcMetaClientCore::new(channel);
    }

    async fn refresh_members(&mut self) -> Result<()> {
        let leader_addr = match self.members.as_mut() {
            Either::Left(client) => {
                let resp = client.to_owned().members(MembersRequest {}).await?;
                let resp = resp.into_inner();
                resp.members.into_iter().find(|member| member.is_leader)
            }
            Either::Right(member_group) => {
                let mut fetched_members = None;

                for (addr, client) in &mut member_group.members {
                    let client: Result<MetaMemberClient> = try {
                        match client {
                            Some(cached_client) => cached_client.to_owned(),
                            None => {
                                let endpoint = GrpcMetaClient::addr_to_endpoint(addr.clone())?;
                                let channel = GrpcMetaClient::connect_to_endpoint(endpoint).await?;
                                let new_client: MetaMemberClient =
                                    MetaMemberServiceClient::new(channel);
                                *client = Some(new_client.clone());

                                new_client
                            }
                        }
                    };
                    if let Err(err) = client {
                        tracing::warn!("failed to create client from {}: {}", addr, err);
                        continue;
                    }
                    match client.unwrap().members(MembersRequest {}).await {
                        Err(err) => {
                            tracing::warn!("failed to fetch members from {}: {}", addr, err);
                            continue;
                        }
                        Ok(resp) => {
                            fetched_members = Some(resp.into_inner().members);
                            break;
                        }
                    }
                }

                let members =
                    fetched_members.ok_or_else(|| anyhow!("could not refresh members"))?;

                // find new leader
                let mut leader = None;
                for member in members {
                    if member.is_leader {
                        leader = Some(member.clone());
                    }

                    let addr = Self::host_address_to_url(member.address.unwrap());
                    // We don't clean any expired addrs here to deal with some extreme situations.
                    if !member_group.members.contains(&addr) {
                        tracing::info!("new meta member joined: {}", addr);
                        member_group.members.put(addr, None);
                    }
                }

                leader
            }
        };

        if let Some(leader) = leader_addr {
            let discovered_leader = Self::host_address_to_url(leader.address.unwrap());

            if discovered_leader != self.current_leader {
                tracing::info!("new meta leader {} discovered", discovered_leader);

                let retry_strategy = GrpcMetaClient::retry_strategy_to_bound(
                    Duration::from_secs(self.meta_config.meta_leader_lease_secs),
                    false,
                );

                let channel = tokio_retry::Retry::spawn(retry_strategy, || async {
                    let endpoint = GrpcMetaClient::addr_to_endpoint(discovered_leader.clone())?;
                    GrpcMetaClient::connect_to_endpoint(endpoint).await
                })
                .await?;

                self.recreate_core(channel).await;
                self.current_leader = discovered_leader;
            }
        }

        Ok(())
    }
}

impl GrpcMetaClient {
    // See `Endpoint::http2_keep_alive_interval`
    const ENDPOINT_KEEP_ALIVE_INTERVAL_SEC: u64 = 60;
    // See `Endpoint::keep_alive_timeout`
    const ENDPOINT_KEEP_ALIVE_TIMEOUT_SEC: u64 = 60;
    // Retry base interval in ms for connecting to meta server.
    const INIT_RETRY_BASE_INTERVAL_MS: u64 = 50;
    // Max retry times for connecting to meta server.
    const INIT_RETRY_MAX_INTERVAL_MS: u64 = 5000;

    fn start_meta_member_monitor(
        &self,
        init_leader_addr: String,
        members: Either<MetaMemberClient, MetaMemberGroup>,
        force_refresh_receiver: Receiver<Sender<Result<()>>>,
        meta_config: MetaConfig,
    ) -> Result<()> {
        let core_ref: Arc<RwLock<GrpcMetaClientCore>> = self.core.clone();
        let current_leader = init_leader_addr;

        let enable_period_tick = matches!(members, Either::Right(_));

        let member_management = MetaMemberManagement {
            core_ref,
            members,
            current_leader,
            meta_config,
        };

        let mut force_refresh_receiver = force_refresh_receiver;

        tokio::spawn(async move {
            let mut member_management = member_management;
            let mut ticker = time::interval(MetaMemberManagement::META_MEMBER_REFRESH_PERIOD);

            loop {
                let event: Option<Sender<Result<()>>> = if enable_period_tick {
                    tokio::select! {
                        _ = ticker.tick() => None,
                        result_sender = force_refresh_receiver.recv() => {
                            if result_sender.is_none() {
                                break;
                            }

                            result_sender
                        },
                    }
                } else {
                    let result_sender = force_refresh_receiver.recv().await;

                    if result_sender.is_none() {
                        break;
                    }

                    result_sender
                };

                let tick_result = member_management.refresh_members().await;
                if let Err(e) = tick_result.as_ref() {
                    tracing::warn!("refresh meta member client failed {}", e);
                }

                if let Some(sender) = event {
                    // ignore resp
                    let _resp = sender.send(tick_result);
                }
            }
        });

        Ok(())
    }

    async fn force_refresh_leader(&self) -> Result<()> {
        let (sender, receiver) = oneshot::channel();

        self.member_monitor_event_sender
            .send(sender)
            .await
            .map_err(|e| anyhow!(e))?;

        receiver.await.map_err(|e| anyhow!(e))?
    }

    /// Connect to the meta server from `addrs`.
    pub async fn new(strategy: &MetaAddressStrategy, config: MetaConfig) -> Result<Self> {
        let (channel, addr) = match strategy {
            MetaAddressStrategy::LoadBalance(addr) => {
                Self::try_build_rpc_channel(vec![addr.clone()]).await
            }
            MetaAddressStrategy::List(addrs) => Self::try_build_rpc_channel(addrs.clone()).await,
        }?;
        let (force_refresh_sender, force_refresh_receiver) = mpsc::channel(1);
        let client = GrpcMetaClient {
            member_monitor_event_sender: force_refresh_sender,
            core: Arc::new(RwLock::new(GrpcMetaClientCore::new(channel))),
        };

        let meta_member_client = client.core.read().await.meta_member_client.clone();
        let members = match strategy {
            MetaAddressStrategy::LoadBalance(_) => Either::Left(meta_member_client),
            MetaAddressStrategy::List(addrs) => {
                let mut members = LruCache::new(NonZeroUsize::new(20).unwrap());
                for addr in addrs {
                    members.put(addr.clone(), None);
                }
                members.put(addr.clone(), Some(meta_member_client));

                Either::Right(MetaMemberGroup { members })
            }
        };

        client.start_meta_member_monitor(addr, members, force_refresh_receiver, config)?;

        client.force_refresh_leader().await?;

        Ok(client)
    }

    fn addr_to_endpoint(addr: String) -> Result<Endpoint> {
        let endpoint = Endpoint::from_shared(addr)?;
        Ok(endpoint.initial_connection_window_size(MAX_CONNECTION_WINDOW_SIZE))
    }

    pub(crate) async fn try_build_rpc_channel(addrs: Vec<String>) -> Result<(Channel, String)> {
        let endpoints: Vec<_> = addrs
            .into_iter()
            .map(|addr| Self::addr_to_endpoint(addr.clone()).map(|endpoint| (endpoint, addr)))
            .try_collect()?;

        let endpoints = endpoints.clone();

        for (endpoint, addr) in endpoints {
            match Self::connect_to_endpoint(endpoint).await {
                Ok(channel) => {
                    tracing::info!("Connect to meta server {} successfully", addr);
                    return Ok((channel, addr));
                }
                Err(e) => {
                    tracing::warn!(
                        "Failed to connect to meta server {}, trying again: {}",
                        addr,
                        e
                    )
                }
            }
        }

        Err(RpcError::Internal(anyhow!(
            "Failed to connect to meta server"
        )))
    }

    async fn connect_to_endpoint(endpoint: Endpoint) -> Result<Channel> {
        let channel = endpoint
            .http2_keep_alive_interval(Duration::from_secs(Self::ENDPOINT_KEEP_ALIVE_INTERVAL_SEC))
            .keep_alive_timeout(Duration::from_secs(Self::ENDPOINT_KEEP_ALIVE_TIMEOUT_SEC))
            .connect_timeout(Duration::from_secs(5))
            .connect()
            .await?
            .tracing_injected();

        Ok(channel)
    }

    pub(crate) fn retry_strategy_to_bound(
        high_bound: Duration,
        exceed: bool,
    ) -> impl Iterator<Item = Duration> {
        let iter = ExponentialBackoff::from_millis(Self::INIT_RETRY_BASE_INTERVAL_MS)
            .max_delay(Duration::from_millis(Self::INIT_RETRY_MAX_INTERVAL_MS))
            .map(jitter);

        let mut sum = Duration::default();

        iter.take_while(move |duration| {
            sum += *duration;

            if exceed {
                sum < high_bound + *duration
            } else {
                sum < high_bound
            }
        })
    }
}

macro_rules! for_all_meta_rpc {
    ($macro:ident) => {
        $macro! {
             { cluster_client, add_worker_node, AddWorkerNodeRequest, AddWorkerNodeResponse }
            ,{ cluster_client, activate_worker_node, ActivateWorkerNodeRequest, ActivateWorkerNodeResponse }
            ,{ cluster_client, delete_worker_node, DeleteWorkerNodeRequest, DeleteWorkerNodeResponse }
            ,{ cluster_client, update_worker_node_schedulability, UpdateWorkerNodeSchedulabilityRequest, UpdateWorkerNodeSchedulabilityResponse }
            //(not used) ,{ cluster_client, list_all_nodes, ListAllNodesRequest, ListAllNodesResponse }
            ,{ cluster_client, list_all_nodes, ListAllNodesRequest, ListAllNodesResponse }
            ,{ heartbeat_client, heartbeat, HeartbeatRequest, HeartbeatResponse }
            ,{ stream_client, flush, FlushRequest, FlushResponse }
            ,{ stream_client, pause, PauseRequest, PauseResponse }
            ,{ stream_client, resume, ResumeRequest, ResumeResponse }
            ,{ stream_client, cancel_creating_jobs, CancelCreatingJobsRequest, CancelCreatingJobsResponse }
            ,{ stream_client, list_table_fragments, ListTableFragmentsRequest, ListTableFragmentsResponse }
            ,{ stream_client, list_table_fragment_states, ListTableFragmentStatesRequest, ListTableFragmentStatesResponse }
            ,{ stream_client, list_fragment_distribution, ListFragmentDistributionRequest, ListFragmentDistributionResponse }
            ,{ stream_client, list_actor_states, ListActorStatesRequest, ListActorStatesResponse }
            ,{ ddl_client, create_table, CreateTableRequest, CreateTableResponse }
            ,{ ddl_client, alter_relation_name, AlterRelationNameRequest, AlterRelationNameResponse }
            ,{ ddl_client, create_materialized_view, CreateMaterializedViewRequest, CreateMaterializedViewResponse }
            ,{ ddl_client, create_view, CreateViewRequest, CreateViewResponse }
            ,{ ddl_client, create_source, CreateSourceRequest, CreateSourceResponse }
            ,{ ddl_client, create_sink, CreateSinkRequest, CreateSinkResponse }
            ,{ ddl_client, create_schema, CreateSchemaRequest, CreateSchemaResponse }
            ,{ ddl_client, create_database, CreateDatabaseRequest, CreateDatabaseResponse }
            ,{ ddl_client, create_index, CreateIndexRequest, CreateIndexResponse }
            ,{ ddl_client, create_function, CreateFunctionRequest, CreateFunctionResponse }
            ,{ ddl_client, drop_table, DropTableRequest, DropTableResponse }
            ,{ ddl_client, drop_materialized_view, DropMaterializedViewRequest, DropMaterializedViewResponse }
            ,{ ddl_client, drop_view, DropViewRequest, DropViewResponse }
            ,{ ddl_client, drop_source, DropSourceRequest, DropSourceResponse }
            ,{ ddl_client, drop_sink, DropSinkRequest, DropSinkResponse }
            ,{ ddl_client, drop_database, DropDatabaseRequest, DropDatabaseResponse }
            ,{ ddl_client, drop_schema, DropSchemaRequest, DropSchemaResponse }
            ,{ ddl_client, drop_index, DropIndexRequest, DropIndexResponse }
            ,{ ddl_client, drop_function, DropFunctionRequest, DropFunctionResponse }
            ,{ ddl_client, replace_table_plan, ReplaceTablePlanRequest, ReplaceTablePlanResponse }
            ,{ ddl_client, alter_source, AlterSourceRequest, AlterSourceResponse }
            ,{ ddl_client, risectl_list_state_tables, RisectlListStateTablesRequest, RisectlListStateTablesResponse }
            ,{ ddl_client, get_ddl_progress, GetDdlProgressRequest, GetDdlProgressResponse }
            ,{ ddl_client, create_connection, CreateConnectionRequest, CreateConnectionResponse }
            ,{ ddl_client, list_connections, ListConnectionsRequest, ListConnectionsResponse }
            ,{ ddl_client, drop_connection, DropConnectionRequest, DropConnectionResponse }
            ,{ ddl_client, get_tables, GetTablesRequest, GetTablesResponse }
            ,{ hummock_client, unpin_version_before, UnpinVersionBeforeRequest, UnpinVersionBeforeResponse }
            ,{ hummock_client, get_current_version, GetCurrentVersionRequest, GetCurrentVersionResponse }
            ,{ hummock_client, replay_version_delta, ReplayVersionDeltaRequest, ReplayVersionDeltaResponse }
            ,{ hummock_client, list_version_deltas, ListVersionDeltasRequest, ListVersionDeltasResponse }
            ,{ hummock_client, get_assigned_compact_task_num, GetAssignedCompactTaskNumRequest, GetAssignedCompactTaskNumResponse }
            ,{ hummock_client, trigger_compaction_deterministic, TriggerCompactionDeterministicRequest, TriggerCompactionDeterministicResponse }
            ,{ hummock_client, disable_commit_epoch, DisableCommitEpochRequest, DisableCommitEpochResponse }
            ,{ hummock_client, pin_snapshot, PinSnapshotRequest, PinSnapshotResponse }
            ,{ hummock_client, pin_specific_snapshot, PinSpecificSnapshotRequest, PinSnapshotResponse }
            ,{ hummock_client, get_epoch, GetEpochRequest, GetEpochResponse }
            ,{ hummock_client, unpin_snapshot, UnpinSnapshotRequest, UnpinSnapshotResponse }
            ,{ hummock_client, unpin_snapshot_before, UnpinSnapshotBeforeRequest, UnpinSnapshotBeforeResponse }
            ,{ hummock_client, get_new_sst_ids, GetNewSstIdsRequest, GetNewSstIdsResponse }
            ,{ hummock_client, report_vacuum_task, ReportVacuumTaskRequest, ReportVacuumTaskResponse }
            ,{ hummock_client, trigger_manual_compaction, TriggerManualCompactionRequest, TriggerManualCompactionResponse }
            ,{ hummock_client, report_full_scan_task, ReportFullScanTaskRequest, ReportFullScanTaskResponse }
            ,{ hummock_client, trigger_full_gc, TriggerFullGcRequest, TriggerFullGcResponse }
            ,{ hummock_client, rise_ctl_get_pinned_versions_summary, RiseCtlGetPinnedVersionsSummaryRequest, RiseCtlGetPinnedVersionsSummaryResponse }
            ,{ hummock_client, rise_ctl_get_pinned_snapshots_summary, RiseCtlGetPinnedSnapshotsSummaryRequest, RiseCtlGetPinnedSnapshotsSummaryResponse }
            ,{ hummock_client, rise_ctl_list_compaction_group, RiseCtlListCompactionGroupRequest, RiseCtlListCompactionGroupResponse }
            ,{ hummock_client, rise_ctl_update_compaction_config, RiseCtlUpdateCompactionConfigRequest, RiseCtlUpdateCompactionConfigResponse }
            ,{ hummock_client, rise_ctl_get_checkpoint_version, RiseCtlGetCheckpointVersionRequest, RiseCtlGetCheckpointVersionResponse }
            ,{ hummock_client, rise_ctl_pause_version_checkpoint, RiseCtlPauseVersionCheckpointRequest, RiseCtlPauseVersionCheckpointResponse }
            ,{ hummock_client, rise_ctl_resume_version_checkpoint, RiseCtlResumeVersionCheckpointRequest, RiseCtlResumeVersionCheckpointResponse }
            ,{ hummock_client, init_metadata_for_replay, InitMetadataForReplayRequest, InitMetadataForReplayResponse }
            ,{ hummock_client, split_compaction_group, SplitCompactionGroupRequest, SplitCompactionGroupResponse }
            ,{ hummock_client, rise_ctl_list_compaction_status, RiseCtlListCompactionStatusRequest, RiseCtlListCompactionStatusResponse }
            ,{ hummock_client, subscribe_compaction_event, impl tonic::IntoStreamingRequest<Message = SubscribeCompactionEventRequest>, Streaming<SubscribeCompactionEventResponse> }
            ,{ hummock_client, list_branched_object, ListBranchedObjectRequest, ListBranchedObjectResponse }
            ,{ hummock_client, list_active_write_limit, ListActiveWriteLimitRequest, ListActiveWriteLimitResponse }
            ,{ hummock_client, list_hummock_meta_config, ListHummockMetaConfigRequest, ListHummockMetaConfigResponse }
            ,{ user_client, create_user, CreateUserRequest, CreateUserResponse }
            ,{ user_client, update_user, UpdateUserRequest, UpdateUserResponse }
            ,{ user_client, drop_user, DropUserRequest, DropUserResponse }
            ,{ user_client, grant_privilege, GrantPrivilegeRequest, GrantPrivilegeResponse }
            ,{ user_client, revoke_privilege, RevokePrivilegeRequest, RevokePrivilegeResponse }
            ,{ scale_client, get_cluster_info, GetClusterInfoRequest, GetClusterInfoResponse }
            ,{ scale_client, reschedule, RescheduleRequest, RescheduleResponse }
            ,{ scale_client, get_reschedule_plan, GetReschedulePlanRequest, GetReschedulePlanResponse }
            ,{ notification_client, subscribe, SubscribeRequest, Streaming<SubscribeResponse> }
            ,{ backup_client, backup_meta, BackupMetaRequest, BackupMetaResponse }
            ,{ backup_client, get_backup_job_status, GetBackupJobStatusRequest, GetBackupJobStatusResponse }
            ,{ backup_client, delete_meta_snapshot, DeleteMetaSnapshotRequest, DeleteMetaSnapshotResponse}
            ,{ backup_client, get_meta_snapshot_manifest, GetMetaSnapshotManifestRequest, GetMetaSnapshotManifestResponse}
            ,{ telemetry_client, get_telemetry_info, GetTelemetryInfoRequest, TelemetryInfoResponse}
            ,{ system_params_client, get_system_params, GetSystemParamsRequest, GetSystemParamsResponse }
            ,{ system_params_client, set_system_param, SetSystemParamRequest, SetSystemParamResponse }
            ,{ serving_client, get_serving_vnode_mappings, GetServingVnodeMappingsRequest, GetServingVnodeMappingsResponse }
            ,{ cloud_client, rw_cloud_validate_source, RwCloudValidateSourceRequest, RwCloudValidateSourceResponse }
        }
    };
}

impl GrpcMetaClient {
    async fn refresh_client_if_needed(&self, code: Code) {
        if matches!(
            code,
            Code::Unknown | Code::Unimplemented | Code::Unavailable
        ) {
            tracing::debug!("matching tonic code {}", code);
            let (result_sender, result_receiver) = oneshot::channel();
            if self
                .member_monitor_event_sender
                .try_send(result_sender)
                .is_ok()
            {
                if let Ok(Err(e)) = result_receiver.await {
                    tracing::warn!("force refresh meta client failed {}", e);
                }
            } else {
                tracing::debug!("skipping the current refresh, somewhere else is already doing it")
            }
        }
    }
}

impl GrpcMetaClient {
    for_all_meta_rpc! { meta_rpc_client_method_impl }
}

#[cfg(test)]
mod tests {
    use crate::meta_client::MetaAddressStrategy;
    use crate::MetaClient;

    #[test]
    fn test_parse_meta_addr() {
        let results = vec![
            (
                "load-balance+http://abc",
                Some(MetaAddressStrategy::LoadBalance("http://abc".to_string())),
            ),
            ("load-balance+http://abc,http://def", None),
            ("load-balance+http://abc:xxx", None),
            ("", None),
            (
                "http://abc,http://def",
                Some(MetaAddressStrategy::List(vec![
                    "http://abc".to_string(),
                    "http://def".to_string(),
                ])),
            ),
            ("http://abc:xx,http://def", None),
        ];
        for (addr, result) in results {
            let parsed_result = MetaClient::parse_meta_addr(addr);
            match result {
                None => {
                    assert!(parsed_result.is_err());
                }
                Some(strategy) => {
                    assert_eq!(strategy, parsed_result.unwrap())
                }
            }
        }
    }
}
