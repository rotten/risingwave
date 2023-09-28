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

use std::collections::{HashMap, HashSet};
use std::fmt::Debug;

use itertools::Itertools;
use risingwave_common::catalog::{Field, Schema, PG_CATALOG_SCHEMA_NAME, RW_CATALOG_SCHEMA_NAME};
use risingwave_common::error::{ErrorCode, Result, RwError};
use risingwave_common::types::{DataType, ScalarImpl};
use risingwave_common::util::iter_util::ZipEqFast;
use risingwave_expr::aggregate::AggKind;
use risingwave_sqlparser::ast::{
    BinaryOperator, DataType as AstDataType, Distinct, Expr, Ident, Join, JoinConstraint,
    JoinOperator, ObjectName, Select, SelectItem, TableFactor, TableWithJoins,
};

use super::bind_context::{Clause, ColumnBinding};
use super::statement::RewriteExprsRecursive;
use super::UNNAMED_COLUMN;
use crate::binder::{Binder, Relation};
use crate::catalog::check_valid_column_name;
use crate::catalog::system_catalog::pg_catalog::{
    PG_INDEX_COLUMNS, PG_INDEX_TABLE_NAME, PG_USER_ID_INDEX, PG_USER_NAME_INDEX, PG_USER_TABLE_NAME,
};
use crate::catalog::system_catalog::rw_catalog::{
    RW_TABLE_STATS_COLUMNS, RW_TABLE_STATS_KEY_SIZE_INDEX, RW_TABLE_STATS_TABLE_ID_INDEX,
    RW_TABLE_STATS_TABLE_NAME, RW_TABLE_STATS_VALUE_SIZE_INDEX,
};
use crate::expr::{
    AggCall, CorrelatedId, CorrelatedInputRef, Depth, Expr as _, ExprImpl, ExprType, FunctionCall,
    InputRef, OrderBy,
};
use crate::utils::group_by::GroupBy;
use crate::utils::Condition;

#[derive(Debug, Clone)]
pub struct BoundSelect {
    pub distinct: BoundDistinct,
    pub select_items: Vec<ExprImpl>,
    pub aliases: Vec<Option<String>>,
    pub from: Option<Relation>,
    pub where_clause: Option<ExprImpl>,
    pub group_by: GroupBy,
    pub having: Option<ExprImpl>,
    pub schema: Schema,
}

impl RewriteExprsRecursive for BoundSelect {
    fn rewrite_exprs_recursive(&mut self, rewriter: &mut impl crate::expr::ExprRewriter) {
        self.distinct.rewrite_exprs_recursive(rewriter);

        let new_select_items = std::mem::take(&mut self.select_items)
            .into_iter()
            .map(|expr| rewriter.rewrite_expr(expr))
            .collect::<Vec<_>>();
        self.select_items = new_select_items;

        if let Some(from) = &mut self.from {
            from.rewrite_exprs_recursive(rewriter);
        }

        self.where_clause =
            std::mem::take(&mut self.where_clause).map(|expr| rewriter.rewrite_expr(expr));

        let new_group_by = match &mut self.group_by {
            GroupBy::GroupKey(group_key) => GroupBy::GroupKey(
                std::mem::take(group_key)
                    .into_iter()
                    .map(|expr| rewriter.rewrite_expr(expr))
                    .collect::<Vec<_>>(),
            ),
            GroupBy::GroupingSets(grouping_sets) => GroupBy::GroupingSets(
                std::mem::take(grouping_sets)
                    .into_iter()
                    .map(|set| {
                        set.into_iter()
                            .map(|expr| rewriter.rewrite_expr(expr))
                            .collect()
                    })
                    .collect::<Vec<_>>(),
            ),
            GroupBy::Rollup(rollup) => GroupBy::Rollup(
                std::mem::take(rollup)
                    .into_iter()
                    .map(|set| {
                        set.into_iter()
                            .map(|expr| rewriter.rewrite_expr(expr))
                            .collect()
                    })
                    .collect::<Vec<_>>(),
            ),
            GroupBy::Cube(cube) => GroupBy::Cube(
                std::mem::take(cube)
                    .into_iter()
                    .map(|set| {
                        set.into_iter()
                            .map(|expr| rewriter.rewrite_expr(expr))
                            .collect()
                    })
                    .collect::<Vec<_>>(),
            ),
        };
        self.group_by = new_group_by;

        self.having = std::mem::take(&mut self.having).map(|expr| rewriter.rewrite_expr(expr));
    }
}

