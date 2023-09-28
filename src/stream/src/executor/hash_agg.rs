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

use std::collections::{HashMap, HashSet};
use std::marker::PhantomData;
use std::sync::Arc;

use futures::{stream, StreamExt};
use futures_async_stream::try_stream;
use itertools::Itertools;
use risingwave_common::array::StreamChunk;
use risingwave_common::buffer::{Bitmap, BitmapBuilder};
use risingwave_common::catalog::Schema;
use risingwave_common::estimate_size::{EstimateSize, KvSize};
use risingwave_common::hash::{HashKey, PrecomputedBuildHasher};
use risingwave_common::types::ScalarImpl;
use risingwave_common::util::epoch::EpochPair;
use risingwave_common::util::iter_util::ZipEqFast;
use risingwave_expr::aggregate::{build_retractable, AggCall, BoxedAggregateFunction};
use risingwave_storage::StateStore;

use super::agg_common::{AggExecutorArgs, HashAggExecutorExtraArgs};
use super::aggregation::{
    agg_call_filter_res, iter_table_storage, AggStateStorage, DistinctDeduplicater, GroupKey,
    OnlyOutputIfHasInput,
};
use super::sort_buffer::SortBuffer;
use super::{
    expect_first_barrier, ActorContextRef, ExecutorInfo, PkIndicesRef, StreamExecutorResult,
    Watermark,
};
use crate::cache::{cache_may_stale, new_with_hasher, ManagedLruCache};
use crate::common::metrics::MetricsInfo;
use crate::common::table::state_table::StateTable;
use crate::common::StreamChunkBuilder;
use crate::error::StreamResult;
use crate::executor::aggregation::{generate_agg_schema, AggGroup as GenericAggGroup};
use crate::executor::error::StreamExecutorError;
use crate::executor::monitor::StreamingMetrics;
use crate::executor::{BoxedMessageStream, Executor, Message};
use crate::task::AtomicU64Ref;

type AggGroup<S> = GenericAggGroup<S, OnlyOutputIfHasInput>;
type AggGroupCache<K, S> = ManagedLruCache<K, AggGroup<S>, PrecomputedBuildHasher>;

/// [`HashAggExecutor`] could process large amounts of data using a state backend. It works as
/// follows:
///
/// * The executor pulls data from the upstream, and apply the data chunks to the corresponding
///   aggregation states.
/// * While processing, it will record which keys have been modified in this epoch using
///   `group_change_set`.
/// * Upon a barrier is received, the executor will call `.flush` on the storage backend, so that
///   all modifications will be flushed to the storage backend. Meanwhile, the executor will go
///   through `group_change_set`, and produce a stream chunk based on the state changes.
pub struct HashAggExecutor<K: HashKey, S: StateStore> {
    input: Box<dyn Executor>,
    inner: ExecutorInner<K, S>,
}

struct ExecutorInner<K: HashKey, S: StateStore> {
    _phantom: PhantomData<K>,

    actor_ctx: ActorContextRef,
    info: ExecutorInfo,

    /// Pk indices from input.
    input_pk_indices: Vec<usize>,

    /// Schema from input.
    input_schema: Schema,

    /// Indices of the columns
    /// all of the aggregation functions in this executor should depend on same group of keys
    group_key_indices: Vec<usize>,

    // The projection from group key in table schema to table pk.
    group_key_table_pk_projection: Arc<[usize]>,

    /// A [`HashAggExecutor`] may have multiple [`AggCall`]s.
    agg_calls: Vec<AggCall>,

    /// Aggregate functions.
    agg_funcs: Vec<BoxedAggregateFunction>,

    /// Index of row count agg call (`count(*)`) in the call list.
    row_count_index: usize,

    /// State storages for each aggregation calls.
    /// `None` means the agg call need not to maintain a state table by itself.
    storages: Vec<AggStateStorage<S>>,

    /// Intermediate state table for value-state agg calls.
    /// The state of all value-state aggregates are collected and stored in this
    /// table when `flush_data` is called.
    /// Also serves as EOWC sort buffer table.
    intermediate_state_table: StateTable<S>,

    /// State tables for deduplicating rows on distinct key for distinct agg calls.
    /// One table per distinct column (may be shared by multiple agg calls).
    distinct_dedup_tables: HashMap<usize, StateTable<S>>,

    /// Watermark epoch.
    watermark_epoch: AtomicU64Ref,

