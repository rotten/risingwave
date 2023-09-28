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

use risingwave_common::array::{ArrayRef, DataChunk};
use risingwave_common::row::OwnedRow;
use risingwave_common::types::{DataType, Datum};
use risingwave_common::{bail, ensure};
use risingwave_pb::expr::expr_node::{PbType, RexNode};
use risingwave_pb::expr::ExprNode;

use super::Build;
use crate::expr::{BoxedExpression, Expression};
use crate::Result;

#[derive(Debug)]
pub struct WhenClause {
    when: BoxedExpression,
    then: BoxedExpression,
}

#[derive(Debug)]
pub struct CaseExpression {
    return_type: DataType,
    when_clauses: Vec<WhenClause>,
    else_clause: Option<BoxedExpression>,
}

impl CaseExpression {
    pub fn new(
        return_type: DataType,
        when_clauses: Vec<WhenClause>,
        else_clause: Option<BoxedExpression>,
    ) -> Self {
        Self {
            return_type,
            when_clauses,
            else_clause,
        }
    }
}

#[async_trait::async_trait]
impl Expression for CaseExpression {
    fn return_type(&self) -> DataType {
        self.return_type.clone()
    }

    async fn eval(&self, input: &DataChunk) -> Result<ArrayRef> {
        let mut input = input.clone();
        let input_len = input.capacity();
        let mut selection = vec![None; input_len];
        let when_len = self.when_clauses.len();
        let mut result_array = Vec::with_capacity(when_len + 1);
        for (when_idx, WhenClause { when, then }) in self.when_clauses.iter().enumerate() {
            let calc_then_vis = when.eval(&input).await?.as_bool().to_bitmap();
            let input_vis = input.visibility().clone();
            input.set_visibility(calc_then_vis.clone());
            let then_res = then.eval(&input).await?;
            calc_then_vis
                .iter_ones()
                .for_each(|pos| selection[pos] = Some(when_idx));
            input.set_visibility(&input_vis & (!calc_then_vis));
            result_array.push(then_res);
        }
        if let Some(ref else_expr) = self.else_clause {
            let else_res = else_expr.eval(&input).await?;
            input
                .visibility()
                .iter_ones()
                .for_each(|pos| selection[pos] = Some(when_len));
            result_array.push(else_res);
        }
        let mut builder = self.return_type().create_array_builder(input.capacity());
        for (i, sel) in selection.into_iter().enumerate() {
            if let Some(when_idx) = sel {
                builder.append(result_array[when_idx].value_at(i));
            } else {
                builder.append_null();
            }
        }
        Ok(Arc::new(builder.finish()))
    }

    async fn eval_row(&self, input: &OwnedRow) -> Result<Datum> {
        for WhenClause { when, then } in &self.when_clauses {
            if when.eval_row(input).await?.map_or(false, |w| w.into_bool()) {
                return then.eval_row(input).await;
            }
        }
        if let Some(ref else_expr) = self.else_clause {
            else_expr.eval_row(input).await
        } else {
            Ok(None)
        }
    }
}

impl Build for CaseExpression {
    fn build(
        prost: &ExprNode,
        build_child: impl Fn(&ExprNode) -> Result<BoxedExpression>,
    ) -> Result<Self> {
        ensure!(prost.get_function_type().unwrap() == PbType::Case);

        let ret_type = DataType::from(prost.get_return_type().unwrap());
        let RexNode::FuncCall(func_call_node) = prost.get_rex_node().unwrap() else {
            bail!("Expected RexNode::FuncCall");
        };
        let children = &func_call_node.children;
        // children: (when, then)+, (else_clause)?
        let len = children.len();
        let else_clause = if len % 2 == 1 {
            let else_clause = build_child(&children[len - 1])?;
            if else_clause.return_type() != ret_type {
                bail!("Type mismatched between else and case.");
            }
            Some(else_clause)
        } else {
            None
        };
        let mut when_clauses = vec![];
        for i in 0..len / 2 {
            let when_index = i * 2;
            let then_index = i * 2 + 1;
            let when_expr = build_child(&children[when_index])?;
            let then_expr = build_child(&children[then_index])?;
            if when_expr.return_type() != DataType::Boolean {
                bail!("Type mismatched between when clause and condition");
            }
            if then_expr.return_type() != ret_type {
                bail!("Type mismatched between then clause and case");
            }
            let when_clause = WhenClause {
                when: when_expr,
                then: then_expr,
            };
            when_clauses.push(when_clause);
        }
        Ok(CaseExpression::new(ret_type, when_clauses, else_clause))
    }
}

#[cfg(test)]
mod tests {
    use risingwave_common::row::Row;
    use risingwave_common::test_prelude::DataChunkTestExt;
    use risingwave_common::types::ToOwnedDatum;
    use risingwave_common::util::iter_util::ZipEqDebug;

    use super::*;
    use crate::expr::build_from_pretty;

    #[tokio::test]
    async fn test_eval_searched_case() {
        // when x then 1 else 2
        let when_clauses = vec![WhenClause {
            when: build_from_pretty("$0:boolean"),
            then: build_from_pretty("1:int4"),
        }];
        let els = build_from_pretty("2:int4");
        let case = CaseExpression::new(DataType::Int32, when_clauses, Some(els));
        let (input, expected) = DataChunk::from_pretty(
            "B i
             t 1
             f 2
             t 1
             t 1
             f 2",
        )
        .split_column_at(1);

        // test eval
        let output = case.eval(&input).await.unwrap();
        assert_eq!(&output, expected.column_at(0));

        // test eval_row
        for (row, expected) in input.rows().zip_eq_debug(expected.rows()) {
            let result = case.eval_row(&row.to_owned_row()).await.unwrap();
            assert_eq!(result, expected.datum_at(0).to_owned_datum());
        }
    }

    #[tokio::test]
    async fn test_eval_without_else() {
        // when x then 1
        let when_clauses = vec![WhenClause {
            when: build_from_pretty("$0:boolean"),
            then: build_from_pretty("1:int4"),
        }];
        let case = CaseExpression::new(DataType::Int32, when_clauses, None);
        let (input, expected) = DataChunk::from_pretty(
            "B i
             t 1
             f .
             t 1
             f .",
        )
        .split_column_at(1);

        // test eval
        let output = case.eval(&input).await.unwrap();
        assert_eq!(&output, expected.column_at(0));

        // test eval_row
        for (row, expected) in input.rows().zip_eq_debug(expected.rows()) {
            let result = case.eval_row(&row.to_owned_row()).await.unwrap();
            assert_eq!(result, expected.datum_at(0).to_owned_datum());
        }
    }
}
