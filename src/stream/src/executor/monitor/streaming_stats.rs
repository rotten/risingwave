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

use std::sync::OnceLock;

use prometheus::core::{AtomicF64, AtomicI64, AtomicU64, GenericCounterVec, GenericGaugeVec};
use prometheus::{
    exponential_buckets, histogram_opts, register_gauge_vec_with_registry,
    register_histogram_vec_with_registry, register_histogram_with_registry,
    register_int_counter_vec_with_registry, register_int_counter_with_registry,
    register_int_gauge_vec_with_registry, register_int_gauge_with_registry, Histogram,
    HistogramVec, IntCounter, IntGauge, Registry,
};
use risingwave_common::config::MetricLevel;
use risingwave_common::metrics::RelabeledHistogramVec;
use risingwave_common::monitor::GLOBAL_METRICS_REGISTRY;

#[derive(Clone)]
pub struct StreamingMetrics {
    pub level: MetricLevel,

    pub executor_row_count: GenericCounterVec<AtomicU64>,
    pub actor_execution_time: GenericGaugeVec<AtomicF64>,
    pub actor_output_buffer_blocking_duration_ns: GenericCounterVec<AtomicU64>,
    pub actor_input_buffer_blocking_duration_ns: GenericCounterVec<AtomicU64>,
    pub actor_scheduled_duration: GenericGaugeVec<AtomicF64>,
    pub actor_scheduled_cnt: GenericGaugeVec<AtomicI64>,
    pub actor_fast_poll_duration: GenericGaugeVec<AtomicF64>,
    pub actor_fast_poll_cnt: GenericGaugeVec<AtomicI64>,
    pub actor_slow_poll_duration: GenericGaugeVec<AtomicF64>,
    pub actor_slow_poll_cnt: GenericGaugeVec<AtomicI64>,
    pub actor_poll_duration: GenericGaugeVec<AtomicF64>,
    pub actor_poll_cnt: GenericGaugeVec<AtomicI64>,
    pub actor_idle_duration: GenericGaugeVec<AtomicF64>,
    pub actor_idle_cnt: GenericGaugeVec<AtomicI64>,
    pub actor_memory_usage: GenericGaugeVec<AtomicI64>,
    pub actor_in_record_cnt: GenericCounterVec<AtomicU64>,
    pub actor_out_record_cnt: GenericCounterVec<AtomicU64>,
    pub actor_sampled_deserialize_duration_ns: GenericCounterVec<AtomicU64>,
    pub source_output_row_count: GenericCounterVec<AtomicU64>,
    pub source_row_per_barrier: GenericCounterVec<AtomicU64>,
    pub source_split_change_count: GenericCounterVec<AtomicU64>,

    // Exchange (see also `compute::ExchangeServiceMetrics`)
    pub exchange_frag_recv_size: GenericCounterVec<AtomicU64>,

    // Streaming Join
    pub join_lookup_miss_count: GenericCounterVec<AtomicU64>,
    pub join_total_lookup_count: GenericCounterVec<AtomicU64>,
    pub join_insert_cache_miss_count: GenericCounterVec<AtomicU64>,
    pub join_actor_input_waiting_duration_ns: GenericCounterVec<AtomicU64>,
    pub join_match_duration_ns: GenericCounterVec<AtomicU64>,
    pub join_barrier_align_duration: RelabeledHistogramVec,
    pub join_cached_entries: GenericGaugeVec<AtomicI64>,
    pub join_cached_rows: GenericGaugeVec<AtomicI64>,
    pub join_cached_estimated_size: GenericGaugeVec<AtomicI64>,
    pub join_matched_join_keys: RelabeledHistogramVec,

