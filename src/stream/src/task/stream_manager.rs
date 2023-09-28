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

use core::time::Duration;
use std::collections::{HashMap, HashSet};
use std::fmt::Debug;
use std::io::Write;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;

use anyhow::{anyhow, Context};
use async_recursion::async_recursion;
use futures::FutureExt;
use hytra::TrAdder;
use itertools::Itertools;
use risingwave_common::bail;
use risingwave_common::buffer::Bitmap;
use risingwave_common::catalog::{Field, Schema};
use risingwave_common::config::{MetricLevel, StreamingConfig};
use risingwave_common::util::addr::HostAddr;
use risingwave_common::util::runtime::BackgroundShutdownRuntime;
use risingwave_hummock_sdk::LocalSstableInfo;
use risingwave_pb::common::ActorInfo;
use risingwave_pb::stream_plan;
use risingwave_pb::stream_plan::barrier::BarrierKind;
use risingwave_pb::stream_plan::stream_node::NodeBody;
use risingwave_pb::stream_plan::StreamNode;
use risingwave_storage::monitor::HummockTraceFutureExt;
use risingwave_storage::{dispatch_state_store, StateStore, StateStoreImpl};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use super::{unique_executor_id, unique_operator_id, CollectResult};
use crate::error::StreamResult;
use crate::executor::exchange::permit::Receiver;
use crate::executor::monitor::StreamingMetrics;
use crate::executor::subtask::SubtaskHandle;
use crate::executor::*;
use crate::from_proto::create_executor;
use crate::task::{ActorId, FragmentId, SharedContext, StreamEnvironment, UpDownActorIds};

#[cfg(test)]
pub static LOCAL_TEST_ADDR: std::sync::LazyLock<HostAddr> =
    std::sync::LazyLock::new(|| "127.0.0.1:2333".parse().unwrap());

pub type ActorHandle = JoinHandle<()>;

pub type AtomicU64Ref = Arc<AtomicU64>;

pub struct LocalStreamManagerCore {
    /// Runtime for the streaming actors.
    runtime: BackgroundShutdownRuntime,

    /// Each processor runs in a future. Upon receiving a `Terminate` message, they will exit.
    /// `handles` store join handles of these futures, and therefore we could wait their
    /// termination.
    handles: HashMap<ActorId, ActorHandle>,

    pub(crate) context: Arc<SharedContext>,

    /// Stores all actor information, taken after actor built.
    actors: HashMap<ActorId, stream_plan::StreamActor>,

    /// Stores all actor tokio runtime monitoring tasks.
    actor_monitor_tasks: HashMap<ActorId, ActorHandle>,

    /// The state store implement
    state_store: StateStoreImpl,

    /// Metrics of the stream manager
    pub(crate) streaming_metrics: Arc<StreamingMetrics>,

    /// Config of streaming engine
    pub(crate) config: StreamingConfig,

    /// Manages the await-trees of all actors.
    await_tree_reg: Option<await_tree::Registry<ActorId>>,

    /// Watermark epoch number.
    watermark_epoch: AtomicU64Ref,

    total_mem_val: Arc<TrAdder<i64>>,
}

/// `LocalStreamManager` manages all stream executors in this project.
pub struct LocalStreamManager {
    core: Mutex<LocalStreamManagerCore>,

    // Maintain a copy of the core to reduce async locks
    state_store: StateStoreImpl,
    context: Arc<SharedContext>,
    streaming_metrics: Arc<StreamingMetrics>,

    total_mem_val: Arc<TrAdder<i64>>,
}

/// Report expression evaluation errors to the actor context.
///
/// The struct can be cheaply cloned.
#[derive(Clone)]
pub struct ActorEvalErrorReport {
    pub actor_context: ActorContextRef,
    pub identity: Arc<str>,
}

impl risingwave_expr::expr::EvalErrorReport for ActorEvalErrorReport {
    fn report(&self, err: risingwave_expr::ExprError) {
        self.actor_context.on_compute_error(err, &self.identity);
    }
}

pub struct ExecutorParams {
    pub env: StreamEnvironment,

    /// Indices of primary keys
    // TODO: directly use it for `ExecutorInfo`
    pub pk_indices: PkIndices,

