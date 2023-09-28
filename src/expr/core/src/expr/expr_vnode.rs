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

use std::sync::Arc;

use risingwave_common::array::{ArrayBuilder, ArrayImpl, ArrayRef, DataChunk, I16ArrayBuilder};
use risingwave_common::hash::VirtualNode;
use risingwave_common::row::OwnedRow;
use risingwave_common::types::{DataType, Datum};
use risingwave_pb::expr::expr_node::{RexNode, Type};
use risingwave_pb::expr::ExprNode;

use super::{BoxedExpression, Build, Expression};
use crate::expr::InputRefExpression;
use crate::{bail, ensure, Result};

#[derive(Debug)]
pub struct VnodeExpression {
    dist_key_indices: Vec<usize>,
}

impl VnodeExpression {
    pub fn new(dist_key_indices: Vec<usize>) -> Self {
        VnodeExpression { dist_key_indices }
    }
}

impl Build for VnodeExpression {
    fn build(
        prost: &ExprNode,
        _build_child: impl Fn(&ExprNode) -> Result<BoxedExpression>,
    ) -> Result<Self> {
        ensure!(prost.get_function_type().unwrap() == Type::Vnode);
        ensure!(DataType::from(prost.get_return_type().unwrap()) == DataType::Int16);

        let RexNode::FuncCall(func_call_node) = prost.get_rex_node().unwrap() else {
            bail!("Expected RexNode::FuncCall");
        };

        let dist_key_input_refs = func_call_node
            .get_children()
            .iter()
            .map(InputRefExpression::from_prost)
            .map(|input| input.index())
            .collect();

        Ok(VnodeExpression::new(dist_key_input_refs))
    }
}

#[async_trait::async_trait]
impl Expression for VnodeExpression {
    fn return_type(&self) -> DataType {
        DataType::Int16
    }

    async fn eval(&self, input: &DataChunk) -> Result<ArrayRef> {
        let vnodes = VirtualNode::compute_chunk(input, &self.dist_key_indices);
        let mut builder = I16ArrayBuilder::new(input.capacity());
        vnodes
            .into_iter()
            .for_each(|vnode| builder.append(Some(vnode.to_scalar())));
        Ok(Arc::new(ArrayImpl::from(builder.finish())))
    }

    async fn eval_row(&self, input: &OwnedRow) -> Result<Datum> {
        Ok(Some(
            VirtualNode::compute_row(input, &self.dist_key_indices)
                .to_scalar()
                .into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use risingwave_common::array::{DataChunk, DataChunkTestExt};
    use risingwave_common::hash::VirtualNode;
    use risingwave_common::row::Row;
    use risingwave_pb::data::data_type::TypeName;
    use risingwave_pb::data::PbDataType;
    use risingwave_pb::expr::expr_node::RexNode;
    use risingwave_pb::expr::expr_node::Type::Vnode;
    use risingwave_pb::expr::{ExprNode, FunctionCall};

    use super::VnodeExpression;
    use crate::expr::test_utils::make_input_ref;
    use crate::expr::{Build, Expression};

    pub fn make_vnode_function(children: Vec<ExprNode>) -> ExprNode {
        ExprNode {
            function_type: Vnode as i32,
            return_type: Some(PbDataType {
                type_name: TypeName::Int16 as i32,
                ..Default::default()
            }),
            rex_node: Some(RexNode::FuncCall(FunctionCall { children })),
        }
    }

    #[tokio::test]
    async fn test_vnode_expr_eval() {
        let input_node1 = make_input_ref(0, TypeName::Int32);
        let input_node2 = make_input_ref(0, TypeName::Int64);
        let input_node3 = make_input_ref(0, TypeName::Varchar);
        let vnode_expr = VnodeExpression::build_for_test(&make_vnode_function(vec![
            input_node1,
            input_node2,
            input_node3,
        ]))
        .unwrap();
        let chunk = DataChunk::from_pretty(
            "i  I  T
             1  10 abc
             2  32 def
             3  88 ghi",
        );
        let actual = vnode_expr.eval(&chunk).await.unwrap();
        actual.iter().for_each(|vnode| {
            let vnode = vnode.unwrap().into_int16();
            assert!(vnode >= 0);
            assert!((vnode as usize) < VirtualNode::COUNT);
        });
    }

    #[tokio::test]
    async fn test_vnode_expr_eval_row() {
        let input_node1 = make_input_ref(0, TypeName::Int32);
        let input_node2 = make_input_ref(0, TypeName::Int64);
        let input_node3 = make_input_ref(0, TypeName::Varchar);
        let vnode_expr = VnodeExpression::build_for_test(&make_vnode_function(vec![
            input_node1,
            input_node2,
            input_node3,
        ]))
        .unwrap();
        let chunk = DataChunk::from_pretty(
            "i  I  T
             1  10 abc
             2  32 def
             3  88 ghi",
        );
        let rows: Vec<_> = chunk.rows().map(|row| row.into_owned_row()).collect();
        for row in rows {
            let actual = vnode_expr.eval_row(&row).await.unwrap();
            let vnode = actual.unwrap().into_int16();
            assert!(vnode >= 0);
            assert!((vnode as usize) < VirtualNode::COUNT);
        }
    }
}
