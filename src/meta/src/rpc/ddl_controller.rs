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

use std::cmp::Ordering;
use std::num::NonZeroUsize;
use std::sync::Arc;

use itertools::Itertools;
use risingwave_common::config::DefaultParallelism;
use risingwave_common::hash::VirtualNode;
use risingwave_common::util::column_index_mapping::ColIndexMapping;
use risingwave_common::util::epoch::Epoch;
use risingwave_pb::catalog::connection::private_link_service::PbPrivateLinkProvider;
use risingwave_pb::catalog::{
    connection, Connection, CreateType, Database, Function, Schema, Source, Table, View,
};
use risingwave_pb::ddl_service::alter_relation_name_request::Relation;
use risingwave_pb::ddl_service::DdlProgress;
use risingwave_pb::stream_plan::StreamFragmentGraph as StreamFragmentGraphProto;
use tokio::sync::Semaphore;
use tracing::log::warn;
use tracing::Instrument;

use crate::barrier::BarrierManagerRef;
use crate::manager::{
    CatalogManagerRef, ClusterManagerRef, ConnectionId, DatabaseId, FragmentManagerRef, FunctionId,
    IdCategory, IndexId, LocalNotification, MetaSrvEnv, NotificationVersion, RelationIdEnum,
    SchemaId, SinkId, SourceId, StreamingClusterInfo, StreamingJob, TableId, ViewId,
    IGNORED_NOTIFICATION_VERSION,
};
use crate::model::{StreamEnvironment, TableFragments};
use crate::rpc::cloud_provider::AwsEc2Client;
use crate::stream::{
    validate_sink, ActorGraphBuildResult, ActorGraphBuilder, CompleteStreamFragmentGraph,
    CreateStreamingJobContext, GlobalStreamManagerRef, ReplaceTableContext, SourceManagerRef,
    StreamFragmentGraph,
};
use crate::{MetaError, MetaResult};

pub enum DropMode {
    Restrict,
    Cascade,
}

impl DropMode {
    pub fn from_request_setting(cascade: bool) -> DropMode {
        if cascade {
            DropMode::Cascade
        } else {
            DropMode::Restrict
        }
    }
}

pub enum StreamingJobId {
    MaterializedView(TableId),
    Sink(SinkId),
    Table(Option<SourceId>, TableId),
    Index(IndexId),
}

impl StreamingJobId {
    #[allow(dead_code)]
    fn id(&self) -> TableId {
        match self {
            StreamingJobId::MaterializedView(id)
            | StreamingJobId::Sink(id)
            | StreamingJobId::Table(_, id)
            | StreamingJobId::Index(id) => *id,
        }
    }
}

pub enum DdlCommand {
    CreateDatabase(Database),
    DropDatabase(DatabaseId),
    CreateSchema(Schema),
    DropSchema(SchemaId),
    CreateSource(Source),
    DropSource(SourceId, DropMode),
    CreateFunction(Function),
    DropFunction(FunctionId),
    CreateView(View),
    DropView(ViewId, DropMode),
    CreateStreamingJob(StreamingJob, StreamFragmentGraphProto, CreateType),
    DropStreamingJob(StreamingJobId, DropMode),
    ReplaceTable(StreamingJob, StreamFragmentGraphProto, ColIndexMapping),
    AlterRelationName(Relation, String),
    AlterSourceColumn(Source),
    CreateConnection(Connection),
    DropConnection(ConnectionId),
}

#[derive(Clone)]
pub struct DdlController {
    env: MetaSrvEnv,

    catalog_manager: CatalogManagerRef,
    stream_manager: GlobalStreamManagerRef,
    source_manager: SourceManagerRef,
    cluster_manager: ClusterManagerRef,
    fragment_manager: FragmentManagerRef,
    barrier_manager: BarrierManagerRef,

    aws_client: Arc<Option<AwsEc2Client>>,
    // The semaphore is used to limit the number of concurrent streaming job creation.
    creating_streaming_job_permits: Arc<CreatingStreamingJobPermit>,
}

#[derive(Clone)]
pub struct CreatingStreamingJobPermit {
    semaphore: Arc<Semaphore>,
}