    /// State cache size for extreme agg.
    extreme_cache_size: usize,

    /// The maximum size of the chunk produced by executor at a time.
    chunk_size: usize,

    /// Should emit on window close according to watermark?
    emit_on_window_close: bool,

    metrics: Arc<StreamingMetrics>,
}

impl<K: HashKey, S: StateStore> ExecutorInner<K, S> {
    fn all_state_tables_mut(&mut self) -> impl Iterator<Item = &mut StateTable<S>> {
        iter_table_storage(&mut self.storages)
            .chain(self.distinct_dedup_tables.values_mut())
            .chain(std::iter::once(&mut self.intermediate_state_table))
    }
}

struct ExecutionVars<K: HashKey, S: StateStore> {
    stats: ExecutionStats,

    /// Cache for [`AggGroup`]s. `HashKey` -> `AggGroup`.
    agg_group_cache: AggGroupCache<K, S>,

    /// Changed group keys in the current epoch (before next flush).
    group_change_set: HashSet<K>,

    /// Heap size of `group_change_set` and dirty [`AggGroup`]s in `agg_group_cache`.
    dirty_groups_heap_size: KvSize,

    /// Distinct deduplicater to deduplicate input rows for each distinct agg call.
    distinct_dedup: DistinctDeduplicater<S>,

    /// Buffer watermarks on group keys received since last barrier.
    buffered_watermarks: Vec<Option<Watermark>>,

    /// Latest watermark on window column.
    window_watermark: Option<ScalarImpl>,

    /// Stream chunk builder.
    chunk_builder: StreamChunkBuilder,

    buffer: SortBuffer<S>,
}

struct ExecutionStats {
    /// How many times have we hit the cache of hash agg executor for the lookup of each key.
    lookup_miss_count: u64,
    total_lookup_count: u64,

    /// How many times have we hit the cache of hash agg executor for all the lookups generated by
    /// one StreamChunk.
    chunk_lookup_miss_count: u64,
    chunk_total_lookup_count: u64,
}

impl ExecutionStats {
    fn new() -> Self {
        Self {
            lookup_miss_count: 0,
            total_lookup_count: 0,
            chunk_lookup_miss_count: 0,
            chunk_total_lookup_count: 0,
        }
    }
}

impl<K: HashKey, S: StateStore> Executor for HashAggExecutor<K, S> {
    fn execute(self: Box<Self>) -> BoxedMessageStream {
        self.execute_inner().boxed()
    }

    fn schema(&self) -> &Schema {
        &self.inner.info.schema
    }

    fn pk_indices(&self) -> PkIndicesRef<'_> {
        &self.inner.info.pk_indices
    }

    fn identity(&self) -> &str {
        &self.inner.info.identity
    }
}

impl<K: HashKey, S: StateStore> HashAggExecutor<K, S> {
    pub fn new(args: AggExecutorArgs<S, HashAggExecutorExtraArgs>) -> StreamResult<Self> {
        let input_info = args.input.info();
        let schema = generate_agg_schema(
            args.input.as_ref(),
            &args.agg_calls,
            Some(&args.extra.group_key_indices),
        );

        let group_key_len = args.extra.group_key_indices.len();
        // NOTE: we assume the prefix of table pk is exactly the group key
        let group_key_table_pk_projection =
            &args.intermediate_state_table.pk_indices()[..group_key_len];
        assert!(group_key_table_pk_projection
            .iter()
            .sorted()
            .copied()
            .eq(0..group_key_len));

        Ok(Self {
            input: args.input,
            inner: ExecutorInner {
                _phantom: PhantomData,
                actor_ctx: args.actor_ctx,
                info: ExecutorInfo {
                    schema,
                    pk_indices: args.pk_indices,
                    identity: format!("HashAggExecutor {:X}", args.executor_id),
                },
                input_pk_indices: input_info.pk_indices,
                input_schema: input_info.schema,
                group_key_indices: args.extra.group_key_indices,
                group_key_table_pk_projection: group_key_table_pk_projection.to_vec().into(),
                agg_funcs: args.agg_calls.iter().map(build_retractable).try_collect()?,
                agg_calls: args.agg_calls,
                row_count_index: args.row_count_index,
                storages: args.storages,
                intermediate_state_table: args.intermediate_state_table,
                distinct_dedup_tables: args.distinct_dedup_tables,
                watermark_epoch: args.watermark_epoch,
                extreme_cache_size: args.extreme_cache_size,
                chunk_size: args.extra.chunk_size,
                emit_on_window_close: args.extra.emit_on_window_close,
                metrics: args.metrics,
            },
        })
    }

