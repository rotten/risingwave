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

use std::hash::Hash;

use educe::Educe;
use pretty_xmlish::{Pretty, Str, XmlNode};
use risingwave_common::catalog::{Field, Schema, TableVersionId};
use risingwave_common::types::DataType;

use super::{DistillUnit, GenericPlanNode, GenericPlanRef};
use crate::catalog::TableId;
use crate::expr::{ExprImpl, ExprRewriter};
use crate::optimizer::plan_node::utils::childless_record;
use crate::optimizer::property::FunctionalDependencySet;
use crate::OptimizerContextRef;

#[derive(Debug, Clone, Educe)]
#[educe(PartialEq, Eq, Hash)]
pub struct Update<PlanRef: Eq + Hash> {
    #[educe(PartialEq(ignore))]
    #[educe(Hash(ignore))]
    pub table_name: String, // explain-only
    pub table_id: TableId,
    pub table_version_id: TableVersionId,
    pub input: PlanRef,
    pub exprs: Vec<ExprImpl>,
    pub returning: bool,
    pub update_column_indices: Vec<usize>,
}

impl<PlanRef: GenericPlanRef> Update<PlanRef> {
    pub fn output_len(&self) -> usize {
        if self.returning {
            self.input.schema().len()
        } else {
            1
        }
    }
}
impl<PlanRef: GenericPlanRef> GenericPlanNode for Update<PlanRef> {
    fn functional_dependency(&self) -> FunctionalDependencySet {
        FunctionalDependencySet::new(self.output_len())
    }

    fn schema(&self) -> Schema {
        if self.returning {
            self.input.schema().clone()
        } else {
            Schema::new(vec![Field::unnamed(DataType::Int64)])
        }
    }

    fn stream_key(&self) -> Option<Vec<usize>> {
        if self.returning {
            Some(self.input.stream_key().to_vec())
        } else {
            Some(vec![])
        }
    }

    fn ctx(&self) -> OptimizerContextRef {
        self.input.ctx()
    }
}

impl<PlanRef: Eq + Hash> Update<PlanRef> {
    pub fn new(
        input: PlanRef,
        table_name: String,
        table_id: TableId,
        table_version_id: TableVersionId,
        exprs: Vec<ExprImpl>,
        returning: bool,
        update_column_indices: Vec<usize>,
    ) -> Self {
        Self {
            table_name,
            table_id,
            table_version_id,
            input,
            exprs,
            returning,
            update_column_indices,
        }
    }

    pub(crate) fn rewrite_exprs(&mut self, r: &mut dyn ExprRewriter) {
        self.exprs = self
            .exprs
            .iter()
            .map(|e| r.rewrite_expr(e.clone()))
            .collect();
    }
}

impl<PlanRef: Eq + Hash> DistillUnit for Update<PlanRef> {
    fn distill_with_name<'a>(&self, name: impl Into<Str<'a>>) -> XmlNode<'a> {
        let mut vec = Vec::with_capacity(if self.returning { 3 } else { 2 });
        vec.push(("table", Pretty::from(self.table_name.clone())));
        vec.push(("exprs", Pretty::debug(&self.exprs)));
        if self.returning {
            vec.push(("returning", Pretty::display(&true)));
        }
        childless_record(name, vec)
    }
}