    // Streaming Aggregation
    pub agg_lookup_miss_count: GenericCounterVec<AtomicU64>,
    pub agg_total_lookup_count: GenericCounterVec<AtomicU64>,
    pub agg_cached_keys: GenericGaugeVec<AtomicI64>,
    pub agg_chunk_lookup_miss_count: GenericCounterVec<AtomicU64>,
    pub agg_chunk_total_lookup_count: GenericCounterVec<AtomicU64>,
    pub agg_distinct_cache_miss_count: GenericCounterVec<AtomicU64>,
    pub agg_distinct_total_cache_count: GenericCounterVec<AtomicU64>,
    pub agg_distinct_cached_entry_count: GenericGaugeVec<AtomicI64>,
    pub agg_dirty_group_count: GenericGaugeVec<AtomicI64>,
    pub agg_dirty_group_heap_size: GenericGaugeVec<AtomicI64>,

    // Streaming TopN
    pub group_top_n_cache_miss_count: GenericCounterVec<AtomicU64>,
    pub group_top_n_total_query_cache_count: GenericCounterVec<AtomicU64>,
    pub group_top_n_cached_entry_count: GenericGaugeVec<AtomicI64>,
    pub group_top_n_appendonly_cache_miss_count: GenericCounterVec<AtomicU64>,
    pub group_top_n_appendonly_total_query_cache_count: GenericCounterVec<AtomicU64>,
    pub group_top_n_appendonly_cached_entry_count: GenericGaugeVec<AtomicI64>,

    // look up
    pub lookup_cache_miss_count: GenericCounterVec<AtomicU64>,
    pub lookup_total_query_cache_count: GenericCounterVec<AtomicU64>,
    pub lookup_cached_entry_count: GenericGaugeVec<AtomicI64>,

    // temporal join
    pub temporal_join_cache_miss_count: GenericCounterVec<AtomicU64>,
    pub temporal_join_total_query_cache_count: GenericCounterVec<AtomicU64>,
    pub temporal_join_cached_entry_count: GenericGaugeVec<AtomicI64>,

    // Backfill
    pub backfill_snapshot_read_row_count: GenericCounterVec<AtomicU64>,
    pub backfill_upstream_output_row_count: GenericCounterVec<AtomicU64>,

    // Arrangement Backfill
    pub arrangement_backfill_snapshot_read_row_count: GenericCounterVec<AtomicU64>,
    pub arrangement_backfill_upstream_output_row_count: GenericCounterVec<AtomicU64>,

    // Over Window
    pub over_window_cached_entry_count: GenericGaugeVec<AtomicI64>,
    pub over_window_cache_lookup_count: GenericCounterVec<AtomicU64>,
    pub over_window_cache_miss_count: GenericCounterVec<AtomicU64>,

    /// The duration from receipt of barrier to all actors collection.
    /// And the max of all node `barrier_inflight_latency` is the latency for a barrier
    /// to flow through the graph.
    pub barrier_inflight_latency: Histogram,
    /// The duration of sync to storage.
    pub barrier_sync_latency: Histogram,
    /// The progress made by the earliest in-flight barriers in the local barrier manager.
    pub barrier_manager_progress: IntCounter,

    pub sink_commit_duration: HistogramVec,
    pub connector_sink_rows_received: GenericCounterVec<AtomicU64>,

    // Memory management
    // FIXME(yuhao): use u64 here
    pub lru_current_watermark_time_ms: IntGauge,
    pub lru_physical_now_ms: IntGauge,
    pub lru_runtime_loop_count: IntCounter,
    pub lru_watermark_step: IntGauge,
    pub lru_evicted_watermark_time_ms: GenericGaugeVec<AtomicI64>,
    pub jemalloc_allocated_bytes: IntGauge,
    pub jemalloc_active_bytes: IntGauge,

    /// User compute error reporting
    pub user_compute_error_count: GenericCounterVec<AtomicU64>,

    /// User source reader error
    pub user_source_reader_error_count: GenericCounterVec<AtomicU64>,

    // Materialize
    pub materialize_cache_hit_count: GenericCounterVec<AtomicU64>,
    pub materialize_cache_total_count: GenericCounterVec<AtomicU64>,