    /// Get visibilities that mask rows in the chunk for each group. The returned visibility
    /// is a `Bitmap` rather than `Option<Bitmap>` because it's likely to have multiple groups
    /// in one chunk.
    ///
    /// * `keys`: Hash Keys of rows.
    /// * `base_visibility`: Visibility of rows.
    fn get_group_visibilities(keys: Vec<K>, base_visibility: &Bitmap) -> Vec<(K, Bitmap)> {
        let n_rows = keys.len();
        let mut vis_builders = HashMap::new();
        for (row_idx, key) in keys
            .into_iter()
            .enumerate()
            .filter(|(row_idx, _)| base_visibility.is_set(*row_idx))
        {
            vis_builders
                .entry(key)
                .or_insert_with(|| BitmapBuilder::zeroed(n_rows))
                .set(row_idx, true);
        }
        vis_builders
            .into_iter()
            .map(|(key, vis_builder)| (key, vis_builder.finish()))
            .collect()
    }

    async fn ensure_keys_in_cache(
        this: &ExecutorInner<K, S>,
        cache: &mut AggGroupCache<K, S>,
        keys: impl IntoIterator<Item = &K>,
        stats: &mut ExecutionStats,
    ) -> StreamExecutorResult<()> {
        let group_key_types = &this.info.schema.data_types()[..this.group_key_indices.len()];
        let futs = keys
            .into_iter()
            .filter_map(|key| {
                stats.total_lookup_count += 1;
                if cache.contains(key) {
                    None
                } else {
                    stats.lookup_miss_count += 1;
                    Some(async {
                        // Create `AggGroup` for the current group if not exists. This will
                        // restore agg states from the intermediate state table.
                        let agg_group = AggGroup::create(
                            Some(GroupKey::new(
                                key.deserialize(group_key_types)?,
                                Some(this.group_key_table_pk_projection.clone()),
                            )),
                            &this.agg_calls,
                            &this.agg_funcs,
                            &this.storages,
                            &this.intermediate_state_table,
                            &this.input_pk_indices,
                            this.row_count_index,
                            this.extreme_cache_size,
                            &this.input_schema,
                        )
                        .await?;
                        Ok::<_, StreamExecutorError>((key.clone(), agg_group))
                    })
                }
            })
            .collect_vec(); // collect is necessary to avoid lifetime issue of `agg_group_cache`

        stats.chunk_total_lookup_count += 1;
        if !futs.is_empty() {
            // If not all the required states/keys are in the cache, it's a chunk-level cache miss.
            stats.chunk_lookup_miss_count += 1;
            let mut buffered = stream::iter(futs).buffer_unordered(10).fuse();
            while let Some(result) = buffered.next().await {
                let (key, agg_group) = result?;
                cache.put(key, agg_group);
            }
        }
        Ok(())
    }

