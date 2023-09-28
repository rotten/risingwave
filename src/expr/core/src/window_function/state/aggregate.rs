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

use std::collections::BTreeSet;

use futures_util::FutureExt;
use risingwave_common::array::{DataChunk, StreamChunk};
use risingwave_common::estimate_size::{EstimateSize, KvSize};
use risingwave_common::types::{DataType, Datum};
use risingwave_common::util::iter_util::ZipEqFast;
use risingwave_common::{bail, must_match};
use smallvec::SmallVec;

use super::buffer::WindowBuffer;
use super::{StateEvictHint, StateKey, StatePos, WindowState};
use crate::aggregate::{build_append_only, AggArgs, AggCall, BoxedAggregateFunction};
use crate::window_function::{WindowFuncCall, WindowFuncKind};
use crate::Result;

pub struct AggregateState {
    agg_call: AggCall,
    arg_data_types: Vec<DataType>,
    buffer: WindowBuffer<StateKey, SmallVec<[Datum; 2]>>,
    buffer_heap_size: KvSize,
}

impl AggregateState {
    pub fn new(call: &WindowFuncCall) -> Result<Self> {
        if !call.frame.bounds.is_valid() {
            bail!("the window frame must be valid");
        }
        let agg_kind = must_match!(call.kind, WindowFuncKind::Aggregate(agg_kind) => agg_kind);
        let arg_data_types = call.args.arg_types().to_vec();
        let agg_call = AggCall {
            kind: agg_kind,
            args: match &call.args {
                // convert args to [0] or [0, 1]
                AggArgs::None => AggArgs::None,
                AggArgs::Unary(data_type, _) => AggArgs::Unary(data_type.to_owned(), 0),
                AggArgs::Binary(data_types, _) => AggArgs::Binary(data_types.to_owned(), [0, 1]),
            },
            return_type: call.return_type.clone(),
            column_orders: Vec::new(), // the input is already sorted
            // TODO(rc): support filter on window function call
            filter: None,
            // TODO(rc): support distinct on window function call? PG doesn't support it either.
            distinct: false,
            direct_args: vec![],
        };
        Ok(Self {
            agg_call,
            arg_data_types,
            buffer: WindowBuffer::new(call.frame.clone()),
            buffer_heap_size: KvSize::new(),
        })
    }
}

impl WindowState for AggregateState {
    fn append(&mut self, key: StateKey, args: SmallVec<[Datum; 2]>) {
        args.iter().for_each(|arg| {
            self.buffer_heap_size.add_val(arg);
        });
        self.buffer_heap_size.add_val(&key);
        self.buffer.append(key, args);
    }

    fn curr_window(&self) -> StatePos<'_> {
        let window = self.buffer.curr_window();
        StatePos {
            key: window.key,
            is_ready: window.following_saturated,
        }
    }

    fn curr_output(&self) -> Result<Datum> {
        let wrapper = AggregatorWrapper {
            agg: build_append_only(&self.agg_call)?,
            arg_data_types: &self.arg_data_types,
        };
        wrapper.aggregate(self.buffer.curr_window_values().map(SmallVec::as_slice))
    }

    fn slide_forward(&mut self) -> StateEvictHint {
        let removed_keys: BTreeSet<_> = self
            .buffer
            .slide()
            .map(|(k, v)| {
                v.iter().for_each(|arg| {
                    self.buffer_heap_size.sub_val(arg);
                });
                self.buffer_heap_size.sub_val(&k);
                k
            })
            .collect();
        if removed_keys.is_empty() {
            StateEvictHint::CannotEvict(
                self.buffer
                    .smallest_key()
                    .expect("sliding without removing, must have some entry in the buffer")
                    .clone(),
            )
        } else {
            StateEvictHint::CanEvict(removed_keys)
        }
    }
}

impl EstimateSize for AggregateState {
    fn estimated_heap_size(&self) -> usize {
        // estimate `VecDeque` of `StreamWindowBuffer` internal size
        // https://github.com/risingwavelabs/risingwave/issues/9713
        self.arg_data_types.estimated_heap_size() + self.buffer_heap_size.size()
    }
}

struct AggregatorWrapper<'a> {
    agg: BoxedAggregateFunction,
    arg_data_types: &'a [DataType],
}

impl AggregatorWrapper<'_> {
    fn aggregate<'a>(&'a self, values: impl Iterator<Item = &'a [Datum]>) -> Result<Datum> {
        // TODO(rc): switch to a better general version of aggregator implementation

        let mut args_builders = self
            .arg_data_types
            .iter()
            .map(|data_type| data_type.create_array_builder(0 /* bad! */))
            .collect::<Vec<_>>();
        let mut n_values = 0;
        for value in values {
            n_values += 1;
            for (builder, datum) in args_builders.iter_mut().zip_eq_fast(value.iter()) {
                builder.append(datum);
            }
        }

        let columns = args_builders
            .into_iter()
            .map(|builder| builder.finish().into())
            .collect::<Vec<_>>();
        let chunk = StreamChunk::from(DataChunk::new(columns, n_values));

        let mut state = self.agg.create_state();
        self.agg
            .update(&mut state, &chunk)
            .now_or_never()
            .expect("we don't support UDAF currently, so the function should return immediately")?;
        self.agg
            .get_result(&state)
            .now_or_never()
            .expect("we don't support UDAF currently, so the function should return immediately")
    }
}

#[cfg(test)]
mod tests {
    // TODO(rc): need to add some unit tests
}