impl CreatingStreamingJobPermit {
    async fn new(env: &MetaSrvEnv) -> Self {
        let mut permits = env
            .system_params_manager()
            .get_params()
            .await
            .max_concurrent_creating_streaming_jobs() as usize;
        if permits == 0 {
            // if the system parameter is set to zero, use the max permitted value.
            permits = Semaphore::MAX_PERMITS;
        }
        let semaphore = Arc::new(Semaphore::new(permits));

        let (local_notification_tx, mut local_notification_rx) =
            tokio::sync::mpsc::unbounded_channel();
        env.notification_manager()
            .insert_local_sender(local_notification_tx)
            .await;
        let semaphore_clone = semaphore.clone();
        tokio::spawn(async move {
            while let Some(notification) = local_notification_rx.recv().await {
                let LocalNotification::SystemParamsChange(p) = &notification else {
                    continue;
                };
                let mut new_permits = p.max_concurrent_creating_streaming_jobs() as usize;
                if new_permits == 0 {
                    new_permits = Semaphore::MAX_PERMITS;
                }
                match permits.cmp(&new_permits) {
                    Ordering::Less => {
                        semaphore_clone.add_permits(new_permits - permits);
                    }
                    Ordering::Equal => continue,
                    Ordering::Greater => {
                        semaphore_clone
                            .acquire_many((permits - new_permits) as u32)
                            .await
                            .unwrap()
                            .forget();
                    }
                }
                tracing::info!(
                    "max_concurrent_creating_streaming_jobs changed from {} to {}",
                    permits,
                    new_permits
                );
                permits = new_permits;
            }
        });

        Self { semaphore }
    }
}

impl DdlController {
    pub(crate) async fn new(
        env: MetaSrvEnv,
        catalog_manager: CatalogManagerRef,
        stream_manager: GlobalStreamManagerRef,
        source_manager: SourceManagerRef,
        cluster_manager: ClusterManagerRef,
        fragment_manager: FragmentManagerRef,
        barrier_manager: BarrierManagerRef,
        aws_client: Arc<Option<AwsEc2Client>>,
    ) -> Self {
        let creating_streaming_job_permits = Arc::new(CreatingStreamingJobPermit::new(&env).await);
        Self {
            env,
            catalog_manager,
            stream_manager,
            source_manager,
            cluster_manager,
            fragment_manager,
            barrier_manager,
            aws_client,
            creating_streaming_job_permits,
        }
    }

    /// `check_barrier_manager_status` checks the status of the barrier manager, return unavailable
    /// when it's not running.
    async fn check_barrier_manager_status(&self) -> MetaResult<()> {
        if !self.barrier_manager.is_running().await {
            return Err(MetaError::unavailable(
                "The cluster is starting or recovering".into(),
            ));
        }
        Ok(())
    }

    /// `run_command` spawns a tokio coroutine to execute the target ddl command. When the client
    /// has been interrupted during executing, the request will be cancelled by tonic. Since we have
    /// a lot of logic for revert, status management, notification and so on, ensuring consistency
    /// would be a huge hassle and pain if we don't spawn here.
    pub(crate) async fn run_command(&self, command: DdlCommand) -> MetaResult<NotificationVersion> {
        self.check_barrier_manager_status().await?;
        let ctrl = self.clone();
        let fut = async move {
            match command {
                DdlCommand::CreateDatabase(database) => ctrl.create_database(database).await,
                DdlCommand::DropDatabase(database_id) => ctrl.drop_database(database_id).await,
                DdlCommand::CreateSchema(schema) => ctrl.create_schema(schema).await,
                DdlCommand::DropSchema(schema_id) => ctrl.drop_schema(schema_id).await,
                DdlCommand::CreateSource(source) => ctrl.create_source(source).await,
                DdlCommand::DropSource(source_id, drop_mode) => {
                    ctrl.drop_source(source_id, drop_mode).await
                }
                DdlCommand::CreateFunction(function) => ctrl.create_function(function).await,
                DdlCommand::DropFunction(function_id) => ctrl.drop_function(function_id).await,
                DdlCommand::CreateView(view) => ctrl.create_view(view).await,
                DdlCommand::DropView(view_id, drop_mode) => {
                    ctrl.drop_view(view_id, drop_mode).await
                }
                DdlCommand::CreateStreamingJob(stream_job, fragment_graph, create_type) => {
                    ctrl.create_streaming_job(stream_job, fragment_graph, create_type)
                        .await
                }
                DdlCommand::DropStreamingJob(job_id, drop_mode) => {
                    ctrl.drop_streaming_job(job_id, drop_mode).await
                }
                DdlCommand::ReplaceTable(stream_job, fragment_graph, table_col_index_mapping) => {
                    ctrl.replace_table(stream_job, fragment_graph, table_col_index_mapping)
                        .await
                }
                DdlCommand::AlterRelationName(relation, name) => {
                    ctrl.alter_relation_name(relation, &name).await
                }
                DdlCommand::CreateConnection(connection) => {
                    ctrl.create_connection(connection).await
                }
                DdlCommand::DropConnection(connection_id) => {
                    ctrl.drop_connection(connection_id).await
                }
                DdlCommand::AlterSourceColumn(source) => ctrl.alter_source_column(source).await,
            }
        }
        .in_current_span();
        tokio::spawn(fut).await.unwrap()
    }