    async fn apply_chunk(
        this: &mut ExecutorInner<K, S>,
        vars: &mut ExecutionVars<K, S>,
        chunk: StreamChunk,
    ) -> StreamExecutorResult<()> {
        // Find groups in this chunk and generate visibility for each group key.
        let keys = K::build(&this.group_key_indices, chunk.data_chunk())?;
        let group_visibilities = Self::get_group_visibilities(keys, chunk.visibility());

        // Create `AggGroup` for each group if not exists.
        Self::ensure_keys_in_cache(
            this,
            &mut vars.agg_group_cache,
            group_visibilities.iter().map(|(k, _)| k),
            &mut vars.stats,
        )
        .await?;

        // Calculate the row visibility for every agg call.
        let mut call_visibilities = Vec::with_capacity(this.agg_calls.len());
        for agg_call in &this.agg_calls {
            let agg_call_filter_res = agg_call_filter_res(agg_call, &chunk).await?;
            call_visibilities.push(agg_call_filter_res);
        }

        // Materialize input chunk if needed and possible.
        // For aggregations without distinct, we can materialize before grouping.
        for ((call, storage), visibility) in (this.agg_calls.iter())
            .zip_eq_fast(&mut this.storages)
            .zip_eq_fast(call_visibilities.iter())
        {
            if let AggStateStorage::MaterializedInput { table, mapping } = storage && !call.distinct {
                let chunk = chunk.project_with_vis(mapping.upstream_columns(), visibility.clone());
                table.write_chunk(chunk);
            }
        }

        // Apply chunk to each of the state (per agg_call), for each group.
        for (key, visibility) in group_visibilities {
            let mut agg_group = vars.agg_group_cache.get_mut(&key).unwrap();

            // Mark the group as changed.
            let key_size = key.estimated_size();
            let old_group_size = if vars.group_change_set.insert(key) {
                None
            } else {
                Some(agg_group.estimated_size())
            };

            let visibilities = call_visibilities
                .iter()
                .map(|call_vis| call_vis & &visibility)
                .collect();
            let visibilities = vars
                .distinct_dedup
                .dedup_chunk(
                    chunk.ops(),
                    chunk.columns(),
                    visibilities,
                    &mut this.distinct_dedup_tables,
                    agg_group.group_key(),
                    this.actor_ctx.clone(),
                )
                .await?;
            for ((call, storage), visibility) in (this.agg_calls.iter())
                .zip_eq_fast(&mut this.storages)
                .zip_eq_fast(visibilities.iter())
            {
                if let AggStateStorage::MaterializedInput { table, mapping } = storage && call.distinct {
                    let chunk = chunk.project_with_vis(mapping.upstream_columns(), visibility.clone());
                    table.write_chunk(chunk);
                }
            }
            agg_group
                .apply_chunk(&chunk, &this.agg_calls, &this.agg_funcs, visibilities)
                .await?;

            // Update the metrics.
            let actor_id_str = this.actor_ctx.id.to_string();
            let table_id_str = this.intermediate_state_table.table_id().to_string();
            let metric_dirty_count = this
                .metrics
                .agg_dirty_group_count
                .with_label_values(&[&table_id_str, &actor_id_str]);
            let metric_dirty_heap_size = this
                .metrics
                .agg_dirty_group_heap_size
                .with_label_values(&[&table_id_str, &actor_id_str]);
            let new_group_size = agg_group.estimated_size();
            if let Some(old_group_size) = old_group_size {
                match new_group_size.cmp(&old_group_size) {
                    std::cmp::Ordering::Greater => {
                        let inc_size = new_group_size - old_group_size;
                        vars.dirty_groups_heap_size.add_size(inc_size);
                        metric_dirty_heap_size.add(inc_size as i64);
                    }
                    std::cmp::Ordering::Less => {
                        let dec_size = old_group_size - new_group_size;
                        vars.dirty_groups_heap_size.sub_size(dec_size);
                        metric_dirty_heap_size.sub(dec_size as i64);
                    }
                    std::cmp::Ordering::Equal => {}
                }
            } else {
                let inc_size = key_size * 2 + new_group_size;
                vars.dirty_groups_heap_size.add_size(inc_size);
                metric_dirty_heap_size.add(inc_size as i64);
                metric_dirty_count.set(vars.group_change_set.len() as i64);
            }
        }

        Ok(())
    }