impl BoundSelect {
    /// The schema returned by this [`BoundSelect`].
    pub fn schema(&self) -> &Schema {
        &self.schema
    }

    pub fn exprs(&self) -> impl Iterator<Item = &ExprImpl> {
        self.select_items
            .iter()
            .chain(self.group_by.iter())
            .chain(self.where_clause.iter())
            .chain(self.having.iter())
    }

    pub fn exprs_mut(&mut self) -> impl Iterator<Item = &mut ExprImpl> {
        self.select_items
            .iter_mut()
            .chain(self.group_by.iter_mut())
            .chain(self.where_clause.iter_mut())
            .chain(self.having.iter_mut())
    }

    pub fn is_correlated(&self, depth: Depth) -> bool {
        self.exprs()
            .any(|expr| expr.has_correlated_input_ref_by_depth(depth))
            || match self.from.as_ref() {
                Some(relation) => relation.is_correlated(depth),
                None => false,
            }
    }

    pub fn collect_correlated_indices_by_depth_and_assign_id(
        &mut self,
        depth: Depth,
        correlated_id: CorrelatedId,
    ) -> Vec<usize> {
        let mut correlated_indices = self
            .exprs_mut()
            .flat_map(|expr| {
                expr.collect_correlated_indices_by_depth_and_assign_id(depth, correlated_id)
            })
            .collect_vec();

        if let Some(relation) = self.from.as_mut() {
            correlated_indices.extend(
                relation.collect_correlated_indices_by_depth_and_assign_id(depth, correlated_id),
            );
        }

        correlated_indices
    }
}

#[derive(Debug, Clone)]
pub enum BoundDistinct {
    All,
    Distinct,
    DistinctOn(Vec<ExprImpl>),
}

impl RewriteExprsRecursive for BoundDistinct {
    fn rewrite_exprs_recursive(&mut self, rewriter: &mut impl crate::expr::ExprRewriter) {
        if let Self::DistinctOn(exprs) = self {
            let new_exprs = std::mem::take(exprs)
                .into_iter()
                .map(|expr| rewriter.rewrite_expr(expr))
                .collect::<Vec<_>>();
            exprs.extend(new_exprs);
        }
    }
}

impl BoundDistinct {
    pub const fn is_all(&self) -> bool {
        matches!(self, Self::All)
    }

    pub const fn is_distinct(&self) -> bool {
        matches!(self, Self::Distinct)
    }
}