    // Memory
    pub stream_memory_usage: GenericGaugeVec<AtomicI64>,
}

pub static GLOBAL_STREAMING_METRICS: OnceLock<StreamingMetrics> = OnceLock::new();

pub fn global_streaming_metrics(metric_level: MetricLevel) -> StreamingMetrics {
    GLOBAL_STREAMING_METRICS
        .get_or_init(|| StreamingMetrics::new(&GLOBAL_METRICS_REGISTRY, metric_level))
        .clone()
}

impl StreamingMetrics {
    fn new(registry: &Registry, level: MetricLevel) -> Self {
        let executor_row_count = register_int_counter_vec_with_registry!(
            "stream_executor_row_count",
            "Total number of rows that have been output from each executor",
            &["actor_id", "executor_identity"],
            registry
        )
        .unwrap();

        let source_output_row_count = register_int_counter_vec_with_registry!(
            "stream_source_output_rows_counts",
            "Total number of rows that have been output from source",
            &["source_id", "source_name", "actor_id"],
            registry
        )
        .unwrap();

        let source_row_per_barrier = register_int_counter_vec_with_registry!(
            "stream_source_rows_per_barrier_counts",
            "Total number of rows that have been output from source per barrier",
            &["actor_id", "executor_id"],
            registry
        )
        .unwrap();

        let source_split_change_count = register_int_counter_vec_with_registry!(
            "stream_source_split_change_event_count",
            "Total number of split change events that have been operated by source",
            &["source_id", "source_name", "actor_id"],
            registry
        )
        .unwrap();

        let actor_execution_time = register_gauge_vec_with_registry!(
            "stream_actor_actor_execution_time",
            "Total execution time (s) of an actor",
            &["actor_id"],
            registry
        )
        .unwrap();

        let actor_output_buffer_blocking_duration_ns = register_int_counter_vec_with_registry!(
            "stream_actor_output_buffer_blocking_duration_ns",
            "Total blocking duration (ns) of output buffer",
            &["actor_id", "fragment_id", "downstream_fragment_id"],
            registry
        )
        .unwrap();

        let actor_input_buffer_blocking_duration_ns = register_int_counter_vec_with_registry!(
            "stream_actor_input_buffer_blocking_duration_ns",
            "Total blocking duration (ns) of input buffer",
            &["actor_id", "fragment_id", "upstream_fragment_id"],
            registry
        )
        .unwrap();

        let exchange_frag_recv_size = register_int_counter_vec_with_registry!(
            "stream_exchange_frag_recv_size",
            "Total size of messages that have been received from upstream Fragment",
            &["up_fragment_id", "down_fragment_id"],
            registry
        )
        .unwrap();

        let actor_fast_poll_duration = register_gauge_vec_with_registry!(
            "stream_actor_fast_poll_duration",
            "tokio's metrics",
            &["actor_id"],
            registry
        )
        .unwrap();

        let actor_fast_poll_cnt = register_int_gauge_vec_with_registry!(
            "stream_actor_fast_poll_cnt",
            "tokio's metrics",
            &["actor_id"],
            registry
        )
        .unwrap();

        let actor_slow_poll_duration = register_gauge_vec_with_registry!(
            "stream_actor_slow_poll_duration",
            "tokio's metrics",
            &["actor_id"],
            registry
        )
        .unwrap();

        let actor_slow_poll_cnt = register_int_gauge_vec_with_registry!(
            "stream_actor_slow_poll_cnt",
            "tokio's metrics",
            &["actor_id"],
            registry
        )
        .unwrap();

        let actor_poll_duration = register_gauge_vec_with_registry!(
            "stream_actor_poll_duration",
            "tokio's metrics",
            &["actor_id"],
            registry
        )
        .unwrap();

        let actor_poll_cnt = register_int_gauge_vec_with_registry!(
            "stream_actor_poll_cnt",
            "tokio's metrics",
            &["actor_id"],
            registry
        )
        .unwrap();

        let actor_scheduled_duration = register_gauge_vec_with_registry!(
            "stream_actor_scheduled_duration",
            "tokio's metrics",
            &["actor_id"],
            registry
        )
        .unwrap();

        let actor_scheduled_cnt = register_int_gauge_vec_with_registry!(
            "stream_actor_scheduled_cnt",
            "tokio's metrics",
            &["actor_id"],
            registry
        )
        .unwrap();

        let actor_idle_duration = register_gauge_vec_with_registry!(
            "stream_actor_idle_duration",
            "tokio's metrics",
            &["actor_id"],
            registry
        )
        .unwrap();

        let actor_idle_cnt = register_int_gauge_vec_with_registry!(
            "stream_actor_idle_cnt",
            "tokio's metrics",
            &["actor_id"],
            registry
        )
        .unwrap();

        let actor_in_record_cnt = register_int_counter_vec_with_registry!(
            "stream_actor_in_record_cnt",
            "Total number of rows actor received",
            &["actor_id"],
            registry
        )
        .unwrap();

        let actor_out_record_cnt = register_int_counter_vec_with_registry!(
            "stream_actor_out_record_cnt",
            "Total number of rows actor sent",
            &["actor_id", "fragment_id"],
            registry
        )
        .unwrap();

        let actor_sampled_deserialize_duration_ns = register_int_counter_vec_with_registry!(
            "actor_sampled_deserialize_duration_ns",
            "Duration (ns) of sampled chunk deserialization",
            &["actor_id"],
            registry
        )
        .unwrap();

        let actor_memory_usage = register_int_gauge_vec_with_registry!(
            "actor_memory_usage",
            "Memory usage (bytes)",
            &["actor_id"],
            registry,
        )
        .unwrap();

        let join_lookup_miss_count = register_int_counter_vec_with_registry!(
            "stream_join_lookup_miss_count",
            "Join executor lookup miss duration",
            &["side", "join_table_id", "degree_table_id", "actor_id"],
            registry
        )
        .unwrap();

        let join_total_lookup_count = register_int_counter_vec_with_registry!(
            "stream_join_lookup_total_count",
            "Join executor lookup total operation",
            &["side", "join_table_id", "degree_table_id", "actor_id"],
            registry
        )
        .unwrap();

        let join_insert_cache_miss_count = register_int_counter_vec_with_registry!(
            "stream_join_insert_cache_miss_count",
            "Join executor cache miss when insert operation",
            &["side", "join_table_id", "degree_table_id", "actor_id"],
            registry
        )
        .unwrap();

        let join_actor_input_waiting_duration_ns = register_int_counter_vec_with_registry!(
            "stream_join_actor_input_waiting_duration_ns",
            "Total waiting duration (ns) of input buffer of join actor",
            &["actor_id"],
            registry
        )
        .unwrap();

        let join_match_duration_ns = register_int_counter_vec_with_registry!(
            "stream_join_match_duration_ns",
            "Matching duration for each side",
            &["actor_id", "side"],
            registry
        )
        .unwrap();

        let opts = histogram_opts!(
            "stream_join_barrier_align_duration",
            "Duration of join align barrier",
            exponential_buckets(0.0001, 2.0, 21).unwrap() // max 104s
        );
        let join_barrier_align_duration = register_histogram_vec_with_registry!(
            opts,
            &["actor_id", "fragment_id", "wait_side"],
            registry
        )
        .unwrap();

        let join_barrier_align_duration = RelabeledHistogramVec::with_metric_level_relabel_n(
            MetricLevel::Debug,
            join_barrier_align_duration,
            level,
            1,
        );

        let join_cached_entries = register_int_gauge_vec_with_registry!(
            "stream_join_cached_entries",
            "Number of cached entries in streaming join operators",
            &["actor_id", "side"],
            registry
        )
        .unwrap();

        let join_cached_rows = register_int_gauge_vec_with_registry!(
            "stream_join_cached_rows",
            "Number of cached rows in streaming join operators",
            &["actor_id", "side"],
            registry
        )
        .unwrap();

        let join_cached_estimated_size = register_int_gauge_vec_with_registry!(
            "stream_join_cached_estimated_size",
            "Estimated size of all cached entries in streaming join operators",
            &["actor_id", "side"],
            registry
        )
        .unwrap();

        let join_matched_join_keys_opts = histogram_opts!(
            "stream_join_matched_join_keys",
            "The number of keys matched in the opposite side",
            exponential_buckets(16.0, 2.0, 28).unwrap() // max 2^31
        );

        let join_matched_join_keys = register_histogram_vec_with_registry!(
            join_matched_join_keys_opts,
            &["actor_id", "fragment_id", "table_id"],
            registry
        )
        .unwrap();

        let join_matched_join_keys = RelabeledHistogramVec::with_metric_level_relabel_n(
            MetricLevel::Debug,
            join_matched_join_keys,
            level,
            1,
        );

        let agg_lookup_miss_count = register_int_counter_vec_with_registry!(
            "stream_agg_lookup_miss_count",
            "Aggregation executor lookup miss duration",
            &["table_id", "actor_id"],
            registry
        )
        .unwrap();

        let agg_total_lookup_count = register_int_counter_vec_with_registry!(
            "stream_agg_lookup_total_count",
            "Aggregation executor lookup total operation",
            &["table_id", "actor_id"],
            registry
        )
        .unwrap();

        let agg_distinct_cache_miss_count = register_int_counter_vec_with_registry!(
            "stream_agg_distinct_cache_miss_count",
            "Aggregation executor dinsinct miss duration",
            &["table_id", "actor_id"],
            registry
        )
        .unwrap();

        let agg_distinct_total_cache_count = register_int_counter_vec_with_registry!(
            "stream_agg_distinct_total_cache_count",
            "Aggregation executor distinct total operation",
            &["table_id", "actor_id"],
            registry
        )
        .unwrap();

        let agg_distinct_cached_entry_count = register_int_gauge_vec_with_registry!(
            "stream_agg_distinct_cached_entry_count",
            "Total entry counts in distinct aggregation executor cache",
            &["table_id", "actor_id"],
            registry
        )
        .unwrap();

        let agg_dirty_group_count = register_int_gauge_vec_with_registry!(
            "stream_agg_dirty_group_count",
            "Total dirty group counts in aggregation executor",
            &["table_id", "actor_id"],
            registry
        )
        .unwrap();

        let agg_dirty_group_heap_size = register_int_gauge_vec_with_registry!(
            "stream_agg_dirty_group_heap_size",
            "Total dirty group heap size in aggregation executor",
            &["table_id", "actor_id"],
            registry
        )
        .unwrap();

        let group_top_n_cache_miss_count = register_int_counter_vec_with_registry!(
            "stream_group_top_n_cache_miss_count",
            "Group top n executor cache miss count",
            &["table_id", "actor_id"],
            registry
        )
        .unwrap();

        let group_top_n_total_query_cache_count = register_int_counter_vec_with_registry!(
            "stream_group_top_n_total_query_cache_count",
            "Group top n executor query cache total count",
            &["table_id", "actor_id"],
            registry
        )
        .unwrap();

        let group_top_n_cached_entry_count = register_int_gauge_vec_with_registry!(
            "stream_group_top_n_cached_entry_count",
            "Total entry counts in group top n executor cache",
            &["table_id", "actor_id"],
            registry
        )
        .unwrap();

        let group_top_n_appendonly_cache_miss_count = register_int_counter_vec_with_registry!(
            "stream_group_top_n_appendonly_cache_miss_count",
            "Group top n appendonly executor cache miss count",
            &["table_id", "actor_id"],
            registry
        )
        .unwrap();

        let group_top_n_appendonly_total_query_cache_count =
            register_int_counter_vec_with_registry!(
                "stream_group_top_n_appendonly_total_query_cache_count",
                "Group top n appendonly executor total cache count",
                &["table_id", "actor_id"],
                registry
            )
            .unwrap();

        let group_top_n_appendonly_cached_entry_count = register_int_gauge_vec_with_registry!(
            "stream_group_top_n_appendonly_cached_entry_count",
            "Total entry counts in group top n appendonly executor cache",
            &["table_id", "actor_id"],
            registry
        )
        .unwrap();

        let lookup_cache_miss_count = register_int_counter_vec_with_registry!(
            "stream_lookup_cache_miss_count",
            "Lookup executor cache miss count",
            &["table_id", "actor_id"],
            registry
        )
        .unwrap();

        let lookup_total_query_cache_count = register_int_counter_vec_with_registry!(
            "stream_lookup_total_query_cache_count",
            "Lookup executor query cache total count",
            &["table_id", "actor_id"],
            registry
        )
        .unwrap();

        let lookup_cached_entry_count = register_int_gauge_vec_with_registry!(
            "stream_lookup_cached_entry_count",
            "Total entry counts in lookup executor cache",
            &["table_id", "actor_id"],
            registry
        )
        .unwrap();

        let temporal_join_cache_miss_count = register_int_counter_vec_with_registry!(
            "stream_temporal_join_cache_miss_count",
            "Temporal join executor cache miss count",
            &["table_id", "actor_id"],
            registry
        )
        .unwrap();

        let temporal_join_total_query_cache_count = register_int_counter_vec_with_registry!(
            "stream_temporal_join_total_query_cache_count",
            "Temporal join executor query cache total count",
            &["table_id", "actor_id"],
            registry
        )
        .unwrap();

        let temporal_join_cached_entry_count = register_int_gauge_vec_with_registry!(
            "stream_temporal_join_cached_entry_count",
            "Total entry count in temporal join executor cache",
            &["table_id", "actor_id"],
            registry
        )
        .unwrap();

        let agg_cached_keys = register_int_gauge_vec_with_registry!(
            "stream_agg_cached_keys",
            "Number of cached keys in streaming aggregation operators",
            &["table_id", "actor_id"],
            registry
        )
        .unwrap();

        let agg_chunk_lookup_miss_count = register_int_counter_vec_with_registry!(
            "stream_agg_chunk_lookup_miss_count",
            "Aggregation executor chunk-level lookup miss duration",
            &["table_id", "actor_id"],
            registry
        )
        .unwrap();

        let agg_chunk_total_lookup_count = register_int_counter_vec_with_registry!(
            "stream_agg_chunk_lookup_total_count",
            "Aggregation executor chunk-level lookup total operation",
            &["table_id", "actor_id"],
            registry
        )
        .unwrap();

        let backfill_snapshot_read_row_count = register_int_counter_vec_with_registry!(
            "stream_backfill_snapshot_read_row_count",
            "Total number of rows that have been read from the backfill snapshot",
            &["table_id", "actor_id"],
            registry
        )
        .unwrap();

        let backfill_upstream_output_row_count = register_int_counter_vec_with_registry!(
            "stream_backfill_upstream_output_row_count",
            "Total number of rows that have been output from the backfill upstream",
            &["table_id", "actor_id"],
            registry
        )
        .unwrap();

        let arrangement_backfill_snapshot_read_row_count = register_int_counter_vec_with_registry!(
            "stream_arrangement_backfill_snapshot_read_row_count",
            "Total number of rows that have been read from the arrangement_backfill snapshot",
            &["table_id", "actor_id"],
            registry
        )
        .unwrap();

        let arrangement_backfill_upstream_output_row_count =
            register_int_counter_vec_with_registry!(
                "stream_arrangement_backfill_upstream_output_row_count",
                "Total number of rows that have been output from the arrangement_backfill upstream",
                &["table_id", "actor_id"],
                registry
            )
            .unwrap();

        let over_window_cached_entry_count = register_int_gauge_vec_with_registry!(
            "stream_over_window_cached_entry_count",
            "Total entry (partition) count in over window executor cache",
            &["table_id", "actor_id"],
            registry
        )
        .unwrap();

        let over_window_cache_lookup_count = register_int_counter_vec_with_registry!(
            "stream_over_window_cache_lookup_count",
            "Over window executor cache lookup count",
            &["table_id", "actor_id"],
            registry
        )
        .unwrap();

        let over_window_cache_miss_count = register_int_counter_vec_with_registry!(
            "stream_over_window_cache_miss_count",
            "Over window executor cache miss count",
            &["table_id", "actor_id"],
            registry
        )
        .unwrap();

        let opts = histogram_opts!(
            "stream_barrier_inflight_duration_seconds",
            "barrier_inflight_latency",
            exponential_buckets(0.1, 1.5, 16).unwrap() // max 43s
        );
        let barrier_inflight_latency = register_histogram_with_registry!(opts, registry).unwrap();

        let opts = histogram_opts!(
            "stream_barrier_sync_storage_duration_seconds",
            "barrier_sync_latency",
            exponential_buckets(0.1, 1.5, 16).unwrap() // max 43s
        );
        let barrier_sync_latency = register_histogram_with_registry!(opts, registry).unwrap();

        let barrier_manager_progress = register_int_counter_with_registry!(
            "stream_barrier_manager_progress",
            "The number of actors that have processed the earliest in-flight barriers",
            registry
        )
        .unwrap();

        let sink_commit_duration = register_histogram_vec_with_registry!(
            "sink_commit_duration",
            "Duration of commit op in sink",
            &["executor_id", "connector"],
            registry
        )
        .unwrap();

        let connector_sink_rows_received = register_int_counter_vec_with_registry!(
            "connector_sink_rows_received",
            "Number of rows received by sink",
            &["connector_type", "sink_id"],
            registry
        )
        .unwrap();

        let lru_current_watermark_time_ms = register_int_gauge_with_registry!(
            "lru_current_watermark_time_ms",
            "Current LRU manager watermark time(ms)",
            registry
        )
        .unwrap();

        let lru_physical_now_ms = register_int_gauge_with_registry!(
            "lru_physical_now_ms",
            "Current physical time in Risingwave(ms)",
            registry
        )
        .unwrap();

        let lru_runtime_loop_count = register_int_counter_with_registry!(
            "lru_runtime_loop_count",
            "The counts of the eviction loop in LRU manager per second",
            registry
        )
        .unwrap();

        let lru_watermark_step = register_int_gauge_with_registry!(
            "lru_watermark_step",
            "The steps increase in 1 loop",
            registry
        )
        .unwrap();

        let lru_evicted_watermark_time_ms = register_int_gauge_vec_with_registry!(
            "lru_evicted_watermark_time_ms",
            "The latest evicted watermark time by actors",
            &["table_id", "actor_id", "desc"],
            registry
        )
        .unwrap();

        let jemalloc_allocated_bytes = register_int_gauge_with_registry!(
            "jemalloc_allocated_bytes",
            "The allocated memory jemalloc, got from jemalloc_ctl",
            registry
        )
        .unwrap();

        let jemalloc_active_bytes = register_int_gauge_with_registry!(
            "jemalloc_active_bytes",
            "The active memory jemalloc, got from jemalloc_ctl",
            registry
        )
        .unwrap();

        let user_compute_error_count = register_int_counter_vec_with_registry!(
            "user_compute_error_count",
            "Compute errors in the system, queryable by tags",
            &["error_type", "error_msg", "executor_name", "fragment_id"],
            registry,
        )
        .unwrap();

        let user_source_reader_error_count = register_int_counter_vec_with_registry!(
            "user_source_reader_error_count",
            "Source reader error count",
            &[
                "error_type",
                "error_msg",
                "executor_name",
                "actor_id",
                "source_id"
            ],
            registry,
        )
        .unwrap();

        let materialize_cache_hit_count = register_int_counter_vec_with_registry!(
            "stream_materialize_cache_hit_count",
            "Materialize executor cache hit count",
            &["table_id", "actor_id"],
            registry
        )
        .unwrap();

        let materialize_cache_total_count = register_int_counter_vec_with_registry!(
            "stream_materialize_cache_total_count",
            "Materialize executor cache total operation",
            &["table_id", "actor_id"],
            registry
        )
        .unwrap();

        let stream_memory_usage = register_int_gauge_vec_with_registry!(
            "stream_memory_usage",
            "Memory usage for stream executors",
            &["table_id", "actor_id", "desc"],
            registry
        )
        .unwrap();

        Self {
            level,
            executor_row_count,
            actor_execution_time,
            actor_output_buffer_blocking_duration_ns,
            actor_input_buffer_blocking_duration_ns,
            actor_scheduled_duration,
            actor_scheduled_cnt,
            actor_fast_poll_duration,
            actor_fast_poll_cnt,
            actor_slow_poll_duration,
            actor_slow_poll_cnt,
            actor_poll_duration,
            actor_poll_cnt,
            actor_idle_duration,
            actor_idle_cnt,
            actor_memory_usage,
            actor_in_record_cnt,
            actor_out_record_cnt,
            actor_sampled_deserialize_duration_ns,
            source_output_row_count,
            source_row_per_barrier,
            source_split_change_count,
            exchange_frag_recv_size,
            join_lookup_miss_count,
            join_total_lookup_count,
            join_insert_cache_miss_count,
            join_actor_input_waiting_duration_ns,
            join_match_duration_ns,
            join_barrier_align_duration,
            join_cached_entries,
            join_cached_rows,
            join_cached_estimated_size,
            join_matched_join_keys,
            agg_lookup_miss_count,
            agg_total_lookup_count,
            agg_cached_keys,
            agg_chunk_lookup_miss_count,
            agg_chunk_total_lookup_count,
            agg_distinct_cache_miss_count,
            agg_distinct_total_cache_count,
            agg_distinct_cached_entry_count,
            agg_dirty_group_count,
            agg_dirty_group_heap_size,
            group_top_n_cache_miss_count,
            group_top_n_total_query_cache_count,
            group_top_n_cached_entry_count,
            group_top_n_appendonly_cache_miss_count,
            group_top_n_appendonly_total_query_cache_count,
            group_top_n_appendonly_cached_entry_count,
            lookup_cache_miss_count,
            lookup_total_query_cache_count,
            lookup_cached_entry_count,
            temporal_join_cache_miss_count,
            temporal_join_total_query_cache_count,
            temporal_join_cached_entry_count,
            backfill_snapshot_read_row_count,
            backfill_upstream_output_row_count,
            arrangement_backfill_snapshot_read_row_count,
            arrangement_backfill_upstream_output_row_count,
            over_window_cached_entry_count,
            over_window_cache_lookup_count,
            over_window_cache_miss_count,
            barrier_inflight_latency,
            barrier_sync_latency,
            barrier_manager_progress,
            sink_commit_duration,
            connector_sink_rows_received,
            lru_current_watermark_time_ms,
            lru_physical_now_ms,
            lru_runtime_loop_count,
            lru_watermark_step,
            lru_evicted_watermark_time_ms,
            jemalloc_allocated_bytes,
            jemalloc_active_bytes,
            user_compute_error_count,
            user_source_reader_error_count,
            materialize_cache_hit_count,
            materialize_cache_total_count,
            stream_memory_usage,
        }
    }

    /// Create a new `StreamingMetrics` instance used in tests or other places.
    pub fn unused() -> Self {
        global_streaming_metrics(MetricLevel::Disabled)
    }
}
