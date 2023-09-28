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

use educe::Educe;
use fixedbitset::FixedBitSet;
use paste::paste;
use risingwave_common::catalog::Schema;

use super::generic::GenericPlanNode;
use super::*;
use crate::for_all_plan_nodes;
use crate::optimizer::optimizer_context::OptimizerContextRef;
use crate::optimizer::property::{Distribution, FunctionalDependencySet, Order};

/// the common fields of all nodes, please make a field named `base` in
/// every planNode and correctly value it when construct the planNode.
#[derive(Clone, Debug, Educe)]
#[educe(PartialEq, Eq, Hash)]
pub struct PlanBase {
    #[educe(PartialEq(ignore))]
    #[educe(Hash(ignore))]
    pub id: PlanNodeId,
    #[educe(PartialEq(ignore))]
    #[educe(Hash(ignore))]
    pub ctx: OptimizerContextRef,
    pub schema: Schema,
    /// the pk indices of the PlanNode's output, a empty stream key vec means there is no stream key
    pub stream_key: Vec<usize>,
    /// The order property of the PlanNode's output, store an `&Order::any()` here will not affect
    /// correctness, but insert unnecessary sort in plan
    pub order: Order,
    /// The distribution property of the PlanNode's output, store an `Distribution::any()` here
    /// will not affect correctness, but insert unnecessary exchange in plan
    pub dist: Distribution,
    /// The append-only property of the PlanNode's output is a stream-only property. Append-only
    /// means the stream contains only insert operation.
    pub append_only: bool,
    /// Whether the output is emitted on window close.
    pub emit_on_window_close: bool,
    pub functional_dependency: FunctionalDependencySet,
    /// The watermark column indices of the PlanNode's output. There could be watermark output from
    /// this stream operator.
    pub watermark_columns: FixedBitSet,
}

impl generic::GenericPlanRef for PlanBase {
    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn stream_key(&self) -> &[usize] {
        &self.stream_key
    }

    fn ctx(&self) -> OptimizerContextRef {
        self.ctx.clone()
    }

    fn functional_dependency(&self) -> &FunctionalDependencySet {
        &self.functional_dependency
    }
}

impl stream::StreamPlanRef for PlanBase {
    fn distribution(&self) -> &Distribution {
        &self.dist
    }

    fn append_only(&self) -> bool {
        self.append_only
    }

    fn emit_on_window_close(&self) -> bool {
        self.emit_on_window_close
    }
}
impl batch::BatchPlanRef for PlanBase {
    fn order(&self) -> &Order {
        &self.order
    }
}
impl PlanBase {
    pub fn new_logical(
        ctx: OptimizerContextRef,
        schema: Schema,
        stream_key: Vec<usize>,
        functional_dependency: FunctionalDependencySet,
    ) -> Self {
        let id = ctx.next_plan_node_id();
        let watermark_columns = FixedBitSet::with_capacity(schema.len());
        Self {
            id,
            ctx,
            schema,
            stream_key,
            dist: Distribution::Single,
            order: Order::any(),
            // Logical plan node won't touch `append_only` field
            append_only: true,
            emit_on_window_close: false,
            functional_dependency,
            watermark_columns,
        }
    }

    pub fn new_logical_with_core(node: &impl GenericPlanNode) -> Self {
        Self::new_logical(
            node.ctx(),
            node.schema(),
            node.stream_key().unwrap_or_default(),
            node.functional_dependency(),
        )
    }

    pub fn new_stream_with_logical(
        logical: &impl GenericPlanNode,
        dist: Distribution,
        append_only: bool,
        emit_on_window_close: bool,
        watermark_columns: FixedBitSet,
    ) -> Self {
        Self::new_stream(
            logical.ctx(),
            logical.schema(),
            logical.stream_key().unwrap_or_default().to_vec(),
            logical.functional_dependency(),
            dist,
            append_only,
            emit_on_window_close,
            watermark_columns,
        )
    }

    pub fn new_stream(
        ctx: OptimizerContextRef,
        schema: Schema,
        stream_key: Vec<usize>,
        functional_dependency: FunctionalDependencySet,
        dist: Distribution,
        append_only: bool,
        emit_on_window_close: bool,
        watermark_columns: FixedBitSet,
    ) -> Self {
        let id = ctx.next_plan_node_id();
        assert_eq!(watermark_columns.len(), schema.len());
        Self {
            id,
            ctx,
            schema,
            dist,
            order: Order::any(),
            stream_key,
            append_only,
            emit_on_window_close,
            functional_dependency,
            watermark_columns,
        }
    }

    pub fn new_batch_from_logical(
        logical: &impl GenericPlanNode,
        dist: Distribution,
        order: Order,
    ) -> Self {
        Self::new_batch(logical.ctx(), logical.schema(), dist, order)
    }

    pub fn new_batch(
        ctx: OptimizerContextRef,
        schema: Schema,
        dist: Distribution,
        order: Order,
    ) -> Self {
        let id = ctx.next_plan_node_id();
        let functional_dependency = FunctionalDependencySet::new(schema.len());
        let watermark_columns = FixedBitSet::with_capacity(schema.len());
        Self {
            id,
            ctx,
            schema,
            dist,
            order,
            stream_key: vec![],
            // Batch plan node won't touch `append_only` field
            append_only: true,
            emit_on_window_close: false, // TODO(rc): batch EOWC support?
            functional_dependency,
            watermark_columns,
        }
    }

    pub fn derive_stream_plan_base(plan_node: &PlanRef) -> Self {
        PlanBase::new_stream(
            plan_node.ctx(),
            plan_node.schema().clone(),
            plan_node.stream_key().to_vec(),
            plan_node.functional_dependency().clone(),
            plan_node.distribution().clone(),
            plan_node.append_only(),
            plan_node.emit_on_window_close(),
            plan_node.watermark_columns().clone(),
        )
    }

    pub fn clone_with_new_plan_id(&self) -> Self {
        let mut new = self.clone();
        new.id = self.ctx.next_plan_node_id();
        new
    }
}

macro_rules! impl_base_delegate {
    ($( { $convention:ident, $name:ident }),*) => {
        $(paste! {
            impl [<$convention $name>] {
                pub fn id(&self) -> PlanNodeId {
                    self.plan_base().id
                }
                 pub fn ctx(&self) -> OptimizerContextRef {
                    self.plan_base().ctx()
                }
                pub fn schema(&self) -> &Schema {
                    &self.plan_base().schema
                }
                pub fn stream_key(&self) -> &[usize] {
                    &self.plan_base().stream_key
                }
                pub fn order(&self) -> &Order {
                    &self.plan_base().order
                }
                pub fn distribution(&self) -> &Distribution {
                    &self.plan_base().dist
                }
                pub fn append_only(&self) -> bool {
                    self.plan_base().append_only
                }
                pub fn emit_on_window_close(&self) -> bool {
                    self.plan_base().emit_on_window_close
                }
                pub fn functional_dependency(&self) -> &FunctionalDependencySet {
                    &self.plan_base().functional_dependency
                }
            }
        })*
    }
}
for_all_plan_nodes! { impl_base_delegate }
