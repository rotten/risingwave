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
pub enum WorkerType {
    #[sea_orm(string_value = "FRONTEND")]
    Frontend,
    #[sea_orm(string_value = "COMPUTE_NODE")]
    ComputeNode,
    #[sea_orm(string_value = "RISE_CTL")]
    RiseCtl,
    #[sea_orm(string_value = "COMPACTOR")]
    Compactor,
    #[sea_orm(string_value = "META")]
    Meta,
}

#[derive(Clone, Debug, PartialEq, Eq, EnumIter, DeriveActiveEnum)]
#[sea_orm(rs_type = "String", db_type = "String(None)")]
pub enum WorkerStatus {
    #[sea_orm(string_value = "STARTING")]
    Starting,
    #[sea_orm(string_value = "RUNNING")]
    Running,
}

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq)]
#[sea_orm(table_name = "worker")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub worker_id: i32,
    pub worker_type: WorkerType,
    pub host: String,
    pub port: i32,
    pub status: WorkerStatus,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(has_many = "super::worker_property::Entity")]
    WorkerProperty,
}

impl Related<super::worker_property::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::WorkerProperty.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
