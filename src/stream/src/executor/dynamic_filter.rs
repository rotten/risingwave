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

use std::ops::Bound::{self, *};
use std::sync::Arc;

use futures::{pin_mut, stream, StreamExt};
use futures_async_stream::try_stream;
use risingwave_common::array::{Array, ArrayImpl, Op, StreamChunk};
use risingwave_common::bail;
use risingwave_common::buffer::{Bitmap, BitmapBuilder};
use risingwave_common::catalog::Schema;
use risingwave_common::hash::VnodeBitmapExt;
use risingwave_common::row::{once, OwnedRow as RowData, Row};
use risingwave_common::types::{DataType, Datum, DefaultOrd, ScalarImpl, ToDatumRef, ToOwnedDatum};
use risingwave_common::util::iter_util::ZipEqDebug;
use risingwave_expr::expr::{
    build_func_non_strict, BoxedExpression, InputRefExpression, LiteralExpression,
};
use risingwave_pb::expr::expr_node::Type as ExprNodeType;
use risingwave_pb::expr::expr_node::Type::{
    GreaterThan, GreaterThanOrEqual, LessThan, LessThanOrEqual,
};
use risingwave_storage::store::PrefetchOptions;
use risingwave_storage::StateStore;

use super::barrier_align::*;
use super::error::StreamExecutorError;
use super::monitor::StreamingMetrics;
use super::{
    ActorContextRef, BoxedExecutor, BoxedMessageStream, Executor, Message, PkIndices, PkIndicesRef,
};
use crate::common::table::state_table::{StateTable, WatermarkCacheParameterizedStateTable};
use crate::common::StreamChunkBuilder;
use crate::executor::expect_first_barrier_from_aligned_stream;
use crate::task::ActorEvalErrorReport;

pub struct DynamicFilterExecutor<S: StateStore, const USE_WATERMARK_CACHE: bool> {
    ctx: ActorContextRef,
    source_l: Option<BoxedExecutor>,
    source_r: Option<BoxedExecutor>,
    key_l: usize,
    pk_indices: PkIndices,
    identity: String,
    comparator: ExprNodeType,
    left_table: WatermarkCacheParameterizedStateTable<S, USE_WATERMARK_CACHE>,
    right_table: StateTable<S>,
    schema: Schema,
    metrics: Arc<StreamingMetrics>,
    /// The maximum size of the chunk produced by executor at a time.
    chunk_size: usize,
}