    pub(crate) async fn get_ddl_progress(&self) -> Vec<DdlProgress> {
        self.barrier_manager.get_ddl_progress().await
    }

    async fn create_database(&self, database: Database) -> MetaResult<NotificationVersion> {
        self.catalog_manager.create_database(&database).await
    }

    async fn drop_database(&self, database_id: DatabaseId) -> MetaResult<NotificationVersion> {
        // 1. drop all catalogs in this database.
        let (version, streaming_ids, source_ids, connections_dropped) =
            self.catalog_manager.drop_database(database_id).await?;
        // 2. Unregister source connector worker.
        self.source_manager.unregister_sources(source_ids).await;
        // 3. drop streaming jobs.
        if !streaming_ids.is_empty() {
            self.stream_manager.drop_streaming_jobs(streaming_ids).await;
        }
        // 4. delete cloud resources if any
        for conn in connections_dropped {
            self.delete_vpc_endpoint(&conn).await?;
        }

        Ok(version)
    }

    async fn create_schema(&self, schema: Schema) -> MetaResult<NotificationVersion> {
        self.catalog_manager.create_schema(&schema).await
    }

    async fn drop_schema(&self, schema_id: SchemaId) -> MetaResult<NotificationVersion> {
        self.catalog_manager.drop_schema(schema_id).await
    }

    async fn create_source(&self, mut source: Source) -> MetaResult<NotificationVersion> {
        // set the initialized_at_epoch to the current epoch.
        source.initialized_at_epoch = Some(Epoch::now().0);

        self.catalog_manager
            .start_create_source_procedure(&source)
            .await?;

        if let Err(e) = self.source_manager.register_source(&source).await {
            self.catalog_manager
                .cancel_create_source_procedure(&source)
                .await?;
            return Err(e);
        }

        self.catalog_manager
            .finish_create_source_procedure(source)
            .await
    }

    async fn drop_source(
        &self,
        source_id: SourceId,
        drop_mode: DropMode,
    ) -> MetaResult<NotificationVersion> {
        // 1. Drop source in catalog.
        let version = self
            .catalog_manager
            .drop_relation(
                RelationIdEnum::Source(source_id),
                self.fragment_manager.clone(),
                drop_mode,
            )
            .await?
            .0;
        // 2. Unregister source connector worker.
        self.source_manager
            .unregister_sources(vec![source_id])
            .await;

        Ok(version)
    }

    // Maybe we can unify `alter_source_column` and `alter_source_name`.
    async fn alter_source_column(&self, source: Source) -> MetaResult<NotificationVersion> {
        self.catalog_manager.alter_source_column(source).await
    }

    async fn create_function(&self, function: Function) -> MetaResult<NotificationVersion> {
        self.catalog_manager.create_function(&function).await
    }

    async fn drop_function(&self, function_id: FunctionId) -> MetaResult<NotificationVersion> {
        self.catalog_manager.drop_function(function_id).await
    }

    async fn create_view(&self, view: View) -> MetaResult<NotificationVersion> {
        self.catalog_manager.create_view(&view).await
    }

    async fn drop_view(
        &self,
        view_id: ViewId,
        drop_mode: DropMode,
    ) -> MetaResult<NotificationVersion> {
        let (version, streaming_job_ids) = self
            .catalog_manager
            .drop_relation(
                RelationIdEnum::View(view_id),
                self.fragment_manager.clone(),
                drop_mode,
            )
            .await?;
        self.stream_manager
            .drop_streaming_jobs(streaming_job_ids)
            .await;
        Ok(version)
    }