    /// Executor id, unique across all actors.
    pub executor_id: u64,

    /// Operator id, unique for each operator in fragment.
    pub operator_id: u64,

    /// Information of the operator from plan node, like `StreamHashJoin { .. }`.
    // TODO: use it for `identity`
    pub op_info: String,

    /// The output schema of the executor.
    // TODO: directly use it for `ExecutorInfo`
    pub schema: Schema,

    /// The identity of the executor, like `HashJoin 1234ABCD`.
    // TODO: directly use it for `ExecutorInfo`
    pub identity: String,

    /// The input executor.
    pub input: Vec<BoxedExecutor>,

    /// FragmentId of the actor
    pub fragment_id: FragmentId,

    /// Metrics
    pub executor_stats: Arc<StreamingMetrics>,

    /// Actor context
    pub actor_context: ActorContextRef,

    /// Vnodes owned by this executor. Represented in bitmap.
    pub vnode_bitmap: Option<Bitmap>,

    /// Used for reporting expression evaluation errors.
    pub eval_error_report: ActorEvalErrorReport,
}

impl Debug for ExecutorParams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExecutorParams")
            .field("pk_indices", &self.pk_indices)
            .field("executor_id", &self.executor_id)
            .field("operator_id", &self.operator_id)
            .field("op_info", &self.op_info)
            .field("schema", &self.schema)
            .field("input", &self.input.len())
            .field("actor_id", &self.actor_context.id)
            .finish_non_exhaustive()
    }
}

impl LocalStreamManager {
    fn with_core(core: LocalStreamManagerCore) -> Self {
        Self {
            state_store: core.state_store.clone(),
            context: core.context.clone(),
            streaming_metrics: core.streaming_metrics.clone(),
            total_mem_val: core.total_mem_val.clone(),
            core: Mutex::new(core),
        }
    }

    pub fn new(
        addr: HostAddr,
        state_store: StateStoreImpl,
        streaming_metrics: Arc<StreamingMetrics>,
        config: StreamingConfig,
        await_tree_config: Option<await_tree::Config>,
    ) -> Self {
        Self::with_core(LocalStreamManagerCore::new(
            addr,
            state_store,
            streaming_metrics,
            config,
            await_tree_config,
        ))
    }

    #[cfg(test)]
    pub fn for_test() -> Self {
        Self::with_core(LocalStreamManagerCore::for_test())
    }

    /// Print the traces of all actors periodically, used for debugging only.
    pub fn spawn_print_trace(self: Arc<Self>) -> JoinHandle<!> {
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(5000)).await;
                let mut core = self.core.lock().await;
                let mut o = std::io::stdout().lock();