    #[try_stream(ok = StreamChunk, error = StreamExecutorError)]
    async fn flush_data<'a>(
        this: &'a mut ExecutorInner<K, S>,
        vars: &'a mut ExecutionVars<K, S>,
        epoch: EpochPair,
    ) {
        // Update metrics.
        let actor_id_str = this.actor_ctx.id.to_string();
        let table_id_str = this.intermediate_state_table.table_id().to_string();
        this.metrics
            .agg_lookup_miss_count
            .with_label_values(&[&table_id_str, &actor_id_str])
            .inc_by(vars.stats.lookup_miss_count);
        vars.stats.lookup_miss_count = 0;
        this.metrics
            .agg_total_lookup_count
            .with_label_values(&[&table_id_str, &actor_id_str])
            .inc_by(vars.stats.total_lookup_count);
        vars.stats.total_lookup_count = 0;
        this.metrics
            .agg_cached_keys
            .with_label_values(&[&table_id_str, &actor_id_str])
            .set(vars.agg_group_cache.len() as i64);
        this.metrics
            .agg_chunk_lookup_miss_count
            .with_label_values(&[&table_id_str, &actor_id_str])
            .inc_by(vars.stats.chunk_lookup_miss_count);
        vars.stats.chunk_lookup_miss_count = 0;
        this.metrics
            .agg_chunk_total_lookup_count
            .with_label_values(&[&table_id_str, &actor_id_str])
            .inc_by(vars.stats.chunk_total_lookup_count);
        vars.stats.chunk_total_lookup_count = 0;

        let window_watermark = vars.window_watermark.take();
        let n_dirty_group = vars.group_change_set.len();

        // flush changed states into intermediate state table
        for key in &vars.group_change_set {
            let agg_group = vars.agg_group_cache.get_mut(key).unwrap();
            let encoded_states = agg_group.encode_states(&this.agg_funcs)?;
            if this.emit_on_window_close {
                vars.buffer
                    .update_without_old_value(encoded_states, &mut this.intermediate_state_table);
            } else {
                this.intermediate_state_table
                    .update_without_old_value(encoded_states);
            }
        }

        if this.emit_on_window_close {
            // remove all groups under watermark and emit their results
            if let Some(watermark) = window_watermark.as_ref() {
                #[for_await]
                for row in vars
                    .buffer
                    .consume(watermark.clone(), &mut this.intermediate_state_table)
                {
                    let row = row?;
                    let group_key = row
                        .clone()
                        .into_iter()
                        .take(this.group_key_indices.len())
                        .collect();
                    let states = row.into_iter().skip(this.group_key_indices.len()).collect();

                    let mut agg_group = AggGroup::create_eowc(
                        Some(GroupKey::new(
                            group_key,
                            Some(this.group_key_table_pk_projection.clone()),
                        )),
                        &this.agg_calls,
                        &this.agg_funcs,
                        &this.storages,
                        &states,
                        &this.input_pk_indices,
                        this.row_count_index,
                        this.extreme_cache_size,
                        &this.input_schema,
                    )?;

                    let change = agg_group
                        .build_change(&this.storages, &this.agg_funcs)
                        .await?;
                    if let Some(change) = change {
                        if let Some(chunk) = vars.chunk_builder.append_record(change) {
                            yield chunk;
                        }
                    }
                }
            }
        } else {
            // emit on update
            // TODO(wrj,rc): we may need to parallelize it and set a reasonable concurrency limit.
            for group_key in &vars.group_change_set {
                let mut agg_group = vars.agg_group_cache.get_mut(group_key).unwrap();
                let change = agg_group
                    .build_change(&this.storages, &this.agg_funcs)
                    .await?;
                if let Some(change) = change {
                    if let Some(chunk) = vars.chunk_builder.append_record(change) {
                        yield chunk;
                    }
                }
            }
        }

        // clear the change set
        vars.group_change_set.clear();
        vars.dirty_groups_heap_size.set(0);
        this.metrics
            .agg_dirty_group_count
            .with_label_values(&[&table_id_str, &actor_id_str])
            .set(0);
        this.metrics
            .agg_dirty_group_heap_size
            .with_label_values(&[&table_id_str, &actor_id_str])
            .set(0);

        // Yield the remaining rows in chunk builder.
        if let Some(chunk) = vars.chunk_builder.take() {
            yield chunk;
        }

        if n_dirty_group == 0 && window_watermark.is_none() {
            // Nothing is expected to be changed.
            this.all_state_tables_mut().for_each(|table| {
                table.commit_no_data_expected(epoch);
            });
        } else {
            if let Some(watermark) = window_watermark {
                // Update watermark of state tables, for state cleaning.
                this.all_state_tables_mut()
                    .for_each(|table| table.update_watermark(watermark.clone(), false));
            }
            // Commit all state tables.
            futures::future::try_join_all(
                this.all_state_tables_mut()
                    .map(|table| async { table.commit(epoch).await }),
            )
            .await?;
        }

        // Flush distinct dedup state.
        vars.distinct_dedup
            .flush(&mut this.distinct_dedup_tables, this.actor_ctx.clone())?;

        // Evict cache to target capacity.
        vars.agg_group_cache.evict();
    }

    #[try_stream(ok = Message, error = StreamExecutorError)]
    async fn execute_inner(self) {
        let HashAggExecutor {
            input,
            inner: mut this,
        } = self;

        let window_col_idx_in_group_key = this.intermediate_state_table.pk_indices()[0];
        let window_col_idx = this.group_key_indices[window_col_idx_in_group_key];

        let agg_group_cache_metrics_info = MetricsInfo::new(
            this.metrics.clone(),
            this.intermediate_state_table.table_id(),
            this.actor_ctx.id,
            "agg intermediate state table",
        );

        let mut vars = ExecutionVars {
            stats: ExecutionStats::new(),
            agg_group_cache: new_with_hasher(
                this.watermark_epoch.clone(),
                agg_group_cache_metrics_info,
                PrecomputedBuildHasher,
            ),
            group_change_set: HashSet::new(),
            dirty_groups_heap_size: KvSize::default(),
            distinct_dedup: DistinctDeduplicater::new(
                &this.agg_calls,
                &this.watermark_epoch,
                &this.distinct_dedup_tables,
                this.actor_ctx.id,
                this.metrics.clone(),
            ),
            buffered_watermarks: vec![None; this.group_key_indices.len()],
            window_watermark: None,
            chunk_builder: StreamChunkBuilder::new(this.chunk_size, this.info.schema.data_types()),
            buffer: SortBuffer::new(window_col_idx_in_group_key, &this.intermediate_state_table),
        };

        // TODO(rc): use something like a `ColumnMapping` type
        let group_key_invert_idx = {
            let mut group_key_invert_idx = vec![None; input.info().schema.len()];
            for (group_key_seq, group_key_idx) in this.group_key_indices.iter().enumerate() {
                group_key_invert_idx[*group_key_idx] = Some(group_key_seq);
            }
            group_key_invert_idx
        };

        // First barrier
        let mut input = input.execute();
        let barrier = expect_first_barrier(&mut input).await?;
        this.all_state_tables_mut().for_each(|table| {
            table.init_epoch(barrier.epoch);
        });
        vars.agg_group_cache.update_epoch(barrier.epoch.curr);
        vars.distinct_dedup.dedup_caches_mut().for_each(|cache| {
            cache.update_epoch(barrier.epoch.curr);
        });

        yield Message::Barrier(barrier);

        #[for_await]
        for msg in input {
            let msg = msg?;
            vars.agg_group_cache.evict_except_cur_epoch();
            match msg {
                Message::Watermark(watermark) => {
                    let group_key_seq = group_key_invert_idx[watermark.col_idx];
                    if let Some(group_key_seq) = group_key_seq {
                        if watermark.col_idx == window_col_idx {
                            vars.window_watermark = Some(watermark.val.clone());
                        }
                        vars.buffered_watermarks[group_key_seq] =
                            Some(watermark.with_idx(group_key_seq));
                    }
                }
                Message::Chunk(chunk) => {
                    Self::apply_chunk(&mut this, &mut vars, chunk).await?;
                }
                Message::Barrier(barrier) => {
                    #[for_await]
                    for chunk in Self::flush_data(&mut this, &mut vars, barrier.epoch) {
                        yield Message::Chunk(chunk?);
                    }

                    if this.emit_on_window_close {
                        // ignore watermarks on other columns
                        if let Some(watermark) =
                            vars.buffered_watermarks[window_col_idx_in_group_key].take()
                        {
                            yield Message::Watermark(watermark);
                        }
                    } else {
                        for buffered_watermark in &mut vars.buffered_watermarks {
                            if let Some(watermark) = buffered_watermark.take() {
                                yield Message::Watermark(watermark);
                            }
                        }
                    }

                    // Update the vnode bitmap for state tables of all agg calls if asked.
                    if let Some(vnode_bitmap) = barrier.as_update_vnode_bitmap(this.actor_ctx.id) {
                        let previous_vnode_bitmap = this.intermediate_state_table.vnodes().clone();
                        this.all_state_tables_mut().for_each(|table| {
                            let _ = table.update_vnode_bitmap(vnode_bitmap.clone());
                        });

                        // Manipulate the cache if necessary.
                        if cache_may_stale(&previous_vnode_bitmap, &vnode_bitmap) {
                            vars.agg_group_cache.clear();
                            vars.distinct_dedup.dedup_caches_mut().for_each(|cache| {
                                cache.clear();
                            });
                        }
                    }

                    // Update the current epoch.
                    vars.agg_group_cache.update_epoch(barrier.epoch.curr);
                    vars.distinct_dedup.dedup_caches_mut().for_each(|cache| {
                        cache.update_epoch(barrier.epoch.curr);
                    });

                    yield Message::Barrier(barrier);
                }
            }
        }
    }
}
