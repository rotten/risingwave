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

//! Function signatures.

use std::collections::HashMap;
use std::fmt;
use std::sync::LazyLock;

use risingwave_common::types::{DataType, DataTypeName};
use risingwave_pb::expr::expr_node::PbType;

use super::FuncSigDebug;
use crate::error::Result;
use crate::expr::BoxedExpression;

pub static FUNC_SIG_MAP: LazyLock<FuncSigMap> = LazyLock::new(|| unsafe {
    let mut map = FuncSigMap::default();
    tracing::info!("{} function signatures loaded.", FUNC_SIG_MAP_INIT.len());
    for desc in FUNC_SIG_MAP_INIT.drain(..) {
        map.insert(desc);
    }
    map
});

/// The table of function signatures.
pub fn func_sigs() -> impl Iterator<Item = &'static FuncSign> {
    FUNC_SIG_MAP.0.values().flatten()
}

#[derive(Default, Clone, Debug)]
pub struct FuncSigMap(HashMap<PbType, Vec<FuncSign>>);

impl FuncSigMap {
    /// Inserts a function signature.
    pub fn insert(&mut self, desc: FuncSign) {
        self.0.entry(desc.func).or_default().push(desc)
    }

    /// Returns a function signature with the same type, argument types and return type.
    /// Deprecated functions are included.
    pub fn get(&self, ty: PbType, args: &[DataTypeName], ret: DataTypeName) -> Option<&FuncSign> {
        let v = self.0.get(&ty)?;
        v.iter()
            .find(|d| (d.variadic || d.inputs_type == args) && d.ret_type == ret)
    }

    /// Returns all function signatures with the same type and number of arguments.
    /// Deprecated functions are excluded.
    pub fn get_with_arg_nums(&self, ty: PbType, nargs: usize) -> Vec<&FuncSign> {
        match self.0.get(&ty) {
            Some(v) => v
                .iter()
                .filter(|d| (d.variadic || d.inputs_type.len() == nargs) && !d.deprecated)
                .collect(),
            None => vec![],
        }
    }
}

/// A function signature.
#[derive(Clone)]
pub struct FuncSign {
    pub func: PbType,
    pub inputs_type: &'static [DataTypeName],
    pub variadic: bool,
    pub ret_type: DataTypeName,
    pub build: fn(return_type: DataType, children: Vec<BoxedExpression>) -> Result<BoxedExpression>,
    /// Whether the function is deprecated and should not be used in the frontend.
    /// For backward compatibility, it is still available in the backend.
    pub deprecated: bool,
}

impl fmt::Debug for FuncSign {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        FuncSigDebug {
            func: self.func.as_str_name(),
            inputs_type: self.inputs_type,
            ret_type: self.ret_type,
            set_returning: false,
            deprecated: self.deprecated,
            append_only: false,
        }
        .fmt(f)
    }
}

/// Register a function into global registry.
///
/// # Safety
///
/// This function must be called sequentially.
///
/// It is designed to be used by `#[function]` macro.
/// Users SHOULD NOT call this function.
#[doc(hidden)]
pub unsafe fn _register(desc: FuncSign) {
    FUNC_SIG_MAP_INIT.push(desc)
}

/// The global registry of function signatures on initialization.
///
/// `#[function]` macro will generate a `#[ctor]` function to register the signature into this
/// vector. The calls are guaranteed to be sequential. The vector will be drained and moved into
/// `FUNC_SIG_MAP` on the first access of `FUNC_SIG_MAP`.
static mut FUNC_SIG_MAP_INIT: Vec<FuncSign> = Vec::new();