impl Binder {
    pub(super) fn bind_select(&mut self, select: Select) -> Result<BoundSelect> {
        // Bind FROM clause.
        let from = self.bind_vec_table_with_joins(select.from)?;

        // Bind SELECT clause.
        let (select_items, aliases) = self.bind_select_list(select.projection)?;

        // Bind DISTINCT ON.
        let distinct = self.bind_distinct_on(select.distinct)?;

        // Bind WHERE clause.
        self.context.clause = Some(Clause::Where);
        let selection = select
            .selection
            .map(|expr| {
                self.bind_expr(expr)
                    .and_then(|expr| expr.enforce_bool_clause("WHERE"))
            })
            .transpose()?;
        self.context.clause = None;

        // Bind GROUP BY clause.
        let out_name_to_index = Self::build_name_to_index(aliases.iter().filter_map(Clone::clone));
        self.context.clause = Some(Clause::GroupBy);

        // Only support one grouping item in group by clause
        let group_by = if select.group_by.len() == 1 && let Expr::GroupingSets(grouping_sets) = &select.group_by[0] {
            GroupBy::GroupingSets(self.bind_grouping_items_expr_in_select(grouping_sets.clone(), &out_name_to_index, &select_items)?)
        } else if select.group_by.len() == 1 && let Expr::Rollup(rollup) = &select.group_by[0] {
            GroupBy::Rollup(self.bind_grouping_items_expr_in_select(rollup.clone(), &out_name_to_index, &select_items)?)
        } else if select.group_by.len() == 1 && let Expr::Cube(cube) = &select.group_by[0] {
            GroupBy::Cube(self.bind_grouping_items_expr_in_select(cube.clone(), &out_name_to_index, &select_items)?)
        } else {
            if select.group_by.iter().any(|expr| matches!(expr, Expr::GroupingSets(_)) || matches!(expr, Expr::Rollup(_)) || matches!(expr, Expr::Cube(_))) {
                return Err(ErrorCode::BindError("Only support one grouping item in group by clause".to_string()).into());
            }
            GroupBy::GroupKey(select
                .group_by
                .into_iter()
                .map(|expr| self.bind_group_by_expr_in_select(expr, &out_name_to_index, &select_items))
                .try_collect()?)
        };
        self.context.clause = None;

        // Bind HAVING clause.
        self.context.clause = Some(Clause::Having);
        let having = select
            .having
            .map(|expr| {
                self.bind_expr(expr)
                    .and_then(|expr| expr.enforce_bool_clause("HAVING"))
            })
            .transpose()?;
        self.context.clause = None;

        // Store field from `ExprImpl` to support binding `field_desc` in `subquery`.
        let fields = select_items
            .iter()
            .zip_eq_fast(aliases.iter())
            .map(|(s, a)| {
                let name = a.clone().unwrap_or_else(|| UNNAMED_COLUMN.to_string());
                Ok(Field::with_name(s.return_type(), name))
            })
            .collect::<Result<Vec<Field>>>()?;

        Ok(BoundSelect {
            distinct,
            select_items,
            aliases,
            from,
            where_clause: selection,
            group_by,
            having,
            schema: Schema { fields },
        })
    }

    pub fn bind_select_list(
        &mut self,
        select_items: Vec<SelectItem>,
    ) -> Result<(Vec<ExprImpl>, Vec<Option<String>>)> {
        let mut select_list = vec![];
        let mut aliases = vec![];
        for item in select_items {
            match item {
                SelectItem::UnnamedExpr(expr) => {
                    let alias = derive_alias(&expr);
                    let bound = self.bind_expr(expr)?;
                    select_list.push(bound);
                    aliases.push(alias);
                }
                SelectItem::ExprWithAlias { expr, alias } => {
                    check_valid_column_name(&alias.real_value())?;

                    let expr = self.bind_expr(expr)?;
                    select_list.push(expr);
                    aliases.push(Some(alias.real_value()));
                }
                SelectItem::QualifiedWildcard(obj_name, except) => {
                    let table_name = &obj_name.0.last().unwrap().real_value();
                    let except_indices = self.generate_except_indices(except)?;
                    let (begin, end) = self.context.range_of.get(table_name).ok_or_else(|| {
                        ErrorCode::ItemNotFound(format!("relation \"{}\"", table_name))
                    })?;
                    let (exprs, names) = Self::iter_bound_columns(
                        self.context.columns[*begin..*end]
                            .iter()
                            .filter(|c| !c.is_hidden && !except_indices.contains(&c.index)),
                    );
                    select_list.extend(exprs);
                    aliases.extend(names);
                }
                SelectItem::ExprQualifiedWildcard(expr, prefix) => {
                    let (exprs, names) = self.bind_wildcard_field_column(expr, prefix)?;
                    select_list.extend(exprs);
                    aliases.extend(names);
                }
                SelectItem::Wildcard(except) => {
                    if self.context.range_of.is_empty() {
                        return Err(ErrorCode::BindError(
                            "SELECT * with no tables specified is not valid".into(),
                        )
                        .into());
                    }

                    // Bind the column groups
                    // In psql, the USING and NATURAL columns come before the rest of the
                    // columns in a SELECT * statement
                    let (exprs, names) = self.iter_column_groups();
                    select_list.extend(exprs);
                    aliases.extend(names);

                    let except_indices = self.generate_except_indices(except)?;

                    // Bind columns that are not in groups
                    let (exprs, names) =
                        Self::iter_bound_columns(self.context.columns[..].iter().filter(|c| {
                            !c.is_hidden
                                && !self
                                    .context
                                    .column_group_context
                                    .mapping
                                    .contains_key(&c.index)
                                && !except_indices.contains(&c.index)
                        }));

                    select_list.extend(exprs);
                    aliases.extend(names);
                    // TODO: we will need to be able to handle wildcard expressions bound to
                    // aliases in the future. We'd then need a
                    // `NaturalGroupContext` bound to each alias
                    // to correctly disambiguate column
                    // references
                    //
                    // We may need to refactor `NaturalGroupContext` to become span aware in
                    // that case.
                }
            }
        }
        Ok((select_list, aliases))
    }

