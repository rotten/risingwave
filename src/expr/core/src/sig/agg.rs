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

use std::collections::HashMap;
use std::fmt;
use std::sync::LazyLock;

use risingwave_common::types::DataTypeName;

use super::FuncSigDebug;
use crate::aggregate::{AggCall, AggKind, BoxedAggregateFunction};
use crate::Result;

pub static AGG_FUNC_SIG_MAP: LazyLock<AggFuncSigMap> = LazyLock::new(|| unsafe {
    let mut map = AggFuncSigMap::default();
    tracing::info!("{} aggregations loaded.", AGG_FUNC_SIG_MAP_INIT.len());
    for desc in AGG_FUNC_SIG_MAP_INIT.drain(..) {
        map.insert(desc);
    }
    map
});

// Same as FuncSign in func.rs except this is for aggregate function
#[derive(PartialEq, Eq, Hash, Clone)]
pub struct AggFuncSig {
    pub func: AggKind,
    pub inputs_type: &'static [DataTypeName],
    pub state_type: DataTypeName,
    pub ret_type: DataTypeName,
    pub build: fn(agg: &AggCall) -> Result<BoxedAggregateFunction>,
    pub append_only: bool,
}

impl fmt::Debug for AggFuncSig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        FuncSigDebug {
            func: self.func,
            inputs_type: self.inputs_type,
            ret_type: self.ret_type,
            set_returning: false,
            deprecated: false,
            append_only: self.append_only,
        }
        .fmt(f)
    }
}

// Same as FuncSigMap in func.rs except this is for aggregate function
#[derive(Default)]
pub struct AggFuncSigMap(HashMap<(AggKind, usize), Vec<AggFuncSig>>);

impl AggFuncSigMap {
    /// Inserts a function signature into the map.
    fn insert(&mut self, sig: AggFuncSig) {
        let arity = sig.inputs_type.len();
        self.0.entry((sig.func, arity)).or_default().push(sig);
    }

    /// Returns a function signature with the given type, argument types, return type.
    ///
    /// The `append_only` flag only works when both append-only and retractable version exist.
    /// Otherwise, return the signature of the only version.
    pub fn get(
        &self,
        ty: AggKind,
        args: &[DataTypeName],
        ret: DataTypeName,
        append_only: bool,
    ) -> Option<&AggFuncSig> {
        let v = self.0.get(&(ty, args.len()))?;
        let mut iter = v
            .iter()
            .filter(|d| d.inputs_type == args && d.ret_type == ret);
        if iter.clone().count() == 2 {
            iter.find(|d| d.append_only == append_only)
        } else {
            iter.next()
        }
    }

    /// Returns the return type for the given function and arguments.
    pub fn get_return_type(&self, ty: AggKind, args: &[DataTypeName]) -> Option<DataTypeName> {
        let v = self.0.get(&(ty, args.len()))?;
        v.iter().find(|d| d.inputs_type == args).map(|d| d.ret_type)
    }
}

/// The table of function signatures.
pub fn agg_func_sigs() -> impl Iterator<Item = &'static AggFuncSig> {
    AGG_FUNC_SIG_MAP.0.values().flatten()
}

/// Register a function into global registry.
///
/// # Safety
///
/// This function must be called sequentially.
///
/// It is designed to be used by `#[aggregate]` macro.
/// Users SHOULD NOT call this function.
#[doc(hidden)]
pub unsafe fn _register(desc: AggFuncSig) {
    AGG_FUNC_SIG_MAP_INIT.push(desc);
}

/// The global registry of function signatures on initialization.
///
/// `#[aggregate]` macro will generate a `#[ctor]` function to register the signature into this
/// vector. The calls are guaranteed to be sequential. The vector will be drained and moved into
/// `AGG_FUNC_SIG_MAP` on the first access of `AGG_FUNC_SIG_MAP`.
static mut AGG_FUNC_SIG_MAP_INIT: Vec<AggFuncSig> = Vec::new();
