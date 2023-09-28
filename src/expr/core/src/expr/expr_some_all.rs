// Copyright 2023 RisingWave Labs
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::sync::Arc;

use itertools::Itertools;
use risingwave_common::array::{Array, ArrayRef, BoolArray, DataChunk};
use risingwave_common::row::OwnedRow;
use risingwave_common::types::{DataType, Datum, ListRef, Scalar, ScalarImpl, ScalarRefImpl};
use risingwave_common::util::iter_util::ZipEqFast;
use risingwave_common::{bail, ensure};
use risingwave_pb::expr::expr_node::{RexNode, Type};
use risingwave_pb::expr::{ExprNode, FunctionCall};

use super::build::get_children_and_return_type;
use super::{BoxedExpression, Build, Expression};
use crate::Result;

#[derive(Debug)]
pub struct SomeAllExpression {
    left_expr: BoxedExpression,
    right_expr: BoxedExpression,
    expr_type: Type,
    func: BoxedExpression,
}

impl SomeAllExpression {
    pub fn new(
        left_expr: BoxedExpression,
        right_expr: BoxedExpression,
        expr_type: Type,
        func: BoxedExpression,
    ) -> Self {
        SomeAllExpression {
            left_expr,
            right_expr,
            expr_type,
            func,
        }
    }

    fn resolve_boolean_vec(&self, boolean_vec: Vec<Option<bool>>) -> Option<bool> {
        match self.expr_type {
            Type::Some => {
                if boolean_vec.iter().any(|b| b.unwrap_or(false)) {
                    Some(true)
                } else if boolean_vec.iter().any(|b| b.is_none()) {
                    None
                } else {
                    Some(false)
                }
            }
            Type::All => {
                if boolean_vec.iter().all(|b| b.unwrap_or(false)) {
                    Some(true)
                } else if boolean_vec.iter().any(|b| !b.unwrap_or(true)) {
                    Some(false)
                } else {
                    None
                }
            }
            _ => unreachable!(),
        }
    }
}

#[async_trait::async_trait]
impl Expression for SomeAllExpression {
    fn return_type(&self) -> DataType {
        DataType::Boolean
    }

    async fn eval(&self, data_chunk: &DataChunk) -> Result<ArrayRef> {
        let arr_left = self.left_expr.eval(data_chunk).await?;
        let arr_right = self.right_expr.eval(data_chunk).await?;
        let mut num_array = Vec::with_capacity(data_chunk.capacity());

        let arr_right_inner = arr_right.as_list();
        let DataType::List(datatype) = arr_right_inner.data_type() else {
            unreachable!()
        };
        let capacity = arr_right_inner
            .iter()
            .flatten()
            .map(ListRef::flatten_len)
            .sum();

        let mut unfolded_arr_left_builder = arr_left.create_builder(capacity);
        let mut unfolded_arr_right_builder = datatype.create_array_builder(capacity);

        let mut unfolded_left_right =
            |left: Option<ScalarRefImpl<'_>>,
             right: Option<ScalarRefImpl<'_>>,
             num_array: &mut Vec<Option<usize>>| {
                if right.is_none() {
                    num_array.push(None);
                    return;
                }

                let datum_right = right.unwrap();
                match datum_right {
                    ScalarRefImpl::List(array) => {
                        let flattened = array.flatten();
                        let len = flattened.len();
                        num_array.push(Some(len));
                        unfolded_arr_left_builder.append_n(len, left);
                        for item in flattened {
                            unfolded_arr_right_builder.append(item);
                        }
                    }
                    _ => unreachable!(),
                }
            };

        if data_chunk.is_compacted() {
            for (left, right) in arr_left.iter().zip_eq_fast(arr_right.iter()) {
                unfolded_left_right(left, right, &mut num_array);
            }
        } else {
            for ((left, right), visible) in arr_left
                .iter()
                .zip_eq_fast(arr_right.iter())
                .zip_eq_fast(data_chunk.visibility().iter())
            {
                if !visible {
                    num_array.push(None);
                    continue;
                }
                unfolded_left_right(left, right, &mut num_array);
            }
        }

        assert_eq!(num_array.len(), data_chunk.capacity());

        let unfolded_arr_left = unfolded_arr_left_builder.finish();
        let unfolded_arr_right = unfolded_arr_right_builder.finish();

        // Unfolded array are actually compacted, and the visibility of the output array will be
        // further restored by `num_array`.
        assert_eq!(unfolded_arr_left.len(), unfolded_arr_right.len());
        let unfolded_compact_len = unfolded_arr_left.len();

        let data_chunk = DataChunk::new(
            vec![unfolded_arr_left.into(), unfolded_arr_right.into()],
            unfolded_compact_len,
        );

        let func_results = self.func.eval(&data_chunk).await?;
        let mut func_results_iter = func_results.as_bool().iter();
        Ok(Arc::new(
            num_array
                .into_iter()
                .map(|num| match num {
                    Some(num) => {
                        self.resolve_boolean_vec(func_results_iter.by_ref().take(num).collect_vec())
                    }
                    None => None,
                })
                .collect::<BoolArray>()
                .into(),
        ))
    }