    /// Bind an `GROUP BY` expression in a [`Select`], which can be either:
    /// * index of an output column
    /// * an arbitrary expression on input columns
    /// * an output-column name
    ///
    /// Note the differences from `bind_order_by_expr_in_query`:
    /// * When a name matches both an input column and an output column, `group by` interprets it as
    ///   input column while `order by` interprets it as output column.
    /// * As the name suggests, `group by` is part of `select` while `order by` is part of `query`.
    ///   A `query` may consist unions of multiple `select`s (each with their own `group by`) but
    ///   only one `order by`.
    /// * Logically / semantically, `group by` evaluates before `select items`, which evaluates
    ///   before `order by`. This means, `group by` can evaluate arbitrary expressions itself, or
    ///   take expressions from `select items` (we `clone` here and `logical_agg` will rewrite those
    ///   `select items` to `InputRef`). However, `order by` can only refer to `select items`, or
    ///   append its extra arbitrary expressions as hidden `select items` for evaluation.
    ///
    /// # Arguments
    ///
    /// * `name_to_index` - output column name -> index. Ambiguous (duplicate) output names are
    ///   marked with `usize::MAX`.
    fn bind_group_by_expr_in_select(
        &mut self,
        expr: Expr,
        name_to_index: &HashMap<String, usize>,
        select_items: &[ExprImpl],
    ) -> Result<ExprImpl> {
        let name = match &expr {
            Expr::Identifier(ident) => Some(ident.real_value()),
            _ => None,
        };
        match self.bind_expr(expr) {
            Ok(ExprImpl::Literal(lit)) => match lit.get_data() {
                Some(ScalarImpl::Int32(idx)) => idx
                    .saturating_sub(1)
                    .try_into()
                    .ok()
                    .and_then(|i: usize| select_items.get(i).cloned())
                    .ok_or_else(|| {
                        ErrorCode::BindError(format!(
                            "GROUP BY position {idx} is not in select list"
                        ))
                        .into()
                    }),
                _ => Err(ErrorCode::BindError("non-integer constant in GROUP BY".into()).into()),
            },
            Ok(e) => Ok(e),
            Err(e) => match name {
                None => Err(e),
                Some(name) => match name_to_index.get(&name) {
                    None => Err(e),
                    Some(&usize::MAX) => Err(ErrorCode::BindError(format!(
                        "GROUP BY \"{name}\" is ambiguous"
                    ))
                    .into()),
                    Some(out_idx) => Ok(select_items[*out_idx].clone()),
                },
            },
        }
    }