    async fn create_connection(&self, connection: Connection) -> MetaResult<NotificationVersion> {
        self.catalog_manager.create_connection(connection).await
    }

    async fn drop_connection(
        &self,
        connection_id: ConnectionId,
    ) -> MetaResult<NotificationVersion> {
        let (version, connection) = self.catalog_manager.drop_connection(connection_id).await?;
        self.delete_vpc_endpoint(&connection).await?;
        Ok(version)
    }

    async fn delete_vpc_endpoint(&self, connection: &Connection) -> MetaResult<()> {
        // delete AWS vpc endpoint
        if let Some(connection::Info::PrivateLinkService(svc)) = &connection.info
            && svc.get_provider()? == PbPrivateLinkProvider::Aws {
            if let Some(aws_cli) = self.aws_client.as_ref() {
                aws_cli.delete_vpc_endpoint(&svc.endpoint_id).await?;
            } else {
                warn!("AWS client is not initialized, skip deleting vpc endpoint {}", svc.endpoint_id);
            }
        }
        Ok(())
    }

    async fn create_streaming_job(
        &self,
        mut stream_job: StreamingJob,
        fragment_graph: StreamFragmentGraphProto,
        create_type: CreateType,
    ) -> MetaResult<NotificationVersion> {
        let _permit = self
            .creating_streaming_job_permits
            .semaphore
            .acquire()
            .await
            .unwrap();
        let _reschedule_job_lock = self.stream_manager.reschedule_lock.read().await;

        let env = StreamEnvironment::from_protobuf(fragment_graph.get_env().unwrap());
        let fragment_graph = self
            .prepare_stream_job(&mut stream_job, fragment_graph)
            .await?;

        // Update the corresponding 'initiated_at' field.
        stream_job.mark_initialized();

        let mut internal_tables = vec![];
        let result = try {
            let (ctx, table_fragments) = self
                .build_stream_job(env, &stream_job, fragment_graph)
                .await?;

            internal_tables = ctx.internal_tables();

            match stream_job {
                StreamingJob::Table(Some(ref source), _) => {
                    // Register the source on the connector node.
                    self.source_manager.register_source(source).await?;
                }
                StreamingJob::Sink(ref sink) => {
                    // Validate the sink on the connector node.
                    validate_sink(sink).await?;
                }
                _ => {}
            }
            (ctx, table_fragments)
        };

        let (ctx, table_fragments) = match result {
            Ok(r) => r,
            Err(e) => {
                self.cancel_stream_job(&stream_job, internal_tables).await;
                return Err(e);
            }
        };

        match create_type {
            CreateType::Foreground | CreateType::Unspecified => {
                self.create_streaming_job_inner(stream_job, table_fragments, ctx, internal_tables)
                    .await
            }
            CreateType::Background => {
                let ctrl = self.clone();
                let definition = stream_job.definition();
                let fut = async move {
                    let result = ctrl
                        .create_streaming_job_inner(
                            stream_job,
                            table_fragments,
                            ctx,
                            internal_tables,
                        )
                        .await;
                    match result {
                        Err(e) => tracing::error!(definition, error = ?e, "stream_job_error"),
                        Ok(_) => {
                            tracing::info!(definition, "stream_job_ok")
                        }
                    }
                };
                tokio::spawn(fut);
                Ok(IGNORED_NOTIFICATION_VERSION)
            }
        }
    }

    async fn create_streaming_job_inner(
        &self,
        stream_job: StreamingJob,
        table_fragments: TableFragments,
        ctx: CreateStreamingJobContext,
        internal_tables: Vec<Table>,
    ) -> MetaResult<NotificationVersion> {
        let result = self
            .stream_manager
            .create_streaming_job(table_fragments, ctx)
            .await;
        if let Err(e) = result {
            self.cancel_stream_job(&stream_job, internal_tables).await;
            return Err(e);
        };
        self.finish_stream_job(stream_job, internal_tables).await
    }