                for (k, trace) in core
                    .await_tree_reg
                    .as_mut()
                    .expect("async stack trace not enabled")
                    .iter()
                {
                    writeln!(o, ">> Actor {}\n\n{}", k, trace).ok();
                }
            }
        })
    }

    /// Get await-tree contexts for all actors.
    pub async fn get_actor_traces(&self) -> HashMap<ActorId, await_tree::TreeContext> {
        let core = self.core.lock().await;
        match &core.await_tree_reg {
            Some(mgr) => mgr.iter().map(|(k, v)| (*k, v)).collect(),
            None => Default::default(),
        }
    }

    /// Get all existing actor ids.
    pub async fn all_actor_ids(&self) -> HashSet<ActorId> {
        self.core.lock().await.handles.keys().cloned().collect()
    }

    /// Broadcast a barrier to all senders. Save a receiver in barrier manager
    pub async fn send_barrier(
        &self,
        barrier: &Barrier,
        actor_ids_to_send: impl IntoIterator<Item = ActorId>,
        actor_ids_to_collect: impl IntoIterator<Item = ActorId>,
    ) -> StreamResult<()> {
        let timer = self
            .streaming_metrics
            .barrier_inflight_latency
            .start_timer();
        if barrier.kind == BarrierKind::Initial {
            let core = self.core.lock().await;
            core.get_watermark_epoch()
                .store(barrier.epoch.curr, std::sync::atomic::Ordering::SeqCst);
        }
        let mut barrier_manager = self.context.lock_barrier_manager();
        barrier_manager.send_barrier(
            barrier,
            actor_ids_to_send,
            actor_ids_to_collect,
            Some(timer),
        )?;
        Ok(())
    }

    /// Reset the state of the barrier manager.
    pub fn reset_barrier_manager(&self) {
        self.context.lock_barrier_manager().reset();
    }

    /// Use `epoch` to find collect rx. And wait for all actor to be collected before
    /// returning.
    pub async fn collect_barrier(&self, epoch: u64) -> StreamResult<CollectResult> {
        let complete_receiver = {
            let mut barrier_manager = self.context.lock_barrier_manager();
            barrier_manager.remove_collect_rx(epoch)?
        };
        // Wait for all actors finishing this barrier.
        let result = complete_receiver
            .complete_receiver
            .expect("no rx for local mode")
            .await
            .context("failed to collect barrier")??;
        complete_receiver
            .barrier_inflight_timer
            .expect("no timer for test")
            .observe_duration();
        Ok(result)
    }

    pub async fn sync_epoch(&self, epoch: u64) -> StreamResult<Vec<LocalSstableInfo>> {
        let timer = self
            .core
            .lock()
            .await
            .streaming_metrics
            .barrier_sync_latency
            .start_timer();
        let res = dispatch_state_store!(self.state_store.clone(), store, {
            match store.sync(epoch).await {
                Ok(sync_result) => Ok(sync_result.uncommitted_ssts),
                Err(e) => {
                    tracing::error!(
                        "Failed to sync state store after receiving barrier prev_epoch {:?} due to {}",
                        epoch, e);
                    Err(e.into())
                }
            }
        });
        timer.observe_duration();
        res
    }

    pub async fn clear_storage_buffer(&self) {
        dispatch_state_store!(self.state_store.clone(), store, {
            store.clear_shared_buffer().await.unwrap();
        });
    }

    /// Broadcast a barrier to all senders. Returns immediately, and caller won't be notified when
    /// this barrier is finished.
    #[cfg(test)]
    pub fn send_barrier_for_test(&self, barrier: &Barrier) -> StreamResult<()> {
        use std::iter::empty;

        let mut barrier_manager = self.context.lock_barrier_manager();
        assert!(barrier_manager.is_local_mode());
        let timer = self
            .streaming_metrics
            .barrier_inflight_latency
            .start_timer();
        barrier_manager.send_barrier(barrier, empty(), empty(), Some(timer))?;
        barrier_manager.remove_collect_rx(barrier.epoch.prev)?;
        Ok(())
    }

    /// Drop the resources of the given actors.
    pub async fn drop_actors(&self, actors: &[ActorId]) -> StreamResult<()> {
        let mut core = self.core.lock().await;
        for &id in actors {
            core.drop_actor(id);
        }
        tracing::debug!(actors = ?actors, "drop actors");
        Ok(())
    }

    /// Force stop all actors on this worker, and then drop their resources.
    pub async fn stop_all_actors(&self) -> StreamResult<()> {
        self.core.lock().await.stop_all_actors().await;
        self.reset_barrier_manager();
        // Clear shared buffer in storage to release memory
        self.clear_storage_buffer().await;

        Ok(())
    }

    pub async fn take_receiver(&self, ids: UpDownActorIds) -> StreamResult<Receiver> {
        let core = self.core.lock().await;
        core.context.take_receiver(&ids)
    }

    pub async fn update_actors(&self, actors: &[stream_plan::StreamActor]) -> StreamResult<()> {
        let mut core = self.core.lock().await;
        core.update_actors(actors)
    }

    /// This function could only be called once during the lifecycle of `LocalStreamManager` for
    /// now.
    pub async fn update_actor_info(&self, actor_infos: &[ActorInfo]) -> StreamResult<()> {
        let mut core = self.core.lock().await;
        core.update_actor_info(actor_infos)
    }

    /// This function could only be called once during the lifecycle of `LocalStreamManager` for
    /// now.
    pub async fn build_actors(
        &self,
        actors: &[ActorId],
        env: StreamEnvironment,
    ) -> StreamResult<()> {
        let mut core = self.core.lock().await;
        core.build_actors(actors, env).await
    }

    pub async fn config(&self) -> StreamingConfig {
        let core = self.core.lock().await;
        core.config.clone()
    }

    /// After memory manager is created, it will store the watermark epoch in stream manager, so
    /// stream executor can get it to build managed cache.
    pub async fn set_watermark_epoch(&self, watermark_epoch: AtomicU64Ref) {
        let mut guard = self.core.lock().await;
        guard.watermark_epoch = watermark_epoch;
    }

    pub fn total_mem_usage(&self) -> usize {
        self.total_mem_val.get() as usize
    }
}