    fn bind_grouping_items_expr_in_select(
        &mut self,
        grouping_items: Vec<Vec<Expr>>,
        name_to_index: &HashMap<String, usize>,
        select_items: &[ExprImpl],
    ) -> Result<Vec<Vec<ExprImpl>>> {
        let mut result = vec![];
        for set in grouping_items {
            let mut set_exprs = vec![];
            for expr in set {
                let name = match &expr {
                    Expr::Identifier(ident) => Some(ident.real_value()),
                    _ => None,
                };
                let expr_impl = match self.bind_expr(expr) {
                    Ok(ExprImpl::Literal(lit)) => match lit.get_data() {
                        Some(ScalarImpl::Int32(idx)) => idx
                            .saturating_sub(1)
                            .try_into()
                            .ok()
                            .and_then(|i: usize| select_items.get(i).cloned())
                            .ok_or_else(|| {
                                ErrorCode::BindError(format!(
                                    "GROUP BY position {idx} is not in select list"
                                ))
                                .into()
                            }),
                        _ => Err(
                            ErrorCode::BindError("non-integer constant in GROUP BY".into()).into(),
                        ),
                    },
                    Ok(e) => Ok(e),
                    Err(e) => match name {
                        None => Err(e),
                        Some(name) => match name_to_index.get(&name) {
                            None => Err(e),
                            Some(&usize::MAX) => Err(ErrorCode::BindError(format!(
                                "GROUP BY \"{name}\" is ambiguous"
                            ))
                            .into()),
                            Some(out_idx) => Ok(select_items[*out_idx].clone()),
                        },
                    },
                };

                set_exprs.push(expr_impl?);
            }
            result.push(set_exprs);
        }
        Ok(result)
    }

    pub fn bind_returning_list(
        &mut self,
        returning_items: Vec<SelectItem>,
    ) -> Result<(Vec<ExprImpl>, Vec<Field>)> {
        let (returning_list, aliases) = self.bind_select_list(returning_items)?;
        if returning_list
            .iter()
            .any(|expr| expr.has_agg_call() || expr.has_window_function())
        {
            return Err(RwError::from(ErrorCode::BindError(
                "should not have agg/window in the `RETURNING` list".to_string(),
            )));
        }

        let fields = returning_list
            .iter()
            .zip_eq_fast(aliases.iter())
            .map(|(s, a)| {
                let name = a.clone().unwrap_or_else(|| UNNAMED_COLUMN.to_string());
                Ok::<Field, RwError>(Field::with_name(s.return_type(), name))
            })
            .try_collect()?;
        Ok((returning_list, fields))
    }

    /// `bind_get_user_by_id_select` binds a select statement that returns a single user name by id,
    /// this is used for function `pg_catalog.get_user_by_id()`.
    pub fn bind_get_user_by_id_select(&mut self, input: &ExprImpl) -> Result<BoundSelect> {
        let select_items = vec![InputRef::new(PG_USER_NAME_INDEX, DataType::Varchar).into()];
        let schema = Schema {
            fields: vec![Field::with_name(
                DataType::Varchar,
                UNNAMED_COLUMN.to_string(),
            )],
        };
        let input = match input {
            ExprImpl::InputRef(input_ref) => {
                CorrelatedInputRef::new(input_ref.index(), input_ref.return_type(), 1).into()
            }
            ExprImpl::CorrelatedInputRef(col_input_ref) => CorrelatedInputRef::new(
                col_input_ref.index(),
                col_input_ref.return_type(),
                col_input_ref.depth() + 1,
            )
            .into(),
            ExprImpl::Literal(_) => input.clone(),
            _ => return Err(ErrorCode::BindError("Unsupported input type".to_string()).into()),
        };
        let from = Some(self.bind_relation_by_name_inner(
            Some(PG_CATALOG_SCHEMA_NAME),
            PG_USER_TABLE_NAME,
            None,
            false,
        )?);
        let where_clause = Some(
            FunctionCall::new(
                ExprType::Equal,
                vec![
                    input,
                    InputRef::new(PG_USER_ID_INDEX, DataType::Int32).into(),
                ],
            )?
            .into(),
        );

        Ok(BoundSelect {
            distinct: BoundDistinct::All,
            select_items,
            aliases: vec![None],
            from,
            where_clause,
            group_by: GroupBy::GroupKey(vec![]),
            having: None,
            schema,
        })
    }