impl<S: StateStore, const USE_WATERMARK_CACHE: bool> DynamicFilterExecutor<S, USE_WATERMARK_CACHE> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        ctx: ActorContextRef,
        source_l: BoxedExecutor,
        source_r: BoxedExecutor,
        key_l: usize,
        pk_indices: PkIndices,
        executor_id: u64,
        comparator: ExprNodeType,
        state_table_l: WatermarkCacheParameterizedStateTable<S, USE_WATERMARK_CACHE>,
        state_table_r: StateTable<S>,
        metrics: Arc<StreamingMetrics>,
        chunk_size: usize,
    ) -> Self {
        let schema = source_l.schema().clone();
        Self {
            ctx,
            source_l: Some(source_l),
            source_r: Some(source_r),
            key_l,
            pk_indices,
            identity: format!("DynamicFilterExecutor {:X}", executor_id),
            comparator,
            left_table: state_table_l,
            right_table: state_table_r,
            metrics,
            schema,
            chunk_size,
        }
    }

    async fn apply_batch(
        &mut self,
        chunk: &StreamChunk,
        condition: Option<BoxedExpression>,
    ) -> Result<(Vec<Op>, Bitmap), StreamExecutorError> {
        let mut new_ops = Vec::with_capacity(chunk.capacity());
        let mut new_visibility = BitmapBuilder::with_capacity(chunk.capacity());
        let mut last_res = false;

        let eval_results = if let Some(cond) = condition {
            Some(cond.eval_infallible(chunk).await)
        } else {
            None
        };

        for (idx, (op, row)) in chunk.rows().enumerate() {
            let left_val = row.datum_at(self.key_l).to_owned_datum();

            let res = if let Some(array) = &eval_results {
                if let ArrayImpl::Bool(results) = &**array {
                    results.value_at(idx).unwrap_or(false)
                } else {
                    panic!("condition eval must return bool array")
                }
            } else {
                // A NULL right value implies a false evaluation for all rows
                false
            };

            match op {
                Op::Insert | Op::Delete => {
                    new_ops.push(op);
                    if res {
                        new_visibility.append(true);
                    } else {
                        new_visibility.append(false);
                    }
                }
                Op::UpdateDelete => {
                    last_res = res;
                }
                Op::UpdateInsert => match (last_res, res) {
                    (true, false) => {
                        new_ops.push(Op::Delete);
                        new_ops.push(Op::UpdateInsert);
                        new_visibility.append(true);
                        new_visibility.append(false);
                    }
                    (false, true) => {
                        new_ops.push(Op::UpdateDelete);
                        new_ops.push(Op::Insert);
                        new_visibility.append(false);
                        new_visibility.append(true);
                    }
                    (true, true) => {
                        new_ops.push(Op::UpdateDelete);
                        new_ops.push(Op::UpdateInsert);
                        new_visibility.append(true);
                        new_visibility.append(true);
                    }
                    (false, false) => {
                        new_ops.push(Op::UpdateDelete);
                        new_ops.push(Op::UpdateInsert);
                        new_visibility.append(false);
                        new_visibility.append(false);
                    }
                },
            }

            // Store the rows without a null left key
            // null key in left side of predicate should never be stored
            // (it will never satisfy the filter condition)
            if left_val.is_some() {
                match op {
                    Op::Insert | Op::UpdateInsert => {
                        self.left_table.insert(row);
                    }
                    Op::Delete | Op::UpdateDelete => {
                        self.left_table.delete(row);
                    }
                }
            }
        }

        let new_visibility = new_visibility.finish();

        Ok((new_ops, new_visibility))
    }

    /// Returns the required range, whether the latest value is in lower bound (rather than upper)
    /// and whether to insert or delete the range.
    fn get_range(
        &self,
        curr: &Datum,
        prev: Datum,
    ) -> ((Bound<ScalarImpl>, Bound<ScalarImpl>), bool, bool) {
        debug_assert_ne!(curr, &prev);
        let curr_is_some = curr.is_some();
        match (curr.clone(), prev) {
            (Some(c), None) | (None, Some(c)) => {
                let range = match self.comparator {
                    GreaterThan => (Excluded(c), Unbounded),
                    GreaterThanOrEqual => (Included(c), Unbounded),
                    LessThan => (Unbounded, Excluded(c)),
                    LessThanOrEqual => (Unbounded, Included(c)),
                    _ => unreachable!(),
                };
                let is_insert = curr_is_some;
                // The new bound is always towards the last known value
                let is_lower = matches!(self.comparator, GreaterThan | GreaterThanOrEqual);
                (range, is_lower, is_insert)
            }
            (Some(c), Some(p)) => {
                if c.default_cmp(&p).is_lt() {
                    let range = match self.comparator {
                        GreaterThan | LessThanOrEqual => (Excluded(c), Included(p)),
                        GreaterThanOrEqual | LessThan => (Included(c), Excluded(p)),
                        _ => unreachable!(),
                    };
                    let is_insert = matches!(self.comparator, GreaterThan | GreaterThanOrEqual);
                    (range, true, is_insert)
                } else {
                    // c > p
                    let range = match self.comparator {
                        GreaterThan | LessThanOrEqual => (Excluded(p), Included(c)),
                        GreaterThanOrEqual | LessThan => (Included(p), Excluded(c)),
                        _ => unreachable!(),
                    };
                    let is_insert = matches!(self.comparator, LessThan | LessThanOrEqual);
                    (range, false, is_insert)
                }
            }
            (None, None) => unreachable!(), // prev != curr
        }
    }

    async fn recover_rhs(&mut self) -> Result<Option<RowData>, StreamExecutorError> {
        // Recover value for RHS if available
        let rhs_stream = self.right_table.iter_row(Default::default()).await?;
        pin_mut!(rhs_stream);

        if let Some(res) = rhs_stream.next().await {
            let value = res?.into_owned_row();
            assert!(rhs_stream.next().await.is_none());
            Ok(Some(value))
        } else {
            Ok(None)
        }
    }

    fn to_row_bound(bound: Bound<ScalarImpl>) -> Bound<impl Row> {
        bound.map(|s| once(Some(s)))
    }

    #[try_stream(ok = Message, error = StreamExecutorError)]
    async fn into_stream(mut self) {
        let input_l = self.source_l.take().unwrap();
        let input_r = self.source_r.take().unwrap();

        // Derive the dynamic expression
        let l_data_type = input_l.schema().data_types()[self.key_l].clone();
        let r_data_type = input_r.schema().data_types()[0].clone();
        // The types are aligned by frontend.
        assert_eq!(l_data_type, r_data_type);
        let dynamic_cond = {
            let eval_error_report = ActorEvalErrorReport {
                actor_context: self.ctx.clone(),
                identity: self.identity.as_str().into(),
            };
            move |literal: Datum| {
                literal.map(|scalar| {
                    build_func_non_strict(
                        self.comparator,
                        DataType::Boolean,
                        vec![
                            Box::new(InputRefExpression::new(l_data_type.clone(), self.key_l)),
                            Box::new(LiteralExpression::new(r_data_type.clone(), Some(scalar))),
                        ],
                        eval_error_report.clone(),
                    )
                })
            }
        };

        let aligned_stream = barrier_align(
            input_l.execute(),
            input_r.execute(),
            self.ctx.id,
            self.ctx.fragment_id,
            self.metrics.clone(),
        );

        pin_mut!(aligned_stream);

        let barrier = expect_first_barrier_from_aligned_stream(&mut aligned_stream).await?;
        self.right_table.init_epoch(barrier.epoch);
        self.left_table.init_epoch(barrier.epoch);

        let recovered_row = self.recover_rhs().await?;
        let recovered_value = recovered_row.as_ref().map(|r| r[0].clone());
        // At the beginning of an epoch, the `prev_epoch_value` == `current_epoch_value`
        let mut prev_epoch_value: Option<Datum> = recovered_value.clone();
        let mut current_epoch_value: Option<Datum> = recovered_value;
        // This is only required to be some if the row arrived during this epoch.
        let mut current_epoch_row = recovered_row.clone();
        let mut last_committed_epoch_row = recovered_row;

        // The first barrier message should be propagated.
        yield Message::Barrier(barrier);

        let mut stream_chunk_builder =
            StreamChunkBuilder::new(self.chunk_size, self.schema.data_types());

        let watermark_can_clean_state = !matches!(self.comparator, LessThan | LessThanOrEqual);
        let mut unused_clean_hint = None;

        #[for_await]
        for msg in aligned_stream {
            match msg? {
                AlignedMessage::Left(chunk) => {
                    // Reuse the logic from `FilterExecutor`
                    let chunk = chunk.compact(); // Is this unnecessary work?
                    let right_val = prev_epoch_value.clone().flatten();

                    // The condition is `None` if it is always false by virtue of a NULL right
                    // input, so we save evaluating it on the datachunk
                    let condition = dynamic_cond(right_val).transpose()?;

                    let (new_ops, new_visibility) = self.apply_batch(&chunk, condition).await?;

                    let columns = chunk.into_parts().0.into_parts().0;

                    if new_visibility.count_ones() > 0 {
                        let new_chunk =
                            StreamChunk::with_visibility(new_ops, columns, new_visibility);
                        yield Message::Chunk(new_chunk)
                    }
                }
                AlignedMessage::Right(chunk) => {
                    // Record the latest update to the right value
                    let chunk = chunk.compact(); // Is this unnecessary work?
                    let (data_chunk, ops) = chunk.into_parts();

                    for (row, op) in data_chunk.rows().zip_eq_debug(ops.iter()) {
                        match *op {
                            Op::UpdateInsert | Op::Insert => {
                                current_epoch_value = Some(row.datum_at(0).to_owned_datum());
                                current_epoch_row = Some(row.into_owned_row());
                            }
                            _ => {
                                // To be consistent, there must be an existing `current_epoch_value`
                                // equivalent to row indicated for
                                // deletion.
                                if Some(row.datum_at(0))
                                    != current_epoch_value.as_ref().map(ToDatumRef::to_datum_ref)
                                {
                                    bail!(
                                        "Inconsistent Delete - current: {:?}, delete: {:?}",
                                        current_epoch_value,
                                        row
                                    );
                                }
                                current_epoch_value = None;
                                current_epoch_row = None;
                            }
                        }
                    }
                }
                AlignedMessage::WatermarkLeft(_) => {
                    // Do nothing.
                }
                AlignedMessage::WatermarkRight(watermark) => {
                    if watermark_can_clean_state {
                        unused_clean_hint = Some(watermark);
                    }
                }
                AlignedMessage::Barrier(barrier) => {
                    // Flush the difference between the `prev_value` and `current_value`
                    //
                    // This block is guaranteed to be idempotent even if we may encounter multiple
                    // barriers since `prev_epoch_value` is always be reset to
                    // the equivalent of `current_epoch_value` at the end of
                    // this block. Likewise, `last_committed_epoch_row` will always be equal to
                    // `current_epoch_row`.
                    // It is thus guaranteed not to commit state or produce chunks as long as
                    // no new chunks have arrived since the previous barrier.
                    let curr: Datum = current_epoch_value.clone().flatten();
                    let prev: Datum = prev_epoch_value.flatten();
                    if prev != curr {
                        let (range, _latest_is_lower, is_insert) = self.get_range(&curr, prev);
                        let range = (Self::to_row_bound(range.0), Self::to_row_bound(range.1));

                        // TODO: prefetching for append-only case.
                        let streams = futures::future::try_join_all(
                            self.left_table.vnodes().iter_vnodes().map(|vnode| {
                                self.left_table.iter_row_with_pk_range(
                                    &range,
                                    vnode,
                                    PrefetchOptions::new_for_exhaust_iter(),
                                )
                            }),
                        )
                        .await?
                        .into_iter()
                        .map(Box::pin);

                        #[for_await]
                        for res in stream::select_all(streams) {
                            let row = res?;
                            if let Some(chunk) = stream_chunk_builder.append_row(
                                // All rows have a single identity at this point
                                if is_insert { Op::Insert } else { Op::Delete },
                                row.as_ref(),
                            ) {
                                yield Message::Chunk(chunk);
                            }
                        }

                        if let Some(chunk) = stream_chunk_builder.take() {
                            yield Message::Chunk(chunk);
                        }
                    }

                    if let Some(mut watermark) = unused_clean_hint.take() {
                        self.left_table
                            .update_watermark(watermark.val.clone(), false);
                        watermark.col_idx = self.key_l;
                        yield Message::Watermark(watermark);
                    };

                    // Update the committed value on RHS if it has changed.
                    if last_committed_epoch_row != current_epoch_row {
                        // Only write the RHS value if this actor is in charge of vnode 0 on LHS
                        // Otherwise, we only actively replicate the changes.
                        if self.left_table.vnode_bitmap().is_set(0) {
                            // If both `None`, then this branch is inactive.
                            // Hence, at least one is `Some`, hence at least one update.
                            if let Some(old_row) = last_committed_epoch_row.take() {
                                self.right_table.delete(old_row);
                            }
                            if let Some(row) = &current_epoch_row {
                                self.right_table.insert(row);
                            }
                            self.right_table.commit(barrier.epoch).await?;
                        } else {
                            self.right_table.commit_no_data_expected(barrier.epoch);
                        }
                        // Update the last committed row since it has changed
                        last_committed_epoch_row = current_epoch_row.clone();
                    } else {
                        self.right_table.commit_no_data_expected(barrier.epoch);
                    }

                    self.left_table.commit(barrier.epoch).await?;

                    prev_epoch_value = Some(curr);

                    debug_assert_eq!(last_committed_epoch_row, current_epoch_row);

                    // Update the vnode bitmap for the left state table if asked.
                    if let Some(vnode_bitmap) = barrier.as_update_vnode_bitmap(self.ctx.id) {
                        let (_previous_vnode_bitmap, _cache_may_stale) =
                            self.left_table.update_vnode_bitmap(vnode_bitmap);
                    }

                    yield Message::Barrier(barrier);
                }
            }
        }
    }
}