impl LocalStreamManagerCore {
    fn new(
        addr: HostAddr,
        state_store: StateStoreImpl,
        streaming_metrics: Arc<StreamingMetrics>,
        config: StreamingConfig,
        await_tree_config: Option<await_tree::Config>,
    ) -> Self {
        let context = SharedContext::new(addr, state_store.clone(), &config);
        Self::new_inner(
            state_store,
            context,
            streaming_metrics,
            config,
            await_tree_config,
        )
    }

    fn new_inner(
        state_store: StateStoreImpl,
        context: SharedContext,
        streaming_metrics: Arc<StreamingMetrics>,
        config: StreamingConfig,
        await_tree_config: Option<await_tree::Config>,
    ) -> Self {
        let runtime = {
            let mut builder = tokio::runtime::Builder::new_multi_thread();
            if let Some(worker_threads_num) = config.actor_runtime_worker_threads_num {
                builder.worker_threads(worker_threads_num);
            }
            builder
                .thread_name("risingwave-streaming-actor")
                .enable_all()
                .build()
                .unwrap()
        };

        Self {
            runtime: runtime.into(),
            handles: HashMap::new(),
            context: Arc::new(context),
            actors: HashMap::new(),
            actor_monitor_tasks: HashMap::new(),
            state_store,
            streaming_metrics,
            config,
            await_tree_reg: await_tree_config.map(await_tree::Registry::new),
            watermark_epoch: Arc::new(AtomicU64::new(0)),
            total_mem_val: Arc::new(TrAdder::new()),
        }
    }

    #[cfg(test)]
    fn for_test() -> Self {
        use risingwave_storage::monitor::MonitoredStorageMetrics;
        let streaming_metrics = Arc::new(StreamingMetrics::unused());
        Self::new_inner(
            StateStoreImpl::shared_in_memory_store(Arc::new(MonitoredStorageMetrics::unused())),
            SharedContext::for_test(),
            streaming_metrics,
            StreamingConfig::default(),
            None,
        )
    }

    /// Create dispatchers with downstream information registered before
    fn create_dispatcher(
        &mut self,
        input: BoxedExecutor,
        dispatchers: &[stream_plan::Dispatcher],
        actor_id: ActorId,
        fragment_id: FragmentId,
    ) -> StreamResult<DispatchExecutor> {
        let dispatcher_impls = dispatchers
            .iter()
            .map(|dispatcher| DispatcherImpl::new(&self.context, actor_id, dispatcher))
            .try_collect()?;

        Ok(DispatchExecutor::new(
            input,
            dispatcher_impls,
            actor_id,
            fragment_id,
            self.context.clone(),
            self.streaming_metrics.clone(),
        ))
    }