    async fn drop_streaming_job(
        &self,
        job_id: StreamingJobId,
        drop_mode: DropMode,
    ) -> MetaResult<NotificationVersion> {
        let _reschedule_job_lock = self.stream_manager.reschedule_lock.read().await;
        let (version, streaming_job_ids) = match job_id {
            StreamingJobId::MaterializedView(table_id) => {
                self.catalog_manager
                    .drop_relation(
                        RelationIdEnum::Table(table_id),
                        self.fragment_manager.clone(),
                        drop_mode,
                    )
                    .await?
            }
            StreamingJobId::Sink(sink_id) => {
                self.catalog_manager
                    .drop_relation(
                        RelationIdEnum::Sink(sink_id),
                        self.fragment_manager.clone(),
                        drop_mode,
                    )
                    .await?
            }
            StreamingJobId::Table(source_id, table_id) => {
                self.drop_table_inner(
                    source_id,
                    table_id,
                    self.fragment_manager.clone(),
                    drop_mode,
                )
                .await?
            }
            StreamingJobId::Index(index_id) => {
                self.catalog_manager
                    .drop_relation(
                        RelationIdEnum::Index(index_id),
                        self.fragment_manager.clone(),
                        drop_mode,
                    )
                    .await?
            }
        };

        self.stream_manager
            .drop_streaming_jobs(streaming_job_ids)
            .await;
        Ok(version)
    }

    /// `prepare_stream_job` prepares a stream job and returns the stream fragment graph.
    async fn prepare_stream_job(
        &self,
        stream_job: &mut StreamingJob,
        fragment_graph: StreamFragmentGraphProto,
    ) -> MetaResult<StreamFragmentGraph> {
        // 1. Build fragment graph.
        let fragment_graph =
            StreamFragmentGraph::new(fragment_graph, self.env.id_gen_manager_ref(), stream_job)
                .await?;

        // 2. Set the graph-related fields and freeze the `stream_job`.
        stream_job.set_table_fragment_id(fragment_graph.table_fragment_id());
        stream_job.set_dml_fragment_id(fragment_graph.dml_fragment_id());
        let stream_job = &*stream_job;

        // 3. Mark current relation as "creating" and add reference count to dependent relations.
        self.catalog_manager
            .start_create_stream_job_procedure(stream_job)
            .await?;

        Ok(fragment_graph)
    }

    fn resolve_stream_parallelism(
        &self,
        default_parallelism: Option<NonZeroUsize>,
        cluster_info: &StreamingClusterInfo,
    ) -> MetaResult<NonZeroUsize> {
        if cluster_info.parallel_units.is_empty() {
            return Err(MetaError::unavailable(
                "No available parallel units to schedule".to_string(),
            ));
        }

        let available_parallel_units =
            NonZeroUsize::new(cluster_info.parallel_units.len()).unwrap();
        // Use configured parallel units if no default parallelism is specified.
        let parallelism = default_parallelism.unwrap_or(match &self.env.opts.default_parallelism {
            DefaultParallelism::Full => {
                if available_parallel_units.get() > VirtualNode::COUNT {
                    tracing::warn!(
                        "Too many parallel units, use {} instead",
                        VirtualNode::COUNT
                    );
                    NonZeroUsize::new(VirtualNode::COUNT).unwrap()
                } else {
                    available_parallel_units
                }
            }
            DefaultParallelism::Default(num) => *num,
        });

        if parallelism > available_parallel_units {
            return Err(MetaError::unavailable(format!(
                "Not enough parallel units to schedule, required: {}, available: {}",
                parallelism, available_parallel_units
            )));
        }

        Ok(parallelism)
    }

