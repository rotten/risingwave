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

#[derive(Clone, Debug, PartialEq, Eq, EnumIter, DeriveActiveEnum)]
#[sea_orm(rs_type = "String", db_type = "String(None)")]
pub enum ObjectType {
    #[sea_orm(string_value = "DATABASE")]
    Database,
    #[sea_orm(string_value = "SCHEMA")]
    Schema,
    #[sea_orm(string_value = "TABLE")]
    Table,
    #[sea_orm(string_value = "SOURCE")]
    Source,
    #[sea_orm(string_value = "SINK")]
    Sink,
    #[sea_orm(string_value = "VIEW")]
    View,
    #[sea_orm(string_value = "INDEX")]
    Index,
    #[sea_orm(string_value = "FUNCTION")]
    Function,
    #[sea_orm(string_value = "CONNECTION")]
    Connection,
}

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq)]
#[sea_orm(table_name = "object")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub oid: i32,
    pub obj_type: ObjectType,
    pub owner_id: i32,
    pub initialized_at: DateTime,
    pub created_at: DateTime,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(has_many = "super::connection::Entity")]
    Connection,
    #[sea_orm(has_many = "super::database::Entity")]
    Database,
    #[sea_orm(has_many = "super::fragment::Entity")]
    Fragment,
    #[sea_orm(has_many = "super::function::Entity")]
    Function,
    #[sea_orm(has_many = "super::index::Entity")]
    Index,
    #[sea_orm(has_many = "super::schema::Entity")]
    Schema,
    #[sea_orm(has_many = "super::sink::Entity")]
    Sink,
    #[sea_orm(has_many = "super::source::Entity")]
    Source,
    #[sea_orm(has_many = "super::table::Entity")]
    Table,
    #[sea_orm(
        belongs_to = "super::user::Entity",
        from = "Column::OwnerId",
        to = "super::user::Column::UserId",
        on_update = "NoAction",
        on_delete = "NoAction"
    )]
    User,
    #[sea_orm(has_many = "super::user_privilege::Entity")]
    UserPrivilege,
    #[sea_orm(has_many = "super::view::Entity")]
    View,
}

impl Related<super::connection::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Connection.def()
    }
}

impl Related<super::database::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Database.def()
    }
}

impl Related<super::fragment::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Fragment.def()
    }
}

impl Related<super::function::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Function.def()
    }
}

impl Related<super::index::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Index.def()
    }
}

impl Related<super::schema::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Schema.def()
    }
}

impl Related<super::sink::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Sink.def()
    }
}

impl Related<super::source::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Source.def()
    }
}

impl Related<super::table::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Table.def()
    }
}

impl Related<super::user::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::User.def()
    }
}

impl Related<super::user_privilege::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::UserPrivilege.def()
    }
}

impl Related<super::view::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::View.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