impl<S: StateStore, const USE_WATERMARK_CACHE: bool> Executor
    for DynamicFilterExecutor<S, USE_WATERMARK_CACHE>
{
    fn execute(self: Box<Self>) -> BoxedMessageStream {
        self.into_stream().boxed()
    }

    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn pk_indices(&self) -> PkIndicesRef<'_> {
        &self.pk_indices
    }

    fn identity(&self) -> &str {
        self.identity.as_str()
    }
}

#[cfg(test)]
mod tests {
    use risingwave_common::array::stream_chunk::StreamChunkTestExt;
    use risingwave_common::array::*;
    use risingwave_common::catalog::{ColumnDesc, ColumnId, Field, Schema, TableId};
    use risingwave_common::util::sort_util::OrderType;
    use risingwave_storage::memory::MemoryStateStore;

    use super::*;
    use crate::executor::test_utils::{MessageSender, MockSource, StreamExecutorTestExt};
    use crate::executor::{ActorContext, StreamExecutorResult};

    async fn create_in_memory_state_table(
        mem_state: MemoryStateStore,
    ) -> (StateTable<MemoryStateStore>, StateTable<MemoryStateStore>) {
        let column_descs = ColumnDesc::unnamed(ColumnId::new(0), DataType::Int64);
        // TODO: use consistent operations for dynamic filter <https://github.com/risingwavelabs/risingwave/issues/3893>
        let state_table_l = StateTable::new_without_distribution_inconsistent_op(
            mem_state.clone(),
            TableId::new(0),
            vec![column_descs.clone()],
            vec![OrderType::ascending()],
            vec![0],
        )
        .await;
        let state_table_r = StateTable::new_without_distribution_inconsistent_op(
            mem_state,
            TableId::new(1),
            vec![column_descs],
            vec![OrderType::ascending()],
            vec![0],
        )
        .await;
        (state_table_l, state_table_r)
    }