    /// `build_stream_job` builds a streaming job and returns the context and table fragments.
    async fn build_stream_job(
        &self,
        env: StreamEnvironment,
        stream_job: &StreamingJob,
        fragment_graph: StreamFragmentGraph,
    ) -> MetaResult<(CreateStreamingJobContext, TableFragments)> {
        let id = stream_job.id();
        let default_parallelism = fragment_graph.default_parallelism();
        let internal_tables = fragment_graph.internal_tables();

        // 1. Resolve the upstream fragments, extend the fragment graph to a complete graph that
        // contains all information needed for building the actor graph.
        let upstream_mview_fragments = self
            .fragment_manager
            .get_upstream_mview_fragments(fragment_graph.dependent_table_ids())
            .await?;
        let upstream_mview_actors = upstream_mview_fragments
            .iter()
            .map(|(&table_id, fragment)| {
                (
                    table_id,
                    fragment.actors.iter().map(|a| a.actor_id).collect_vec(),
                )
            })
            .collect();

        let complete_graph =
            CompleteStreamFragmentGraph::with_upstreams(fragment_graph, upstream_mview_fragments)?;

        // 2. Build the actor graph.
        let cluster_info = self.cluster_manager.get_streaming_cluster_info().await;
        let default_parallelism =
            self.resolve_stream_parallelism(default_parallelism, &cluster_info)?;

        let actor_graph_builder =
            ActorGraphBuilder::new(complete_graph, cluster_info, default_parallelism)?;

        let ActorGraphBuildResult {
            graph,
            building_locations,
            existing_locations,
            dispatchers,
            merge_updates,
        } = actor_graph_builder
            .generate_graph(self.env.id_gen_manager_ref(), stream_job)
            .await?;
        assert!(merge_updates.is_empty());

        // 3. Build the table fragments structure that will be persisted in the stream manager,
        // and the context that contains all information needed for building the
        // actors on the compute nodes.
        let table_fragments =
            TableFragments::new(id.into(), graph, &building_locations.actor_locations, env);

        let ctx = CreateStreamingJobContext {
            dispatchers,
            upstream_mview_actors,
            internal_tables,
            building_locations,
            existing_locations,
            table_properties: stream_job.properties(),
            definition: stream_job.definition(),
            mv_table_id: stream_job.mv_table(),
        };

        // 4. Mark creating tables, including internal tables and the table of the stream job.
        let creating_tables = ctx
            .internal_tables()
            .into_iter()
            .chain(stream_job.table().cloned())
            .collect_vec();

        self.catalog_manager
            .mark_creating_tables(&creating_tables)
            .await;

        Ok((ctx, table_fragments))
    }

    /// `cancel_stream_job` cancels a stream job and clean some states.
    async fn cancel_stream_job(&self, stream_job: &StreamingJob, internal_tables: Vec<Table>) {
        let mut creating_internal_table_ids =
            internal_tables.into_iter().map(|t| t.id).collect_vec();
        // 1. cancel create procedure.
        match stream_job {
            StreamingJob::MaterializedView(table) => {
                creating_internal_table_ids.push(table.id);
                self.catalog_manager
                    .cancel_create_table_procedure(table)
                    .await;
            }
            StreamingJob::Sink(sink) => {
                self.catalog_manager
                    .cancel_create_sink_procedure(sink)
                    .await;
            }
            StreamingJob::Table(source, table) => {
                creating_internal_table_ids.push(table.id);
                if let Some(source) = source {
                    self.catalog_manager
                        .cancel_create_table_procedure_with_source(source, table)
                        .await;
                } else {
                    self.catalog_manager
                        .cancel_create_table_procedure(table)
                        .await;
                }
            }
            StreamingJob::Index(index, table) => {
                creating_internal_table_ids.push(table.id);
                self.catalog_manager
                    .cancel_create_index_procedure(index, table)
                    .await;
            }
        }
        // 2. unmark creating tables.
        self.catalog_manager
            .unmark_creating_tables(&creating_internal_table_ids, true)
            .await;
    }

    /// `finish_stream_job` finishes a stream job and clean some states.
    async fn finish_stream_job(
        &self,
        mut stream_job: StreamingJob,
        internal_tables: Vec<Table>,
    ) -> MetaResult<u64> {
        // 1. finish procedure.
        let mut creating_internal_table_ids = internal_tables.iter().map(|t| t.id).collect_vec();

        // Update the corresponding 'created_at' field.
        stream_job.mark_created();

        let version = match stream_job {
            StreamingJob::MaterializedView(table) => {
                creating_internal_table_ids.push(table.id);
                self.catalog_manager
                    .finish_create_table_procedure(internal_tables, table)
                    .await?
            }
            StreamingJob::Sink(sink) => {
                self.catalog_manager
                    .finish_create_sink_procedure(internal_tables, sink)
                    .await?
            }
            StreamingJob::Table(source, table) => {
                creating_internal_table_ids.push(table.id);
                if let Some(source) = source {
                    self.catalog_manager
                        .finish_create_table_procedure_with_source(source, table, internal_tables)
                        .await?
                } else {
                    self.catalog_manager
                        .finish_create_table_procedure(internal_tables, table)
                        .await?
                }
            }
            StreamingJob::Index(index, table) => {
                creating_internal_table_ids.push(table.id);
                self.catalog_manager
                    .finish_create_index_procedure(internal_tables, index, table)
                    .await?
            }
        };

        // 2. unmark creating tables.
        self.catalog_manager
            .unmark_creating_tables(&creating_internal_table_ids, false)
            .await;

        Ok(version)
    }

