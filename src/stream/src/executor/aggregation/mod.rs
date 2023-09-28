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

pub use agg_group::*;
pub use agg_state::*;
pub use distinct::*;
use risingwave_common::array::ArrayImpl::Bool;
use risingwave_common::array::DataChunk;
use risingwave_common::bail;
use risingwave_common::buffer::Bitmap;
use risingwave_common::catalog::{Field, Schema};
use risingwave_expr::aggregate::{AggCall, AggKind};
use risingwave_storage::StateStore;

use crate::common::table::state_table::StateTable;
use crate::executor::error::StreamExecutorResult;
use crate::executor::Executor;

mod agg_group;
mod agg_state;
mod agg_state_cache;
mod distinct;
mod minput;

/// Generate [`crate::executor::HashAggExecutor`]'s schema from `input`, `agg_calls` and
/// `group_key_indices`. For [`crate::executor::HashAggExecutor`], the group key indices should
/// be provided.
pub fn generate_agg_schema(
    input: &dyn Executor,
    agg_calls: &[AggCall],
    group_key_indices: Option<&[usize]>,
) -> Schema {
    let aggs = agg_calls
        .iter()
        .map(|agg| Field::unnamed(agg.return_type.clone()));

    let fields = if let Some(key_indices) = group_key_indices {
        let keys = key_indices
            .iter()
            .map(|idx| input.schema().fields[*idx].clone());

        keys.chain(aggs).collect()
    } else {
        aggs.collect()
    };

    Schema { fields }
}

pub async fn agg_call_filter_res(
    agg_call: &AggCall,
    chunk: &DataChunk,
) -> StreamExecutorResult<Bitmap> {
    let mut vis = chunk.visibility().clone();
    if matches!(
        agg_call.kind,
        AggKind::Min | AggKind::Max | AggKind::StringAgg
    ) {
        // should skip NULL value for these kinds of agg function
        let agg_col_idx = agg_call.args.val_indices()[0]; // the first arg is the agg column for all these kinds
        let agg_col_bitmap = chunk.column_at(agg_col_idx).null_bitmap();
        vis &= agg_col_bitmap;
    }

    if let Some(ref filter) = agg_call.filter {
        if let Bool(filter_res) = filter.eval_infallible(chunk).await.as_ref() {
            vis &= filter_res.to_bitmap();
        } else {
            bail!("Filter can only receive bool array");
        }
    }

    Ok(vis)
}

pub fn iter_table_storage<S>(
    state_storages: &mut [AggStateStorage<S>],
) -> impl Iterator<Item = &mut StateTable<S>>
where
    S: StateStore,
{
    state_storages
        .iter_mut()
        .filter_map(|storage| match storage {
            AggStateStorage::Value => None,
            AggStateStorage::MaterializedInput { table, .. } => Some(table),
        })
}