    /// Create a chain(tree) of nodes, with given `store`.
    // This is a clippy bug, see https://github.com/rust-lang/rust-clippy/issues/11380.
    // TODO: remove `allow` here after the issued is closed.
    #[expect(clippy::needless_pass_by_ref_mut)]
    #[allow(clippy::too_many_arguments)]
    #[async_recursion]
    async fn create_nodes_inner(
        &mut self,
        fragment_id: FragmentId,
        node: &stream_plan::StreamNode,
        input_pos: usize,
        env: StreamEnvironment,
        store: impl StateStore,
        actor_context: &ActorContextRef,
        vnode_bitmap: Option<Bitmap>,
        has_stateful: bool,
        subtasks: &mut Vec<SubtaskHandle>,
    ) -> StreamResult<BoxedExecutor> {
        // The "stateful" here means that the executor may issue read operations to the state store
        // massively and continuously. Used to decide whether to apply the optimization of subtasks.
        fn is_stateful_executor(stream_node: &StreamNode) -> bool {
            matches!(
                stream_node.get_node_body().unwrap(),
                NodeBody::HashAgg(_)
                    | NodeBody::HashJoin(_)
                    | NodeBody::DeltaIndexJoin(_)
                    | NodeBody::Lookup(_)
                    | NodeBody::Chain(_)
                    | NodeBody::DynamicFilter(_)
                    | NodeBody::GroupTopN(_)
                    | NodeBody::Now(_)
            )
        }
        let is_stateful = is_stateful_executor(node);

        // Create the input executor before creating itself
        let mut input = Vec::with_capacity(node.input.iter().len());
        for (input_pos, input_stream_node) in node.input.iter().enumerate() {
            input.push(
                self.create_nodes_inner(
                    fragment_id,
                    input_stream_node,
                    input_pos,
                    env.clone(),
                    store.clone(),
                    actor_context,
                    vnode_bitmap.clone(),
                    has_stateful || is_stateful,
                    subtasks,
                )
                .await?,
            );
        }

        let op_info = node.get_identity().clone();
        let pk_indices = node
            .get_stream_key()
            .iter()
            .map(|idx| *idx as usize)
            .collect::<Vec<_>>();

        // We assume that the operator_id of different instances from the same RelNode will be the
        // same.
        let executor_id = unique_executor_id(actor_context.id, node.operator_id);
        let operator_id = unique_operator_id(fragment_id, node.operator_id);
        let schema = node.fields.iter().map(Field::from).collect();

        let identity = format!("{} {:X}", node.get_node_body().unwrap(), executor_id);
        let eval_error_report = ActorEvalErrorReport {
            actor_context: actor_context.clone(),
            identity: identity.clone().into(),
        };

        // Build the executor with params.
        let executor_params = ExecutorParams {
            env: env.clone(),
            pk_indices,
            executor_id,
            operator_id,
            identity,
            op_info,
            schema,
            input,
            fragment_id,
            executor_stats: self.streaming_metrics.clone(),
            actor_context: actor_context.clone(),
            vnode_bitmap,
            eval_error_report,
        };

        let executor = create_executor(executor_params, self, node, store).await?;

        // Wrap the executor for debug purpose.
        let executor = WrapperExecutor::new(
            executor,
            input_pos,
            actor_context.id,
            executor_id,
            self.streaming_metrics.clone(),
            self.config.developer.enable_executor_row_count,
        )
        .boxed();

        // If there're multiple stateful executors in this actor, we will wrap it into a subtask.
        let executor = if has_stateful && is_stateful {
            // TODO(bugen): subtask does not work with tracing spans.
            // let (subtask, executor) = subtask::wrap(executor, actor_context.id);
            // subtasks.push(subtask);
            // executor.boxed()

            let _ = subtasks;
            executor
        } else {
            executor
        };

        Ok(executor)
    }

    /// Create a chain(tree) of nodes and return the head executor.
    async fn create_nodes(
        &mut self,
        fragment_id: FragmentId,
        node: &stream_plan::StreamNode,
        env: StreamEnvironment,
        actor_context: &ActorContextRef,
        vnode_bitmap: Option<Bitmap>,
    ) -> StreamResult<(BoxedExecutor, Vec<SubtaskHandle>)> {
        let mut subtasks = vec![];

        let executor = dispatch_state_store!(self.state_store.clone(), store, {
            self.create_nodes_inner(
                fragment_id,
                node,
                0,
                env,
                store,
                actor_context,
                vnode_bitmap,
                false,
                &mut subtasks,
            )
            .await
        })?;

        Ok((executor, subtasks))
    }

