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

#![expect(clippy::arc_with_non_send_sync, reason = "FIXME: later")]

pub mod compaction_config;
mod overlap_strategy;
use risingwave_common::catalog::TableOption;
use risingwave_hummock_sdk::prost_key_range::KeyRangeExt;
use risingwave_pb::hummock::compact_task::{self, TaskStatus};

mod picker;
pub mod selector;

use std::collections::{HashMap, HashSet};
use std::fmt::{Debug, Formatter};
use std::sync::Arc;

use picker::{LevelCompactionPicker, TierCompactionPicker};
use risingwave_hummock_sdk::{
    can_concat, CompactionGroupId, HummockCompactionTaskId, HummockEpoch,
};
use risingwave_pb::hummock::compaction_config::CompactionMode;
use risingwave_pb::hummock::hummock_version::Levels;
use risingwave_pb::hummock::{CompactTask, CompactionConfig, KeyRange, LevelType};
pub use selector::CompactionSelector;

use self::selector::LocalSelectorStatistic;
use crate::hummock::compaction::overlap_strategy::{OverlapStrategy, RangeOverlapStrategy};
use crate::hummock::compaction::picker::CompactionInput;
use crate::hummock::level_handler::LevelHandler;
use crate::hummock::model::CompactionGroup;

pub struct CompactStatus {
    pub(crate) compaction_group_id: CompactionGroupId,
    pub(crate) level_handlers: Vec<LevelHandler>,
}

impl Debug for CompactStatus {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompactStatus")
            .field("compaction_group_id", &self.compaction_group_id)
            .field("level_handlers", &self.level_handlers)
            .finish()
    }
}

impl PartialEq for CompactStatus {
    fn eq(&self, other: &Self) -> bool {
        self.level_handlers.eq(&other.level_handlers)
            && self.compaction_group_id == other.compaction_group_id
    }
}

impl Clone for CompactStatus {
    fn clone(&self) -> Self {
        Self {
            compaction_group_id: self.compaction_group_id,
            level_handlers: self.level_handlers.clone(),
        }
    }
}

pub struct CompactionTask {
    pub input: CompactionInput,
    pub base_level: usize,
    pub compression_algorithm: String,
    pub target_file_size: u64,
    pub compaction_task_type: compact_task::TaskType,
    pub enable_split_by_table: bool,
}

pub fn create_overlap_strategy(compaction_mode: CompactionMode) -> Arc<dyn OverlapStrategy> {
    match compaction_mode {
        CompactionMode::Range => Arc::new(RangeOverlapStrategy::default()),
        CompactionMode::Unspecified => unreachable!(),
    }
}

impl CompactStatus {
    pub fn new(compaction_group_id: CompactionGroupId, max_level: u64) -> CompactStatus {
        let mut level_handlers = vec![];
        for level in 0..=max_level {
            level_handlers.push(LevelHandler::new(level as u32));
        }
        CompactStatus {
            compaction_group_id,
            level_handlers,
        }
    }