    /// This returns the size of all the indexes that are on the specified table.
    pub fn bind_get_indexes_size_select(&mut self, table: &ExprImpl) -> Result<BoundSelect> {
        // this function is implemented with the following query:
        //     SELECT sum(total_key_size + total_value_size)
        //     FROM rw_catalog.rw_table_stats as stats
        //     JOIN pg_index on stats.id = pg_index.indexrelid
        //     WHERE pg_index.indrelid = 'table_name'::regclass

        let indexrelid_col = PG_INDEX_COLUMNS[0].1;
        let tbl_stats_id_col = RW_TABLE_STATS_COLUMNS[0].1;

        // Filter to only the Indexes on this table
        let table_id = self.table_id_query(table)?;

        let constraint = JoinConstraint::On(Expr::BinaryOp {
            left: Box::new(Expr::Identifier(Ident::new_unchecked(tbl_stats_id_col))),
            op: BinaryOperator::Eq,
            right: Box::new(Expr::Identifier(Ident::new_unchecked(indexrelid_col))),
        });
        let indexes_with_stats = self.bind_table_with_joins(TableWithJoins {
            relation: TableFactor::Table {
                name: ObjectName(vec![
                    RW_CATALOG_SCHEMA_NAME.into(),
                    RW_TABLE_STATS_TABLE_NAME.into(),
                ]),
                alias: None,
                for_system_time_as_of_proctime: false,
            },
            joins: vec![Join {
                relation: TableFactor::Table {
                    name: ObjectName(vec![
                        PG_CATALOG_SCHEMA_NAME.into(),
                        PG_INDEX_TABLE_NAME.into(),
                    ]),
                    alias: None,
                    for_system_time_as_of_proctime: false,
                },
                join_operator: JoinOperator::Inner(constraint),
            }],
        })?;

        // Get the size of an index by adding the size of the keys and the size of the values
        let sum = FunctionCall::new(
            ExprType::Add,
            vec![
                InputRef::new(RW_TABLE_STATS_KEY_SIZE_INDEX, DataType::Int64).into(),
                InputRef::new(RW_TABLE_STATS_VALUE_SIZE_INDEX, DataType::Int64).into(),
            ],
        )?
        .into();

        // There could be multiple indexes on a table so aggregate the sizes of all indexes
        let select_items: Vec<ExprImpl> = vec![AggCall::new(
            AggKind::Sum0,
            vec![sum],
            false,
            OrderBy::any(),
            Condition::true_cond(),
            vec![],
        )?
        .into()];

        let indrelid_col = PG_INDEX_COLUMNS[1].1;
        let indrelid_ref = self.bind_column(&[indrelid_col.into()])?;
        let where_clause: Option<ExprImpl> =
            Some(FunctionCall::new(ExprType::Equal, vec![indrelid_ref, table_id])?.into());

        // define the output schema
        let result_schema = Schema {
            fields: vec![Field::with_name(
                DataType::Int64,
                "pg_indexes_size".to_string(),
            )],
        };

        Ok(BoundSelect {
            distinct: BoundDistinct::All,
            select_items,
            aliases: vec![None],
            from: Some(indexes_with_stats),
            where_clause,
            group_by: GroupBy::GroupKey(vec![]),
            having: None,
            schema: result_schema,
        })
    }

