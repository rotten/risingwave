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

use std::collections::HashSet;
use std::fmt::Debug;
use std::sync::Arc;

use futures_util::future::FutureExt;
use risingwave_common::array::{ArrayBuilder, ArrayRef, BoolArrayBuilder, DataChunk};
use risingwave_common::row::OwnedRow;
use risingwave_common::types::{DataType, Datum, Scalar, ToOwnedDatum};
use risingwave_common::util::iter_util::ZipEqFast;
use risingwave_common::{bail, ensure};
use risingwave_pb::expr::expr_node::{RexNode, Type};
use risingwave_pb::expr::ExprNode;

use super::Build;
use crate::expr::{BoxedExpression, Expression};
use crate::Result;

#[derive(Debug)]
pub struct InExpression {
    left: BoxedExpression,
    set: HashSet<Datum>,
    return_type: DataType,
}

impl InExpression {
    pub fn new(
        left: BoxedExpression,
        data: impl Iterator<Item = Datum>,
        return_type: DataType,
    ) -> Self {
        let mut sarg = HashSet::new();
        for datum in data {
            sarg.insert(datum);
        }
        Self {
            left,
            set: sarg,
            return_type,
        }
    }

    // Returns true if datum exists in set, null if datum is null or datum does not exist in set
    // but null does, and false if neither datum nor null exists in set.
    fn exists(&self, datum: &Datum) -> Option<bool> {
        if datum.is_none() {
            None
        } else if self.set.contains(datum) {
            Some(true)
        } else if self.set.contains(&None) {
            None
        } else {
            Some(false)
        }
    }
}

#[async_trait::async_trait]
impl Expression for InExpression {
    fn return_type(&self) -> DataType {
        self.return_type.clone()
    }

    async fn eval(&self, input: &DataChunk) -> Result<ArrayRef> {
        let input_array = self.left.eval(input).await?;
        let mut output_array = BoolArrayBuilder::new(input_array.len());
        for (data, vis) in input_array.iter().zip_eq_fast(input.visibility().iter()) {
            if vis {
                let ret = self.exists(&data.to_owned_datum());
                output_array.append(ret);
            } else {
                output_array.append(None);
            }
        }
        Ok(Arc::new(output_array.finish().into()))
    }

    async fn eval_row(&self, input: &OwnedRow) -> Result<Datum> {
        let data = self.left.eval_row(input).await?;
        let ret = self.exists(&data);
        Ok(ret.map(|b| b.to_scalar_value()))
    }
}

impl Build for InExpression {
    fn build(
        prost: &ExprNode,
        build_child: impl Fn(&ExprNode) -> Result<BoxedExpression>,
    ) -> Result<Self> {
        ensure!(prost.get_function_type().unwrap() == Type::In);

        let ret_type = DataType::from(prost.get_return_type().unwrap());
        let RexNode::FuncCall(func_call_node) = prost.get_rex_node().unwrap() else {
            bail!("Expected RexNode::FuncCall");
        };
        let children = &func_call_node.children;

        let left_expr = build_child(&children[0])?;
        let mut data = Vec::new();
        // Used for const expression below to generate datum.
        // Frontend has made sure these can all be folded to constants.
        let data_chunk = DataChunk::new_dummy(1);
        for child in &children[1..] {
            let const_expr = build_child(child)?;
            let array = const_expr
                .eval(&data_chunk)
                .now_or_never()
                .expect("constant expression should not be async")?;
            let datum = array.value_at(0).to_owned_datum();
            data.push(datum);
        }
        Ok(InExpression::new(left_expr, data.into_iter(), ret_type))
    }
}

#[cfg(test)]
mod tests {
    use risingwave_common::array::DataChunk;
    use risingwave_common::row::OwnedRow;
    use risingwave_common::test_prelude::DataChunkTestExt;
    use risingwave_common::types::{DataType, ScalarImpl};
    use risingwave_common::util::value_encoding::serialize_datum;
    use risingwave_pb::data::data_type::TypeName;
    use risingwave_pb::data::{PbDataType, PbDatum};
    use risingwave_pb::expr::expr_node::{RexNode, Type};
    use risingwave_pb::expr::{ExprNode, FunctionCall};

    use crate::expr::expr_in::InExpression;
    use crate::expr::{Build, Expression, InputRefExpression};