    pub fn get_compact_task(
        &mut self,
        levels: &Levels,
        task_id: HummockCompactionTaskId,
        group: &CompactionGroup,
        stats: &mut LocalSelectorStatistic,
        selector: &mut Box<dyn CompactionSelector>,
        table_id_to_options: HashMap<u32, TableOption>,
    ) -> Option<CompactTask> {
        // When we compact the files, we must make the result of compaction meet the following
        // conditions, for any user key, the epoch of it in the file existing in the lower
        // layer must be larger.
        let ret = selector.pick_compaction(
            task_id,
            group,
            levels,
            &mut self.level_handlers,
            stats,
            table_id_to_options,
        )?;
        let target_level_id = ret.input.target_level;

        let compression_algorithm = match ret.compression_algorithm.as_str() {
            "Lz4" => 1,
            "Zstd" => 2,
            _ => 0,
        };

        let compact_task = CompactTask {
            input_ssts: ret.input.input_levels,
            splits: vec![KeyRange::inf()],
            watermark: HummockEpoch::MAX,
            sorted_output_ssts: vec![],
            task_id,
            target_level: target_level_id as u32,
            // only gc delete keys in last level because there may be older version in more bottom
            // level.
            gc_delete_keys: target_level_id == self.level_handlers.len() - 1,
            base_level: ret.base_level as u32,
            task_status: TaskStatus::Pending as i32,
            compaction_group_id: group.group_id,
            existing_table_ids: vec![],
            compression_algorithm,
            target_file_size: ret.target_file_size,
            compaction_filter_mask: 0,
            table_options: HashMap::default(),
            current_epoch_time: 0,
            target_sub_level_id: ret.input.target_sub_level_id,
            task_type: ret.compaction_task_type as i32,
            split_by_state_table: group.compaction_config.split_by_state_table,
            split_weight_by_vnode: group.compaction_config.split_weight_by_vnode,
        };
        Some(compact_task)
    }

    pub fn is_trivial_move_task(task: &CompactTask) -> bool {
        if task.input_ssts.len() == 1 {
            return task.input_ssts[0].level_idx == 0
                && can_concat(&task.input_ssts[0].table_infos);
        } else if task.input_ssts.len() != 2
            || task.input_ssts[0].level_type() != LevelType::Nonoverlapping
        {
            return false;
        }

        // it may be a manual compaction task
        if task.input_ssts[0].level_idx == task.input_ssts[1].level_idx
            && task.input_ssts[0].level_idx > 0
        {
            return false;
        }

        if task.input_ssts[1].level_idx == task.target_level
            && task.input_ssts[1].table_infos.is_empty()
        {
            return true;
        }

        false
    }

    pub fn is_trivial_reclaim(task: &CompactTask) -> bool {
        let exist_table_ids = HashSet::<u32>::from_iter(task.existing_table_ids.clone());
        task.input_ssts.iter().all(|level| {
            level.table_infos.iter().all(|sst| {
                sst.table_ids
                    .iter()
                    .all(|table_id| !exist_table_ids.contains(table_id))
            })
        })
    }

    /// Declares a task as either succeeded, failed or canceled.
    pub fn report_compact_task(&mut self, compact_task: &CompactTask) {
        for level in &compact_task.input_ssts {
            self.level_handlers[level.level_idx as usize].remove_task(compact_task.task_id);
        }
    }

    pub fn compaction_group_id(&self) -> CompactionGroupId {
        self.compaction_group_id
    }
}

pub fn create_compaction_task(
    compaction_config: &CompactionConfig,
    input: CompactionInput,
    base_level: usize,
    compaction_task_type: compact_task::TaskType,
) -> CompactionTask {
    let target_file_size = if input.target_level == 0 {
        compaction_config.target_file_size_base
    } else if input.target_level == base_level {
        // This is just a temporary optimization measure. We hope to reduce the size of SST as much
        // as possible to reduce the amount of data blocked by a single task during compaction,
        // but too many files will increase computing overhead.
        // TODO: remove it after can reduce configuration `target_file_size_base`.
        compaction_config.target_file_size_base / 4
    } else {
        assert!(input.target_level >= base_level);
        let step = (input.target_level - base_level) / 2;
        compaction_config.target_file_size_base << step
    };

    CompactionTask {
        compression_algorithm: get_compression_algorithm(
            compaction_config,
            base_level,
            input.target_level,
        ),
        base_level,
        input,
        target_file_size,
        compaction_task_type,
        enable_split_by_table: false,
    }
}

pub fn get_compression_algorithm(
    compaction_config: &CompactionConfig,
    base_level: usize,
    level: usize,
) -> String {
    if level == 0 || level < base_level {
        compaction_config.compression_algorithm[0].clone()
    } else {
        let idx = level - base_level + 1;
        compaction_config.compression_algorithm[idx].clone()
    }
}