    async fn drop_table_inner(
        &self,
        source_id: Option<SourceId>,
        table_id: TableId,
        fragment_manager: FragmentManagerRef,
        drop_mode: DropMode,
    ) -> MetaResult<(
        NotificationVersion,
        Vec<risingwave_common::catalog::TableId>,
    )> {
        if let Some(source_id) = source_id {
            // Drop table and source in catalog. Check `source_id` if it is the table's
            // `associated_source_id`. Indexes also need to be dropped atomically.
            let (version, delete_jobs) = self
                .catalog_manager
                .drop_relation(
                    RelationIdEnum::Table(table_id),
                    fragment_manager.clone(),
                    drop_mode,
                )
                .await?;
            // Unregister source connector worker.
            self.source_manager
                .unregister_sources(vec![source_id])
                .await;
            Ok((version, delete_jobs))
        } else {
            self.catalog_manager
                .drop_relation(RelationIdEnum::Table(table_id), fragment_manager, drop_mode)
                .await
        }
    }

    async fn replace_table(
        &self,
        mut stream_job: StreamingJob,
        fragment_graph: StreamFragmentGraphProto,
        table_col_index_mapping: ColIndexMapping,
    ) -> MetaResult<NotificationVersion> {
        let _reschedule_job_lock = self.stream_manager.reschedule_lock.read().await;
        let env = StreamEnvironment::from_protobuf(fragment_graph.get_env().unwrap());

        let fragment_graph = self
            .prepare_replace_table(&mut stream_job, fragment_graph)
            .await?;

        let result = try {
            let (ctx, table_fragments) = self
                .build_replace_table(
                    env,
                    &stream_job,
                    fragment_graph,
                    table_col_index_mapping.clone(),
                )
                .await?;

            self.stream_manager
                .replace_table(table_fragments, ctx)
                .await?;
        };

        match result {
            Ok(_) => {
                self.finish_replace_table(&stream_job, table_col_index_mapping)
                    .await
            }
            Err(err) => {
                self.cancel_replace_table(&stream_job).await?;
                Err(err)
            }
        }
    }

    /// `prepare_replace_table` prepares a table replacement and returns the new stream fragment
    /// graph. This is basically the same as `prepare_stream_job`, except that it does more
    /// assertions and uses a different method to mark in the catalog.
    async fn prepare_replace_table(
        &self,
        stream_job: &mut StreamingJob,
        fragment_graph: StreamFragmentGraphProto,
    ) -> MetaResult<StreamFragmentGraph> {
        // 1. Build fragment graph.
        let fragment_graph =
            StreamFragmentGraph::new(fragment_graph, self.env.id_gen_manager_ref(), stream_job)
                .await?;

        // 2. Set the graph-related fields and freeze the `stream_job`.
        stream_job.set_table_fragment_id(fragment_graph.table_fragment_id());
        stream_job.set_dml_fragment_id(fragment_graph.dml_fragment_id());
        let stream_job = &*stream_job;

        // 3. Mark current relation as "updating".
        self.catalog_manager
            .start_replace_table_procedure(stream_job)
            .await?;

        Ok(fragment_graph)
    }