    async fn build_actors(
        &mut self,
        actors: &[ActorId],
        env: StreamEnvironment,
    ) -> StreamResult<()> {
        for &actor_id in actors {
            let actor = self
                .actors
                .remove(&actor_id)
                .ok_or_else(|| anyhow!("No such actor with actor id:{}", actor_id))?;
            let mview_definition = &actor.mview_definition;
            let actor_context = ActorContext::create_with_metrics(
                actor_id,
                actor.fragment_id,
                self.total_mem_val.clone(),
                self.streaming_metrics.clone(),
                self.config.unique_user_stream_errors,
            );
            let vnode_bitmap = actor
                .vnode_bitmap
                .as_ref()
                .map(|b| b.try_into())
                .transpose()
                .context("failed to decode vnode bitmap")?;

            let (executor, subtasks) = self
                .create_nodes(
                    actor.fragment_id,
                    actor.get_nodes()?,
                    env.clone(),
                    &actor_context,
                    vnode_bitmap,
                )
                // If hummock tracing is not enabled, it directly returns wrapped future.
                .may_trace_hummock()
                .await?;

            let dispatcher =
                self.create_dispatcher(executor, &actor.dispatcher, actor_id, actor.fragment_id)?;
            let actor = Actor::new(
                dispatcher,
                subtasks,
                self.context.clone(),
                self.streaming_metrics.clone(),
                actor_context.clone(),
            );

            let monitor = tokio_metrics::TaskMonitor::new();

            let handle = {
                let context = self.context.clone();
                let actor = async move {
                    if let Err(err) = actor.run().await {
                        // TODO: check error type and panic if it's unexpected.
                        tracing::error!(actor=%actor_id, error=%err, "actor exit");
                        context.lock_barrier_manager().notify_failure(actor_id, err);
                    }
                };
                let traced = match &mut self.await_tree_reg {
                    Some(m) => m
                        .register(
                            actor_id,
                            format!("Actor {actor_id}: `{}`", mview_definition),
                        )
                        .instrument(actor)
                        .left_future(),
                    None => actor.right_future(),
                };
                let instrumented = monitor.instrument(traced);
                #[cfg(enable_task_local_alloc)]
                {
                    let metrics = self.streaming_metrics.clone();
                    let actor_id_str = actor_id.to_string();
                    let allocation_stated = task_stats_alloc::allocation_stat(
                        instrumented,
                        Duration::from_millis(1000),
                        move |bytes| {
                            metrics
                                .actor_memory_usage
                                .with_label_values(&[&actor_id_str])
                                .set(bytes as i64);

                            actor_context.store_mem_usage(bytes);
                        },
                    );
                    self.runtime.spawn(allocation_stated)
                }
                #[cfg(not(enable_task_local_alloc))]
                {
                    self.runtime.spawn(instrumented)
                }
            };
            self.handles.insert(actor_id, handle);

            if self.streaming_metrics.level >= MetricLevel::Debug {
                tracing::info!("Tokio metrics are enabled because metrics_level >= Debug");
                let actor_id_str = actor_id.to_string();
                let metrics = self.streaming_metrics.clone();
                let actor_monitor_task = self.runtime.spawn(async move {
                    loop {
                        let task_metrics = monitor.cumulative();
                        metrics
                            .actor_execution_time
                            .with_label_values(&[&actor_id_str])
                            .set(task_metrics.total_poll_duration.as_secs_f64());
                        metrics
                            .actor_fast_poll_duration
                            .with_label_values(&[&actor_id_str])
                            .set(task_metrics.total_fast_poll_duration.as_secs_f64());
                        metrics
                            .actor_fast_poll_cnt
                            .with_label_values(&[&actor_id_str])
                            .set(task_metrics.total_fast_poll_count as i64);
                        metrics
                            .actor_slow_poll_duration
                            .with_label_values(&[&actor_id_str])
                            .set(task_metrics.total_slow_poll_duration.as_secs_f64());
                        metrics
                            .actor_slow_poll_cnt
                            .with_label_values(&[&actor_id_str])
                            .set(task_metrics.total_slow_poll_count as i64);
                        metrics
                            .actor_poll_duration
                            .with_label_values(&[&actor_id_str])
                            .set(task_metrics.total_poll_duration.as_secs_f64());
                        metrics
                            .actor_poll_cnt
                            .with_label_values(&[&actor_id_str])
                            .set(task_metrics.total_poll_count as i64);
                        metrics
                            .actor_idle_duration
                            .with_label_values(&[&actor_id_str])
                            .set(task_metrics.total_idle_duration.as_secs_f64());
                        metrics
                            .actor_idle_cnt
                            .with_label_values(&[&actor_id_str])
                            .set(task_metrics.total_idled_count as i64);
                        metrics
                            .actor_scheduled_duration
                            .with_label_values(&[&actor_id_str])
                            .set(task_metrics.total_scheduled_duration.as_secs_f64());
                        metrics
                            .actor_scheduled_cnt
                            .with_label_values(&[&actor_id_str])
                            .set(task_metrics.total_scheduled_count as i64);
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                });
                self.actor_monitor_tasks
                    .insert(actor_id, actor_monitor_task);
            }
        }

        Ok(())
    }

    pub fn take_all_handles(&mut self) -> StreamResult<HashMap<ActorId, ActorHandle>> {
        Ok(std::mem::take(&mut self.handles))
    }

    pub fn remove_actor_handles(
        &mut self,
        actor_ids: &[ActorId],
    ) -> StreamResult<Vec<ActorHandle>> {
        actor_ids
            .iter()
            .map(|actor_id| {
                self.handles
                    .remove(actor_id)
                    .ok_or_else(|| anyhow!("No such actor with actor id:{}", actor_id).into())
            })
            .try_collect()
    }

    fn update_actor_info(&mut self, new_actor_infos: &[ActorInfo]) -> StreamResult<()> {
        let mut actor_infos = self.context.actor_infos.write();
        for actor in new_actor_infos {
            let ret = actor_infos.insert(actor.get_actor_id(), actor.clone());
            if let Some(prev_actor) = ret && actor != &prev_actor {
                bail!(
                    "actor info mismatch when broadcasting {}",
                    actor.get_actor_id()
                );
            }
        }
        Ok(())
    }

    /// `drop_actor` is invoked by meta node via RPC once the stop barrier arrives at the
    /// sink. All the actors in the actors should stop themselves before this method is invoked.
    fn drop_actor(&mut self, actor_id: ActorId) {
        self.context.retain_channel(|&(up_id, _)| up_id != actor_id);
        self.actor_monitor_tasks
            .remove(&actor_id)
            .inspect(|handle| handle.abort());
        self.context.actor_infos.write().remove(&actor_id);
        self.actors.remove(&actor_id);

        // Task should have already stopped when this method is invoked. There might be some
        // clean-up work left (like dropping in-memory data structures), but we don't have to wait
        // for them to finish, in order to make this request non-blocking.
        self.handles.remove(&actor_id);
    }

    /// `stop_all_actors` is invoked by meta node via RPC for recovery purpose. Different from the
    /// `drop_actor`, the execution of the actors will be aborted.
    async fn stop_all_actors(&mut self) {
        for (actor_id, handle) in &self.handles {
            tracing::debug!("force stopping actor {}", actor_id);
            handle.abort();
        }
        for (actor_id, handle) in self.handles.drain() {
            tracing::debug!("join actor {}", actor_id);
            let result = handle.await;
            assert!(result.is_ok() || result.unwrap_err().is_cancelled());
        }
        self.actors.clear();
        self.context.clear_channels();
        if let Some(m) = self.await_tree_reg.as_mut() {
            m.clear();
        }
        self.actor_monitor_tasks.clear();
        self.context.actor_infos.write().clear();
    }

    fn update_actors(&mut self, actors: &[stream_plan::StreamActor]) -> StreamResult<()> {
        for actor in actors {
            self.actors
                .try_insert(actor.get_actor_id(), actor.clone())
                .map_err(|_| anyhow!("duplicated actor {}", actor.get_actor_id()))?;
        }

        Ok(())
    }

    /// When executor need to create cache, it will call this needs the watermark epoch for evict.
    pub fn get_watermark_epoch(&self) -> AtomicU64Ref {
        self.watermark_epoch.clone()
    }
}

#[cfg(test)]
pub mod test_utils {
    use risingwave_pb::common::HostAddress;

    use super::*;

    pub fn helper_make_local_actor(actor_id: u32) -> ActorInfo {
        ActorInfo {
            actor_id,
            host: Some(HostAddress {
                host: LOCAL_TEST_ADDR.host.clone(),
                port: LOCAL_TEST_ADDR.port as i32,
            }),
        }
    }
}