    #[test]
    fn test_in_expr() {
        let input_ref_expr_node = ExprNode {
            function_type: Type::Unspecified as i32,
            return_type: Some(PbDataType {
                type_name: TypeName::Varchar as i32,
                ..Default::default()
            }),
            rex_node: Some(RexNode::InputRef(0)),
        };
        let constant_values = vec![
            ExprNode {
                function_type: Type::Unspecified as i32,
                return_type: Some(PbDataType {
                    type_name: TypeName::Varchar as i32,
                    ..Default::default()
                }),
                rex_node: Some(RexNode::Constant(PbDatum {
                    body: serialize_datum(Some("ABC".into()).as_ref()),
                })),
            },
            ExprNode {
                function_type: Type::Unspecified as i32,
                return_type: Some(PbDataType {
                    type_name: TypeName::Varchar as i32,
                    ..Default::default()
                }),
                rex_node: Some(RexNode::Constant(PbDatum {
                    body: serialize_datum(Some("def".into()).as_ref()),
                })),
            },
        ];
        let mut in_children = vec![input_ref_expr_node];
        in_children.extend(constant_values);
        let call = FunctionCall {
            children: in_children,
        };
        let p = ExprNode {
            function_type: Type::In as i32,
            return_type: Some(PbDataType {
                type_name: TypeName::Boolean as i32,
                ..Default::default()
            }),
            rex_node: Some(RexNode::FuncCall(call)),
        };
        assert!(InExpression::build_for_test(&p).is_ok());
    }

    #[tokio::test]
    async fn test_eval_search_expr() {
        let input_refs = [
            Box::new(InputRefExpression::new(DataType::Varchar, 0)),
            Box::new(InputRefExpression::new(DataType::Varchar, 0)),
        ];
        let data = [
            vec![
                Some(ScalarImpl::Utf8("abc".into())),
                Some(ScalarImpl::Utf8("def".into())),
            ],
            vec![None, Some(ScalarImpl::Utf8("abc".into()))],
        ];

        let data_chunks = [
            DataChunk::from_pretty(
                "T
                 abc
                 a
                 def
                 abc
                 .",
            )
            .with_invisible_holes(),
            DataChunk::from_pretty(
                "T
                abc
                a
                .",
            )
            .with_invisible_holes(),
        ];

        let expected = vec![
            vec![Some(true), Some(false), Some(true), Some(true), None],
            vec![Some(true), None, None],
        ];

        for (i, input_ref) in input_refs.into_iter().enumerate() {
            let search_expr =
                InExpression::new(input_ref, data[i].clone().into_iter(), DataType::Boolean);
            let vis = data_chunks[i].visibility();
            let res = search_expr
                .eval(&data_chunks[i])
                .await
                .unwrap()
                .compact(vis, expected[i].len());

            for (i, expect) in expected[i].iter().enumerate() {
                assert_eq!(res.datum_at(i), expect.map(ScalarImpl::Bool));
            }
        }
    }

    #[tokio::test]
    async fn test_eval_row_search_expr() {
        let input_refs = [
            Box::new(InputRefExpression::new(DataType::Varchar, 0)),
            Box::new(InputRefExpression::new(DataType::Varchar, 0)),
        ];

        let data = [
            vec![
                Some(ScalarImpl::Utf8("abc".into())),
                Some(ScalarImpl::Utf8("def".into())),
            ],
            vec![None, Some(ScalarImpl::Utf8("abc".into()))],
        ];

        let row_inputs = vec![
            vec![Some("abc"), Some("a"), Some("def"), None],
            vec![Some("abc"), Some("a"), None],
        ];

        let expected = [
            vec![Some(true), Some(false), Some(true), None],
            vec![Some(true), None, None],
        ];

        for (i, input_ref) in input_refs.into_iter().enumerate() {
            let search_expr =
                InExpression::new(input_ref, data[i].clone().into_iter(), DataType::Boolean);

            for (j, row_input) in row_inputs[i].iter().enumerate() {
                let row_input = vec![row_input.map(|s| s.into())];
                let row = OwnedRow::new(row_input);
                let result = search_expr.eval_row(&row).await.unwrap();
                assert_eq!(result, expected[i][j].map(ScalarImpl::Bool));
            }
        }
    }
}
