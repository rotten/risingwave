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

use risingwave_common::array::{ListValue, StructValue};
use risingwave_common::row::Row;
use risingwave_common::types::ToOwnedDatum;
use risingwave_expr::function;

#[function("array(...) -> list")]
fn array(row: impl Row) -> ListValue {
    ListValue::new(row.iter().map(|d| d.to_owned_datum()).collect())
}

#[function("row(...) -> struct")]
fn row_(row: impl Row) -> StructValue {
    StructValue::new(row.iter().map(|d| d.to_owned_datum()).collect())
}