    pub fn bind_get_table_size_select(
        &mut self,
        output_name: &str,
        table: &ExprImpl,
    ) -> Result<BoundSelect> {
        // define the output schema
        let result_schema = Schema {
            fields: vec![Field::with_name(DataType::Int64, output_name.to_string())],
        };

        // Get table stats data
        let from = Some(self.bind_relation_by_name_inner(
            Some(RW_CATALOG_SCHEMA_NAME),
            RW_TABLE_STATS_TABLE_NAME,
            None,
            false,
        )?);

        let table_id = self.table_id_query(table)?;

        // Filter to only the Indexes on this table
        let where_clause: Option<ExprImpl> = Some(
            FunctionCall::new(
                ExprType::Equal,
                vec![
                    table_id,
                    InputRef::new(RW_TABLE_STATS_TABLE_ID_INDEX, DataType::Int32).into(),
                ],
            )?
            .into(),
        );

        // Add the space used by keys and the space used by values to get the total space used by
        // the table
        let key_value_size_sum = FunctionCall::new(
            ExprType::Add,
            vec![
                InputRef::new(RW_TABLE_STATS_KEY_SIZE_INDEX, DataType::Int64).into(),
                InputRef::new(RW_TABLE_STATS_VALUE_SIZE_INDEX, DataType::Int64).into(),
            ],
        )?
        .into();
        let select_items = vec![key_value_size_sum];

        Ok(BoundSelect {
            distinct: BoundDistinct::All,
            select_items,
            aliases: vec![None],
            from,
            where_clause,
            group_by: GroupBy::GroupKey(vec![]),
            having: None,
            schema: result_schema,
        })
    }

    /// Given literal varchar this will return the Object ID of the table or index whose
    /// name matches the varchar.  Given a literal integer, this will return the integer regardless
    /// of whether an object exists with an Object ID that matches the integer.
    fn table_id_query(&mut self, table: &ExprImpl) -> Result<ExprImpl> {
        match table.as_literal() {
            Some(literal) if literal.return_type().is_int() => Ok(table.clone()),
            Some(literal) if literal.return_type() == DataType::Varchar => {
                let table_name = literal
                    .get_data()
                    .as_ref()
                    .expect("ExprImpl value is a Literal but cannot get ref to data")
                    .as_utf8();
                self.bind_cast(
                    Expr::Value(risingwave_sqlparser::ast::Value::SingleQuotedString(
                        table_name.to_string(),
                    )),
                    AstDataType::Regclass,
                )
            }
            _ => Err(RwError::from(ErrorCode::ExprError(
                "Expected an integer or varchar literal".into(),
            ))),
        }
    }

    pub fn iter_bound_columns<'a>(
        column_binding: impl Iterator<Item = &'a ColumnBinding>,
    ) -> (Vec<ExprImpl>, Vec<Option<String>>) {
        column_binding
            .map(|c| {
                (
                    InputRef::new(c.index, c.field.data_type.clone()).into(),
                    Some(c.field.name.clone()),
                )
            })
            .unzip()
    }

    pub fn iter_column_groups(&self) -> (Vec<ExprImpl>, Vec<Option<String>>) {
        self.context
            .column_group_context
            .groups
            .values()
            .rev() // ensure that the outermost col group gets put first in the list
            .map(|g| {
                if let Some(col) = &g.non_nullable_column {
                    let c = &self.context.columns[*col];
                    (
                        InputRef::new(c.index, c.field.data_type.clone()).into(),
                        Some(c.field.name.clone()),
                    )
                } else {
                    let mut input_idxes = g.indices.iter().collect::<Vec<_>>();
                    input_idxes.sort();
                    let inputs = input_idxes
                        .into_iter()
                        .map(|index| {
                            let column = &self.context.columns[*index];
                            InputRef::new(column.index, column.field.data_type.clone()).into()
                        })
                        .collect::<Vec<_>>();
                    let c = &self.context.columns[*g.indices.iter().next().unwrap()];
                    (
                        FunctionCall::new(ExprType::Coalesce, inputs)
                            .expect("Failure binding COALESCE function call")
                            .into(),
                        Some(c.field.name.clone()),
                    )
                }
            })
            .unzip()
    }

    fn bind_distinct_on(&mut self, distinct: Distinct) -> Result<BoundDistinct> {
        Ok(match distinct {
            Distinct::All => BoundDistinct::All,
            Distinct::Distinct => BoundDistinct::Distinct,
            Distinct::DistinctOn(exprs) => {
                let mut bound_exprs = vec![];
                for expr in exprs {
                    bound_exprs.push(self.bind_expr(expr)?);
                }
                BoundDistinct::DistinctOn(bound_exprs)
            }
        })
    }

    fn generate_except_indices(&mut self, except: Option<Vec<Expr>>) -> Result<HashSet<usize>> {
        let mut except_indices: HashSet<usize> = HashSet::new();
        if let Some(exprs) = except {
            for expr in exprs {
                let bound = self.bind_expr(expr)?;
                match bound {
                    ExprImpl::InputRef(inner) => {
                        if !except_indices.insert(inner.index) {
                            return Err(ErrorCode::BindError(
                                "Duplicate entry in except list".into(),
                            )
                            .into());
                        }
                    }
                    _ => {
                        return Err(ErrorCode::BindError(
                            "Only support column name in except list".into(),
                        )
                        .into())
                    }
                }
            }
        }
        Ok(except_indices)
    }
}