    async fn create_executor(
        comparator: ExprNodeType,
    ) -> (MessageSender, MessageSender, BoxedMessageStream) {
        let mem_state = MemoryStateStore::new();
        create_executor_inner(comparator, mem_state).await
    }

    async fn create_executor_inner(
        comparator: ExprNodeType,
        mem_state: MemoryStateStore,
    ) -> (MessageSender, MessageSender, BoxedMessageStream) {
        let (mem_state_l, mem_state_r) = create_in_memory_state_table(mem_state).await;
        let schema = Schema {
            fields: vec![Field::unnamed(DataType::Int64)],
        };
        let (tx_l, source_l) = MockSource::channel(schema.clone(), vec![0]);
        let (tx_r, source_r) = MockSource::channel(schema, vec![]);

        let executor = DynamicFilterExecutor::<MemoryStateStore, false>::new(
            ActorContext::create(123),
            Box::new(source_l),
            Box::new(source_r),
            0,
            vec![0],
            1,
            comparator,
            mem_state_l,
            mem_state_r,
            Arc::new(StreamingMetrics::unused()),
            1024,
        );
        (tx_l, tx_r, Box::new(executor).execute())
    }

    #[tokio::test]
    async fn test_dynamic_filter_rhs_recovery_gt() -> StreamExecutorResult<()> {
        let chunk_l1 = StreamChunk::from_pretty(
            "  I
             + 1
             + 2
             + 3",
        );
        let chunk_l2 = StreamChunk::from_pretty(
            "  I
             + 4
             - 3",
        );
        let chunk_r0 = StreamChunk::from_pretty(
            "  I
             + 1",
        );
        let chunk_r1 = StreamChunk::from_pretty(
            "  I
             - 1
             + 2",
        );
        let chunk_r2 = StreamChunk::from_pretty(
            "  I
             + 1",
        );
        let chunk_r3 = StreamChunk::from_pretty(
            "  I
             + 4",
        );
        let mem_state = MemoryStateStore::new();
        let (mut tx_l, mut tx_r, mut dynamic_filter) =
            create_executor_inner(ExprNodeType::GreaterThan, mem_state.clone()).await;

        // push the init barrier for left and right
        tx_l.push_barrier(1, false);
        tx_r.push_barrier(1, false);
        dynamic_filter.next_unwrap_ready_barrier()?;

        // push the 0th right chunk
        tx_r.push_chunk(chunk_r0);

        tx_l.push_barrier(2, false);
        tx_r.push_barrier(2, false);

        // Get the barrier
        dynamic_filter.next_unwrap_ready_barrier()?;

        // Drop executor corresponding to node failure
        drop(tx_l);
        drop(tx_r);
        drop(dynamic_filter);

        // Recover executor from state store
        let (mut tx_l, mut tx_r, mut dynamic_filter) =
            create_executor_inner(ExprNodeType::GreaterThan, mem_state.clone()).await;

        // push the recovery barrier for left and right
        tx_l.push_barrier(2, false);
        tx_r.push_barrier(2, false);

        // Get recovery barrier
        dynamic_filter.next_unwrap_ready_barrier()?;

        // push the 1st right chunk
        tx_r.push_chunk(chunk_r1);
        // push the 1st left chunk
        tx_l.push_chunk(chunk_l1);

        // push the init barrier for left and right
        tx_l.push_barrier(3, false);
        tx_r.push_barrier(3, false);

        let chunk = dynamic_filter.next_unwrap_ready_chunk()?.compact();
        assert_eq!(
            chunk,
            StreamChunk::from_pretty(
                " I
                + 2
                + 3"
            )
        );

        let chunk = dynamic_filter.next_unwrap_ready_chunk()?.compact();
        assert_eq!(
            chunk,
            StreamChunk::from_pretty(
                " I
                - 2"
            )
        );

        // Get the barrier
        dynamic_filter.next_unwrap_ready_barrier()?;

        // Drop executor corresponding to node failure
        drop(tx_l);
        drop(tx_r);
        drop(dynamic_filter);

        // Recover executor from state store
        let (mut tx_l, mut tx_r, mut dynamic_filter) =
            create_executor_inner(ExprNodeType::GreaterThan, mem_state.clone()).await;

        // push recovery barrier
        tx_l.push_barrier(3, false);
        tx_r.push_barrier(3, false);

        // Get the barrier
        dynamic_filter.next_unwrap_ready_barrier()?;

        // push the 2nd left chunk
        tx_l.push_chunk(chunk_l2);
        let chunk = dynamic_filter.next_unwrap_ready_chunk()?.compact();
        assert_eq!(
            chunk,
            StreamChunk::from_pretty(
                " I
                + 4
                - 3"
            )
        );

        // push the 2nd right chunk
        tx_r.push_chunk(chunk_r2);

        // push the init barrier for left and right
        tx_l.push_barrier(4, false);
        tx_r.push_barrier(4, false);

        let chunk = dynamic_filter.next_unwrap_ready_chunk()?.compact();
        assert_eq!(
            chunk,
            StreamChunk::from_pretty(
                " I
                + 2"
            )
        );

        // Get the barrier
        dynamic_filter.next_unwrap_ready_barrier()?;

        // push the 3rd right chunk
        tx_r.push_chunk(chunk_r3);

        // push the init barrier for left and right
        tx_l.push_barrier(5, false);
        tx_r.push_barrier(5, false);

        let chunk = dynamic_filter.next_unwrap_ready_chunk()?.compact();
        assert_eq!(
            chunk,
            StreamChunk::from_pretty(
                " I
                - 2
                - 4"
            )
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_dynamic_filter_greater_than() -> StreamExecutorResult<()> {
        let chunk_l1 = StreamChunk::from_pretty(
            "  I
             + 1
             + 2
             + 3",
        );
        let chunk_l2 = StreamChunk::from_pretty(
            "  I
             + 4
             - 3",
        );
        let chunk_r1 = StreamChunk::from_pretty(
            "  I
             + 2",
        );
        let chunk_r2 = StreamChunk::from_pretty(
            "  I
             + 1",
        );
        let chunk_r3 = StreamChunk::from_pretty(
            "  I
             + 4",
        );
        let (mut tx_l, mut tx_r, mut dynamic_filter) =
            create_executor(ExprNodeType::GreaterThan).await;

        // push the init barrier for left and right
        tx_l.push_barrier(1, false);
        tx_r.push_barrier(1, false);
        dynamic_filter.next_unwrap_ready_barrier()?;

        // push the 1st left chunk
        tx_l.push_chunk(chunk_l1);

        // push the 1st right chunk
        tx_r.push_chunk(chunk_r1);

        // push the init barrier for left and right
        tx_l.push_barrier(2, false);
        tx_r.push_barrier(2, false);

        let chunk = dynamic_filter.next_unwrap_ready_chunk()?.compact();
        assert_eq!(
            chunk,
            StreamChunk::from_pretty(
                " I
                + 3"
            )
        );

        // Get the barrier
        dynamic_filter.next_unwrap_ready_barrier()?;

        // push the 2nd left chunk
        tx_l.push_chunk(chunk_l2);
        let chunk = dynamic_filter.next_unwrap_ready_chunk()?.compact();
        assert_eq!(
            chunk,
            StreamChunk::from_pretty(
                " I
                + 4
                - 3"
            )
        );

        // push the 2nd right chunk
        tx_r.push_chunk(chunk_r2);

        // push the init barrier for left and right
        tx_l.push_barrier(3, false);
        tx_r.push_barrier(3, false);

        let chunk = dynamic_filter.next_unwrap_ready_chunk()?.compact();
        assert_eq!(
            chunk,
            StreamChunk::from_pretty(
                " I
                + 2"
            )
        );

        // Get the barrier
        dynamic_filter.next_unwrap_ready_barrier()?;

        // push the 3rd right chunk
        tx_r.push_chunk(chunk_r3);

        // push the init barrier for left and right
        tx_l.push_barrier(4, false);
        tx_r.push_barrier(4, false);

        let chunk = dynamic_filter.next_unwrap_ready_chunk()?.compact();
        assert_eq!(
            chunk,
            StreamChunk::from_pretty(
                " I
                - 2
                - 4"
            )
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_dynamic_filter_greater_than_or_equal() -> StreamExecutorResult<()> {
        let chunk_l1 = StreamChunk::from_pretty(
            "  I
             + 1
             + 2
             + 3",
        );
        let chunk_l2 = StreamChunk::from_pretty(
            "  I
             + 4
             - 3",
        );
        let chunk_r1 = StreamChunk::from_pretty(
            "  I
             + 3",
        );
        let chunk_r2 = StreamChunk::from_pretty(
            "  I
             + 2",
        );
        let chunk_r3 = StreamChunk::from_pretty(
            "  I
             + 5",
        );
        let (mut tx_l, mut tx_r, mut dynamic_filter) =
            create_executor(ExprNodeType::GreaterThanOrEqual).await;

        // push the init barrier for left and right
        tx_l.push_barrier(1, false);
        tx_r.push_barrier(1, false);
        dynamic_filter.next_unwrap_ready_barrier()?;

        // push the 1st left chunk
        tx_l.push_chunk(chunk_l1);

        // push the 1st right chunk
        tx_r.push_chunk(chunk_r1);

        // push the init barrier for left and right
        tx_l.push_barrier(2, false);
        tx_r.push_barrier(2, false);

        let chunk = dynamic_filter.next_unwrap_ready_chunk()?.compact();
        assert_eq!(
            chunk,
            StreamChunk::from_pretty(
                " I
                + 3"
            )
        );

        // Get the barrier
        dynamic_filter.next_unwrap_ready_barrier()?;

        // push the 2nd left chunk
        tx_l.push_chunk(chunk_l2);
        let chunk = dynamic_filter.next_unwrap_ready_chunk()?.compact();
        assert_eq!(
            chunk,
            StreamChunk::from_pretty(
                " I
                + 4
                - 3"
            )
        );

        // push the 2nd right chunk
        tx_r.push_chunk(chunk_r2);

        // push the init barrier for left and right
        tx_l.push_barrier(3, false);
        tx_r.push_barrier(3, false);

        let chunk = dynamic_filter.next_unwrap_ready_chunk()?.compact();
        assert_eq!(
            chunk,
            StreamChunk::from_pretty(
                " I
                + 2"
            )
        );

        // Get the barrier
        dynamic_filter.next_unwrap_ready_barrier()?;

        // push the 3rd right chunk
        tx_r.push_chunk(chunk_r3);

        // push the init barrier for left and right
        tx_l.push_barrier(4, false);
        tx_r.push_barrier(4, false);

        let chunk = dynamic_filter.next_unwrap_ready_chunk()?.compact();
        assert_eq!(
            chunk,
            StreamChunk::from_pretty(
                " I
                - 2
                - 4"
            )
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_dynamic_filter_less_than() -> StreamExecutorResult<()> {
        let chunk_l1 = StreamChunk::from_pretty(
            "  I
             + 2
             + 3
             + 4",
        );
        let chunk_l2 = StreamChunk::from_pretty(
            "  I
             + 1
             - 2",
        );
        let chunk_r1 = StreamChunk::from_pretty(
            "  I
             + 3",
        );
        let chunk_r2 = StreamChunk::from_pretty(
            "  I
             + 4",
        );
        let chunk_r3 = StreamChunk::from_pretty(
            "  I
             + 1",
        );
        let (mut tx_l, mut tx_r, mut dynamic_filter) =
            create_executor(ExprNodeType::LessThan).await;

        // push the init barrier for left and right
        tx_l.push_barrier(1, false);
        tx_r.push_barrier(1, false);
        dynamic_filter.next_unwrap_ready_barrier()?;

        // push the 1st left chunk
        tx_l.push_chunk(chunk_l1);

        // push the 1st right chunk
        tx_r.push_chunk(chunk_r1);

        // push the init barrier for left and right
        tx_l.push_barrier(2, false);
        tx_r.push_barrier(2, false);

        let chunk = dynamic_filter.next_unwrap_ready_chunk()?.compact();
        assert_eq!(
            chunk,
            StreamChunk::from_pretty(
                " I
                + 2"
            )
        );

        // Get the barrier
        dynamic_filter.next_unwrap_ready_barrier()?;

        // push the 2nd left chunk
        tx_l.push_chunk(chunk_l2);
        let chunk = dynamic_filter.next_unwrap_ready_chunk()?.compact();
        assert_eq!(
            chunk,
            StreamChunk::from_pretty(
                " I
                + 1
                - 2"
            )
        );

        // push the 2nd right chunk
        tx_r.push_chunk(chunk_r2);

        // push the init barrier for left and right
        tx_l.push_barrier(3, false);
        tx_r.push_barrier(3, false);

        let chunk = dynamic_filter.next_unwrap_ready_chunk()?.compact();
        assert_eq!(
            chunk,
            StreamChunk::from_pretty(
                " I
                + 3"
            )
        );

        // Get the barrier
        dynamic_filter.next_unwrap_ready_barrier()?;

        // push the 3rd right chunk
        tx_r.push_chunk(chunk_r3);

        // push the init barrier for left and right
        tx_l.push_barrier(4, false);
        tx_r.push_barrier(4, false);

        let chunk = dynamic_filter.next_unwrap_ready_chunk()?.compact();
        assert_eq!(
            chunk,
            StreamChunk::from_pretty(
                " I
                - 1
                - 3"
            )
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_dynamic_filter_less_than_or_equal() -> StreamExecutorResult<()> {
        let chunk_l1 = StreamChunk::from_pretty(
            "  I
             + 2
             + 3
             + 4",
        );
        let chunk_l2 = StreamChunk::from_pretty(
            "  I
             + 1
             - 2",
        );
        let chunk_r1 = StreamChunk::from_pretty(
            "  I
             + 2",
        );
        let chunk_r2 = StreamChunk::from_pretty(
            "  I
             + 3",
        );
        let chunk_r3 = StreamChunk::from_pretty(
            "  I
             + 0",
        );
        let (mut tx_l, mut tx_r, mut dynamic_filter) =
            create_executor(ExprNodeType::LessThanOrEqual).await;

        // push the init barrier for left and right
        tx_l.push_barrier(1, false);
        tx_r.push_barrier(1, false);
        dynamic_filter.next_unwrap_ready_barrier()?;

        // push the 1st left chunk
        tx_l.push_chunk(chunk_l1);

        // push the 1st right chunk
        tx_r.push_chunk(chunk_r1);

        // push the init barrier for left and right
        tx_l.push_barrier(2, false);
        tx_r.push_barrier(2, false);

        let chunk = dynamic_filter.next_unwrap_ready_chunk()?.compact();
        assert_eq!(
            chunk,
            StreamChunk::from_pretty(
                " I
                + 2"
            )
        );

        // Get the barrier
        dynamic_filter.next_unwrap_ready_barrier()?;

        // push the 2nd left chunk
        tx_l.push_chunk(chunk_l2);
        let chunk = dynamic_filter.next_unwrap_ready_chunk()?.compact();
        assert_eq!(
            chunk,
            StreamChunk::from_pretty(
                " I
                + 1
                - 2"
            )
        );

        // push the 2nd right chunk
        tx_r.push_chunk(chunk_r2);

        // push the init barrier for left and right
        tx_l.push_barrier(3, false);
        tx_r.push_barrier(3, false);

        let chunk = dynamic_filter.next_unwrap_ready_chunk()?.compact();
        assert_eq!(
            chunk,
            StreamChunk::from_pretty(
                " I
                + 3"
            )
        );

        // Get the barrier
        dynamic_filter.next_unwrap_ready_barrier()?;

        // push the 3rd right chunk
        tx_r.push_chunk(chunk_r3);

        // push the init barrier for left and right
        tx_l.push_barrier(4, false);
        tx_r.push_barrier(4, false);

        let chunk = dynamic_filter.next_unwrap_ready_chunk()?.compact();
        assert_eq!(
            chunk,
            StreamChunk::from_pretty(
                " I
                - 1
                - 3"
            )
        );

        Ok(())
    }
}