    async fn eval_row(&self, row: &OwnedRow) -> Result<Datum> {
        let datum_left = self.left_expr.eval_row(row).await?;
        let datum_right = self.right_expr.eval_row(row).await?;
        if let Some(array) = datum_right {
            match array {
                ScalarImpl::List(array) => {
                    let mut scalar_vec = Vec::with_capacity(array.values().len());
                    for d in array.values() {
                        let e = self
                            .func
                            .eval_row(&OwnedRow::new(vec![datum_left.clone(), d.clone()]))
                            .await?;
                        scalar_vec.push(e);
                    }
                    let boolean_vec = scalar_vec
                        .into_iter()
                        .map(|scalar_ref| scalar_ref.map(|s| s.into_bool()))
                        .collect_vec();
                    Ok(self
                        .resolve_boolean_vec(boolean_vec)
                        .map(|b| b.to_scalar_value()))
                }
                _ => unreachable!(),
            }
        } else {
            Ok(None)
        }
    }
}

impl Build for SomeAllExpression {
    fn build(
        prost: &ExprNode,
        build_child: impl Fn(&ExprNode) -> Result<BoxedExpression>,
    ) -> Result<Self> {
        let outer_expr_type = prost.get_function_type().unwrap();
        let (outer_children, outer_return_type) = get_children_and_return_type(prost)?;
        ensure!(matches!(outer_return_type, DataType::Boolean));

        let mut inner_expr_type = outer_children[0].get_function_type().unwrap();
        let (mut inner_children, mut inner_return_type) =
            get_children_and_return_type(&outer_children[0])?;
        let mut stack = vec![];
        while inner_children.len() != 2 {
            stack.push((inner_expr_type, inner_return_type));
            inner_expr_type = inner_children[0].get_function_type().unwrap();
            (inner_children, inner_return_type) = get_children_and_return_type(&inner_children[0])?;
        }

        let left_expr = build_child(&inner_children[0])?;
        let right_expr = build_child(&inner_children[1])?;

        let DataType::List(right_expr_return_type) = right_expr.return_type() else {
            bail!("Expect Array Type");
        };

        let eval_func = {
            let left_expr_input_ref = ExprNode {
                function_type: Type::Unspecified as i32,
                return_type: Some(left_expr.return_type().to_protobuf()),
                rex_node: Some(RexNode::InputRef(0)),
            };
            let right_expr_input_ref = ExprNode {
                function_type: Type::Unspecified as i32,
                return_type: Some(right_expr_return_type.to_protobuf()),
                rex_node: Some(RexNode::InputRef(1)),
            };
            let mut root_expr_node = ExprNode {
                function_type: inner_expr_type as i32,
                return_type: Some(inner_return_type.to_protobuf()),
                rex_node: Some(RexNode::FuncCall(FunctionCall {
                    children: vec![left_expr_input_ref, right_expr_input_ref],
                })),
            };
            while let Some((expr_type, return_type)) = stack.pop() {
                root_expr_node = ExprNode {
                    function_type: expr_type as i32,
                    return_type: Some(return_type.to_protobuf()),
                    rex_node: Some(RexNode::FuncCall(FunctionCall {
                        children: vec![root_expr_node],
                    })),
                }
            }
            build_child(&root_expr_node)?
        };

        Ok(SomeAllExpression::new(
            left_expr,
            right_expr,
            outer_expr_type,
            eval_func,
        ))
    }
}
