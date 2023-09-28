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

use std::env;
use std::path::Path;
use std::process::Command;

use anyhow::{anyhow, Result};
use itertools::Itertools;

use super::{ExecuteContext, Task};
use crate::util::{get_program_args, get_program_env_cmd, get_program_name};
use crate::{add_hummock_backend, add_tempo_endpoint, HummockInMemoryStrategy, MetaNodeConfig};

pub struct MetaNodeService {
    config: MetaNodeConfig,
}

impl MetaNodeService {
    pub fn new(config: MetaNodeConfig) -> Result<Self> {
        Ok(Self { config })
    }

    fn meta_node(&self) -> Result<Command> {
        let prefix_bin = env::var("PREFIX_BIN")?;

        if let Ok(x) = env::var("ENABLE_ALL_IN_ONE") && x == "true" {
            Ok(Command::new(
                Path::new(&prefix_bin).join("risingwave").join("meta-node"),
            ))
        } else {
            Ok(Command::new(Path::new(&prefix_bin).join("meta-node")))
        }
    }

    /// Apply command args according to config
    pub fn apply_command_args(
        cmd: &mut Command,
        config: &MetaNodeConfig,
        hummock_in_memory_strategy: HummockInMemoryStrategy,
    ) -> Result<()> {
        cmd.arg("--listen-addr")
            .arg(format!("{}:{}", config.listen_address, config.port))
            .arg("--advertise-addr")
            .arg(format!("{}:{}", config.address, config.port))
            .arg("--dashboard-host")
            .arg(format!(
                "{}:{}",
                config.listen_address, config.dashboard_port
            ));

        cmd.arg("--prometheus-host")
            .arg(format!(
                "{}:{}",
                config.listen_address, config.exporter_port
            ))
            .arg("--connector-rpc-endpoint")
            .arg(&config.connector_rpc_endpoint);

        match config.provide_prometheus.as_ref().unwrap().as_slice() {
            [] => {}
            [prometheus] => {
                cmd.arg("--prometheus-endpoint")
                    .arg(format!("http://{}:{}", prometheus.address, prometheus.port));
            }
            _ => {
                return Err(anyhow!(
                    "unexpected prometheus config {:?}, only 1 instance is supported",
                    config.provide_prometheus
                ))
            }
        }

        let mut is_persistent_meta_store = false;
        match config.provide_etcd_backend.as_ref().unwrap().as_slice() {
            [] => {
                cmd.arg("--backend").arg("mem");
            }
            etcds => {
                is_persistent_meta_store = true;
                cmd.arg("--backend")
                    .arg("etcd")
                    .arg("--etcd-endpoints")
                    .arg(
                        etcds
                            .iter()
                            .map(|etcd| format!("{}:{}", etcd.address, etcd.port))
                            .join(","),
                    );
            }
        }

        let provide_minio = config.provide_minio.as_ref().unwrap();
        let provide_opendal = config.provide_opendal.as_ref().unwrap();
        let provide_aws_s3 = config.provide_aws_s3.as_ref().unwrap();

        let provide_compute_node = config.provide_compute_node.as_ref().unwrap();
        let provide_compactor = config.provide_compactor.as_ref().unwrap();

        let (is_shared_backend, is_persistent_backend) = match (
            config.enable_in_memory_kv_state_backend,
            provide_minio.as_slice(),
            provide_aws_s3.as_slice(),
            provide_opendal.as_slice(),
        ) {
            (true, [], [], []) => {
                cmd.arg("--state-store").arg("in-memory");
                (false, false)
            }
            (true, _, _, _) => {
                return Err(anyhow!(
                    "When `enable_in_memory_kv_state_backend` is enabled, no minio and aws-s3 should be provided.",
                ));
            }
            (_, provide_minio, provide_aws_s3, provide_opendal) => add_hummock_backend(
                &config.id,
                provide_opendal,
                provide_minio,
                provide_aws_s3,
                hummock_in_memory_strategy,
                cmd,
            )?,
        };

        if (provide_compute_node.len() > 1 || !provide_compactor.is_empty()) && !is_shared_backend {
            if config.enable_in_memory_kv_state_backend {
                // Using a non-shared backend with multiple compute nodes will be problematic for
                // state sharing like scaling. However, for distributed end-to-end tests with
                // in-memory state store, this is acceptable.
            } else {
                return Err(anyhow!(
                    "Hummock storage may behave incorrectly with in-memory backend for multiple compute-node or compactor-enabled configuration. Should use a shared backend (e.g. MinIO) instead. Consider adding `use: minio` in risedev config."
                ));
            }
        }

        let provide_compactor = config.provide_compactor.as_ref().unwrap();
        if is_shared_backend && provide_compactor.is_empty() {
            return Err(anyhow!(
                "When using a shared backend (minio, aws-s3, or shared in-memory with `risedev playground`), at least one compactor is required. Consider adding `use: compactor` in risedev config."
            ));
        }
        if is_persistent_meta_store && !is_persistent_backend {
            return Err(anyhow!(
                "When using a persistent meta store (etcd), a persistent state store is required (e.g. minio, aws-s3, etc.)."
            ));
        }

        cmd.arg("--data-directory").arg("hummock_001");

        let provide_tempo = config.provide_tempo.as_ref().unwrap();
        add_tempo_endpoint(provide_tempo, cmd)?;

        Ok(())
    }
}

impl Task for MetaNodeService {
    fn execute(&mut self, ctx: &mut ExecuteContext<impl std::io::Write>) -> anyhow::Result<()> {
        ctx.service(self);
        ctx.pb.set_message("starting...");

        let mut cmd = self.meta_node()?;

        cmd.env("RUST_BACKTRACE", "1");

        if crate::util::is_env_set("RISEDEV_ENABLE_PROFILE") {
            cmd.env(
                "RW_PROFILE_PATH",
                Path::new(&env::var("PREFIX_LOG")?).join(format!("profile-{}", self.id())),
            );
        }

        if crate::util::is_env_set("RISEDEV_ENABLE_HEAP_PROFILE") {
            // See https://linux.die.net/man/3/jemalloc for the descriptions of profiling options
            cmd.env(
                "MALLOC_CONF",
                "prof:true,lg_prof_interval:32,lg_prof_sample:19,prof_prefix:meta-node",
            );
        }

        Self::apply_command_args(&mut cmd, &self.config, HummockInMemoryStrategy::Isolated)?;

        let prefix_config = env::var("PREFIX_CONFIG")?;
        cmd.arg("--config-path")
            .arg(Path::new(&prefix_config).join("risingwave.toml"));

        cmd.arg("--dashboard-ui-path")
            .arg(env::var("PREFIX_UI").unwrap_or_else(|_| ".risingwave/ui".to_owned()));

        if !self.config.user_managed {
            ctx.run_command(ctx.tmux_run(cmd)?)?;
            ctx.pb.set_message("started");
        } else {
            ctx.pb.set_message("user managed");
            writeln!(
                &mut ctx.log,
                "Please use the following parameters to start the meta:\n{}\n{} {}\n\n",
                get_program_env_cmd(&cmd),
                get_program_name(&cmd),
                get_program_args(&cmd)
            )?;
        }

        Ok(())
    }

    fn id(&self) -> String {
        self.config.id.clone()
    }
}