fn derive_alias(expr: &Expr) -> Option<String> {
    match expr.clone() {
        Expr::Identifier(ident) => Some(ident.real_value()),
        Expr::CompoundIdentifier(idents) => idents.last().map(|ident| ident.real_value()),
        Expr::FieldIdentifier(_, idents) => idents.last().map(|ident| ident.real_value()),
        Expr::Function(func) => Some(func.name.real_value()),
        Expr::Extract { .. } => Some("extract".to_string()),
        Expr::Case { .. } => Some("case".to_string()),
        Expr::Cast { expr, data_type } => {
            derive_alias(&expr).or_else(|| data_type_to_alias(&data_type))
        }
        Expr::TypedString { data_type, .. } => data_type_to_alias(&data_type),
        Expr::Value(risingwave_sqlparser::ast::Value::Interval { .. }) => {
            Some("interval".to_string())
        }
        Expr::Row(_) => Some("row".to_string()),
        Expr::Array(_) => Some("array".to_string()),
        Expr::ArrayIndex { obj, index: _ } => derive_alias(&obj),
        _ => None,
    }
}

fn data_type_to_alias(data_type: &AstDataType) -> Option<String> {
    let alias = match data_type {
        AstDataType::Char(_) => "bpchar".to_string(),
        AstDataType::Varchar => "varchar".to_string(),
        AstDataType::Uuid => "uuid".to_string(),
        AstDataType::Decimal(_, _) => "numeric".to_string(),
        AstDataType::Real | AstDataType::Float(Some(1..=24)) => "float4".to_string(),
        AstDataType::Double | AstDataType::Float(Some(25..=53) | None) => "float8".to_string(),
        AstDataType::Float(Some(0 | 54..)) => unreachable!(),
        AstDataType::SmallInt => "int2".to_string(),
        AstDataType::Int => "int4".to_string(),
        AstDataType::BigInt => "int8".to_string(),
        AstDataType::Boolean => "bool".to_string(),
        AstDataType::Date => "date".to_string(),
        AstDataType::Time(tz) => format!("time{}", if *tz { "z" } else { "" }),
        AstDataType::Timestamp(tz) => {
            format!("timestamp{}", if *tz { "tz" } else { "" })
        }
        AstDataType::Interval => "interval".to_string(),
        AstDataType::Regclass => "regclass".to_string(),
        AstDataType::Regproc => "regproc".to_string(),
        AstDataType::Text => "text".to_string(),
        AstDataType::Bytea => "bytea".to_string(),
        AstDataType::Array(ty) => return data_type_to_alias(ty),
        AstDataType::Custom(ty) => format!("{}", ty),
        AstDataType::Struct(_) => {
            // Note: Postgres doesn't have anonymous structs
            return None;
        }
    };

    Some(alias)
}