    /// `build_replace_table` builds a table replacement and returns the context and new table
    /// fragments.
    async fn build_replace_table(
        &self,
        env: StreamEnvironment,
        stream_job: &StreamingJob,
        mut fragment_graph: StreamFragmentGraph,
        table_col_index_mapping: ColIndexMapping,
    ) -> MetaResult<(ReplaceTableContext, TableFragments)> {
        let id = stream_job.id();
        let default_parallelism = fragment_graph.default_parallelism();

        let old_table_fragments = self
            .fragment_manager
            .select_table_fragments_by_table_id(&id.into())
            .await?;
        let old_internal_table_ids = old_table_fragments.internal_table_ids();
        let old_internal_tables = self
            .catalog_manager
            .get_tables(&old_internal_table_ids)
            .await;

        fragment_graph.fit_internal_table_ids(old_internal_tables)?;

        // 1. Resolve the edges to the downstream fragments, extend the fragment graph to a complete
        // graph that contains all information needed for building the actor graph.
        let original_table_fragment = self.fragment_manager.get_mview_fragment(id.into()).await?;

        // Map the column indices in the dispatchers with the given mapping.
        let downstream_fragments = self
            .fragment_manager
            .get_downstream_chain_fragments(id.into())
            .await?
            .into_iter()
            .map(|(d, f)| Some((table_col_index_mapping.rewrite_dispatch_strategy(&d)?, f)))
            .collect::<Option<_>>()
            .ok_or_else(|| {
                // The `rewrite` only fails if some column is dropped.
                MetaError::invalid_parameter(
                    "unable to drop the column due to being referenced by downstream materialized views or sinks",
                )
            })?;

        let complete_graph = CompleteStreamFragmentGraph::with_downstreams(
            fragment_graph,
            original_table_fragment.fragment_id,
            downstream_fragments,
        )?;

        // 2. Build the actor graph.
        let cluster_info = self.cluster_manager.get_streaming_cluster_info().await;
        let default_parallelism =
            self.resolve_stream_parallelism(default_parallelism, &cluster_info)?;
        let actor_graph_builder =
            ActorGraphBuilder::new(complete_graph, cluster_info, default_parallelism)?;

        let ActorGraphBuildResult {
            graph,
            building_locations,
            existing_locations,
            dispatchers,
            merge_updates,
        } = actor_graph_builder
            .generate_graph(self.env.id_gen_manager_ref(), stream_job)
            .await?;
        assert!(dispatchers.is_empty());

        // 3. Assign a new dummy ID for the new table fragments.
        //
        // FIXME: we use a dummy table ID for new table fragments, so we can drop the old fragments
        // with the real table ID, then replace the dummy table ID with the real table ID. This is a
        // workaround for not having the version info in the fragment manager.
        let dummy_id = self
            .env
            .id_gen_manager()
            .generate::<{ IdCategory::Table }>()
            .await? as u32;

        // 4. Build the table fragments structure that will be persisted in the stream manager, and
        // the context that contains all information needed for building the actors on the compute
        // nodes.
        let table_fragments = TableFragments::new(
            dummy_id.into(),
            graph,
            &building_locations.actor_locations,
            env,
        );

        let ctx = ReplaceTableContext {
            old_table_fragments,
            merge_updates,
            dispatchers,
            building_locations,
            existing_locations,
            table_properties: stream_job.properties(),
        };

        Ok((ctx, table_fragments))
    }

    async fn finish_replace_table(
        &self,
        stream_job: &StreamingJob,
        table_col_index_mapping: ColIndexMapping,
    ) -> MetaResult<NotificationVersion> {
        let StreamingJob::Table(source, table) = stream_job else {
            unreachable!("unexpected job: {stream_job:?}")
        };

        self.catalog_manager
            .finish_replace_table_procedure(source, table, table_col_index_mapping)
            .await
    }

    async fn cancel_replace_table(&self, stream_job: &StreamingJob) -> MetaResult<()> {
        self.catalog_manager
            .cancel_replace_table_procedure(stream_job)
            .await
    }

    async fn alter_relation_name(
        &self,
        relation: Relation,
        new_name: &str,
    ) -> MetaResult<NotificationVersion> {
        match relation {
            Relation::TableId(table_id) => {
                self.catalog_manager
                    .alter_table_name(table_id, new_name)
                    .await
            }
            Relation::ViewId(view_id) => {
                self.catalog_manager
                    .alter_view_name(view_id, new_name)
                    .await
            }
            Relation::IndexId(index_id) => {
                self.catalog_manager
                    .alter_index_name(index_id, new_name)
                    .await
            }
            Relation::SinkId(sink_id) => {
                self.catalog_manager
                    .alter_sink_name(sink_id, new_name)
                    .await
            }
            Relation::SourceId(source_id) => {
                self.catalog_manager
                    .alter_source_name(source_id, new_name)
                    .await
            }
        }
    }
}
