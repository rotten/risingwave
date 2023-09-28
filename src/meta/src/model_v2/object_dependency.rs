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

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq)]
#[sea_orm(table_name = "object_dependency")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    pub oid: i32,
    pub used_by: i32,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::object::Entity",
        from = "Column::Oid",
        to = "super::object::Column::Oid",
        on_update = "NoAction",
        on_delete = "Cascade"
    )]
    Object2,
    #[sea_orm(
        belongs_to = "super::object::Entity",
        from = "Column::UsedBy",
        to = "super::object::Column::Oid",
        on_update = "NoAction",
        on_delete = "Cascade"
    )]
    Object1,
}

impl ActiveModelBehavior for ActiveModel {}
