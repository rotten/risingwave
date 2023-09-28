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

mod compactor_observer;
mod rpc;
pub mod server;
mod telemetry;

use clap::Parser;
use risingwave_common::config::{
    AsyncStackTraceOption, CompactorMode, MetricLevel, OverrideConfig,
};

use crate::server::{compactor_serve, shared_compactor_serve};

/// Command-line arguments for compactor-node.
#[derive(Parser, Clone, Debug, OverrideConfig)]
#[command(
    version,
    about = "The stateless worker node that compacts data for the storage engine"
)]
pub struct CompactorOpts {
    // TODO: rename to listen_addr and separate out the port.
    /// The address that this service listens to.
    /// Usually the localhost + desired port.
    #[clap(long, env = "RW_LISTEN_ADDR", default_value = "127.0.0.1:6660")]
    pub listen_addr: String,

    /// The address for contacting this instance of the service.
    /// This would be synonymous with the service's "public address"
    /// or "identifying address".
    /// Optional, we will use listen_addr if not specified.
    #[clap(long, env = "RW_ADVERTISE_ADDR")]
    pub advertise_addr: Option<String>,

    // TODO: This is currently unused.
    #[clap(long, env = "RW_PORT")]
    pub port: Option<u16>,

    #[clap(
        long,
        env = "RW_PROMETHEUS_LISTENER_ADDR",
        default_value = "127.0.0.1:1260"
    )]
    pub prometheus_listener_addr: String,

    #[clap(long, env = "RW_META_ADDR", default_value = "http://127.0.0.1:5690")]
    pub meta_address: String,

    #[clap(long, env = "RW_COMPACTION_WORKER_THREADS_NUMBER")]
    pub compaction_worker_threads_number: Option<usize>,

    /// The path of `risingwave.toml` configuration file.
    ///
    /// If empty, default configuration values will be used.
    #[clap(long, env = "RW_CONFIG_PATH", default_value = "")]
    pub config_path: String,

    /// Used for control the metrics level, similar to log level.
    #[clap(long, env = "RW_METRICS_LEVEL")]
    #[override_opts(path = server.metrics_level)]
    pub metrics_level: Option<MetricLevel>,

    /// Enable async stack tracing through `await-tree` for risectl.
    #[clap(long, env = "RW_ASYNC_STACK_TRACE", value_enum)]
    #[override_opts(path = streaming.async_stack_trace)]
    pub async_stack_trace: Option<AsyncStackTraceOption>,

    #[clap(long, env = "RW_OBJECT_STORE_STREAMING_READ_TIMEOUT_MS", value_enum)]
    #[override_opts(path = storage.object_store_streaming_read_timeout_ms)]
    pub object_store_streaming_read_timeout_ms: Option<u64>,
    #[clap(long, env = "RW_OBJECT_STORE_STREAMING_UPLOAD_TIMEOUT_MS", value_enum)]
    #[override_opts(path = storage.object_store_streaming_upload_timeout_ms)]
    pub object_store_streaming_upload_timeout_ms: Option<u64>,
    #[clap(long, env = "RW_OBJECT_STORE_UPLOAD_TIMEOUT_MS", value_enum)]
    #[override_opts(path = storage.object_store_upload_timeout_ms)]
    pub object_store_upload_timeout_ms: Option<u64>,
    #[clap(long, env = "RW_OBJECT_STORE_READ_TIMEOUT_MS", value_enum)]
    #[override_opts(path = storage.object_store_read_timeout_ms)]
    pub object_store_read_timeout_ms: Option<u64>,

    #[clap(long, env = "RW_COMPACTOR_MODE", value_enum)]
    pub compactor_mode: Option<CompactorMode>,

    #[clap(long, env = "RW_PROXY_RPC_ENDPOINT", default_value = "")]
    pub proxy_rpc_endpoint: String,
}

use std::future::Future;
use std::pin::Pin;

pub fn start(opts: CompactorOpts) -> Pin<Box<dyn Future<Output = ()> + Send>> {
    // WARNING: don't change the function signature. Making it `async fn` will cause
    // slow compile in release mode.
    match opts.compactor_mode {
        Some(CompactorMode::Shared) => Box::pin(async move {
            tracing::info!("Shared compactor pod options: {:?}", opts);
            tracing::info!("Proxy rpc endpoint: {}", opts.proxy_rpc_endpoint.clone());

            let listen_addr = opts.listen_addr.parse().unwrap();

            let (join_handle, _shutdown_sender) = shared_compactor_serve(listen_addr, opts).await;

            tracing::info!("Server listening at {}", listen_addr);

            join_handle.await.unwrap();
        }),
        None | Some(CompactorMode::Dedicated) => Box::pin(async move {
            tracing::info!("Compactor node options: {:?}", opts);
            tracing::info!("meta address: {}", opts.meta_address.clone());

            let listen_addr = opts.listen_addr.parse().unwrap();

            let advertise_addr = opts
                .advertise_addr
                .as_ref()
                .unwrap_or_else(|| {
                    tracing::warn!("advertise addr is not specified, defaulting to listen address");
                    &opts.listen_addr
                })
                .parse()
                .unwrap();
            tracing::info!(" address is {}", advertise_addr);
            let (join_handle, observer_join_handle, _shutdown_sender) =
                compactor_serve(listen_addr, advertise_addr, opts).await;

            tracing::info!("Server listening at {}", listen_addr);

            join_handle.await.unwrap();
            observer_join_handle.abort();
        }),
    }
}
