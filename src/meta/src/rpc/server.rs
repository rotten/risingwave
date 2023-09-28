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

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use either::Either;
use etcd_client::ConnectOptions;
use futures::future::join_all;
use itertools::Itertools;
use model_migration::{Migrator, MigratorTrait};
use regex::Regex;
use risingwave_common::monitor::connection::{RouterExt, TcpConfig};
use risingwave_common::telemetry::manager::TelemetryManager;
use risingwave_common::telemetry::telemetry_env_enabled;
use risingwave_common_service::metrics_manager::MetricsManager;
use risingwave_common_service::tracing::TracingExtractLayer;
use risingwave_pb::backup_service::backup_service_server::BackupServiceServer;
use risingwave_pb::cloud_service::cloud_service_server::CloudServiceServer;
use risingwave_pb::connector_service::sink_coordination_service_server::SinkCoordinationServiceServer;
use risingwave_pb::ddl_service::ddl_service_server::DdlServiceServer;
use risingwave_pb::health::health_server::HealthServer;
use risingwave_pb::hummock::hummock_manager_service_server::HummockManagerServiceServer;
use risingwave_pb::meta::cluster_service_server::ClusterServiceServer;
use risingwave_pb::meta::heartbeat_service_server::HeartbeatServiceServer;
use risingwave_pb::meta::meta_member_service_server::MetaMemberServiceServer;
use risingwave_pb::meta::notification_service_server::NotificationServiceServer;
use risingwave_pb::meta::scale_service_server::ScaleServiceServer;
use risingwave_pb::meta::serving_service_server::ServingServiceServer;
use risingwave_pb::meta::stream_manager_service_server::StreamManagerServiceServer;
use risingwave_pb::meta::system_params_service_server::SystemParamsServiceServer;
use risingwave_pb::meta::telemetry_info_service_server::TelemetryInfoServiceServer;
use risingwave_pb::meta::SystemParams;
use risingwave_pb::user::user_service_server::UserServiceServer;
use risingwave_rpc_client::ComputeClientPool;
use tokio::sync::oneshot::{channel as OneChannel, Receiver as OneReceiver};
use tokio::sync::watch;
use tokio::sync::watch::{Receiver as WatchReceiver, Sender as WatchSender};
use tokio::task::JoinHandle;

use super::intercept::MetricsMiddlewareLayer;
use super::service::health_service::HealthServiceImpl;
use super::service::notification_service::NotificationServiceImpl;
use super::service::scale_service::ScaleServiceImpl;
use super::service::serving_service::ServingServiceImpl;
use super::DdlServiceImpl;
use crate::backup_restore::BackupManager;
use crate::barrier::{BarrierScheduler, GlobalBarrierManager};
use crate::controller::system_param::SystemParamsController;
use crate::controller::SqlMetaStore;
use crate::hummock::HummockManager;
use crate::manager::sink_coordination::SinkCoordinatorManager;
use crate::manager::{
    CatalogManager, ClusterManager, FragmentManager, IdleManager, MetaOpts, MetaSrvEnv,
    SystemParamsManager,
};
use crate::rpc::cloud_provider::AwsEc2Client;
use crate::rpc::election::etcd::EtcdElectionClient;
use crate::rpc::election::ElectionClient;
use crate::rpc::metrics::{
    start_fragment_info_monitor, start_worker_info_monitor, GLOBAL_META_METRICS,
};
use crate::rpc::service::backup_service::BackupServiceImpl;
use crate::rpc::service::cloud_service::CloudServiceImpl;
use crate::rpc::service::cluster_service::ClusterServiceImpl;
use crate::rpc::service::heartbeat_service::HeartbeatServiceImpl;
use crate::rpc::service::hummock_service::HummockServiceImpl;
use crate::rpc::service::meta_member_service::MetaMemberServiceImpl;
use crate::rpc::service::sink_coordination_service::SinkCoordinationServiceImpl;
use crate::rpc::service::stream_service::StreamServiceImpl;
use crate::rpc::service::system_params_service::SystemParamsServiceImpl;
use crate::rpc::service::telemetry_service::TelemetryInfoServiceImpl;
use crate::rpc::service::user_service::UserServiceImpl;
use crate::serving::ServingVnodeMapping;
use crate::storage::{
    EtcdMetaStore, MemStore, MetaStore, MetaStoreBoxExt, MetaStoreRef,
    WrappedEtcdClient as EtcdClient,
};
use crate::stream::{GlobalStreamManager, SourceManager};
use crate::telemetry::{MetaReportCreator, MetaTelemetryInfoFetcher};
use crate::{hummock, serving, MetaError, MetaResult};

#[derive(Debug)]
pub enum MetaStoreBackend {
    Etcd {
        endpoints: Vec<String>,
        credentials: Option<(String, String)>,
    },
    Mem,
}

#[derive(Debug)]
pub struct MetaStoreSqlBackend {
    pub(crate) endpoint: String,
}

#[derive(Clone)]
pub struct AddressInfo {
    pub advertise_addr: String,
    pub listen_addr: SocketAddr,
    pub prometheus_addr: Option<SocketAddr>,
    pub dashboard_addr: Option<SocketAddr>,
    pub ui_path: Option<String>,
}

impl Default for AddressInfo {
    fn default() -> Self {
        Self {
            advertise_addr: "".to_string(),
            listen_addr: SocketAddr::V4("127.0.0.1:0000".parse().unwrap()),
            prometheus_addr: None,
            dashboard_addr: None,
            ui_path: None,
        }
    }
}

pub type ElectionClientRef = Arc<dyn ElectionClient>;

pub async fn rpc_serve(
    address_info: AddressInfo,
    meta_store_backend: MetaStoreBackend,
    meta_store_sql_backend: Option<MetaStoreSqlBackend>,
    max_cluster_heartbeat_interval: Duration,
    lease_interval_secs: u64,
    opts: MetaOpts,
    init_system_params: SystemParams,
) -> MetaResult<(JoinHandle<()>, Option<JoinHandle<()>>, WatchSender<()>)> {
    let meta_store_sql = match meta_store_sql_backend {
        Some(backend) => {
            let mut options = sea_orm::ConnectOptions::new(backend.endpoint);
            options
                .max_connections(20)
                .connect_timeout(Duration::from_secs(10))
                .idle_timeout(Duration::from_secs(30));
            let conn = sea_orm::Database::connect(options).await?;
            Some(SqlMetaStore::new(conn))
        }
        None => None,
    };
    match meta_store_backend {
        MetaStoreBackend::Etcd {
            endpoints,
            credentials,
        } => {
            let mut options = ConnectOptions::default()
                .with_keep_alive(Duration::from_secs(3), Duration::from_secs(5));
            if let Some((username, password)) = &credentials {
                options = options.with_user(username, password)
            }
            let auth_enabled = credentials.is_some();
            let client =
                EtcdClient::connect(endpoints.clone(), Some(options.clone()), auth_enabled)
                    .await
                    .map_err(|e| anyhow::anyhow!("failed to connect etcd {}", e))?;
            let meta_store = EtcdMetaStore::new(client).into_ref();

            // `with_keep_alive` option will break the long connection in election client.
            let mut election_options = ConnectOptions::default();
            if let Some((username, password)) = &credentials {
                election_options = election_options.with_user(username, password)
            }

            let election_client = Arc::new(
                EtcdElectionClient::new(
                    endpoints,
                    Some(election_options),
                    auth_enabled,
                    address_info.advertise_addr.clone(),
                )
                .await?,
            );

            rpc_serve_with_store(
                meta_store,
                Some(election_client),
                meta_store_sql,
                address_info,
                max_cluster_heartbeat_interval,
                lease_interval_secs,
                opts,
                init_system_params,
            )
        }
        MetaStoreBackend::Mem => {
            let meta_store = MemStore::new().into_ref();
            rpc_serve_with_store(
                meta_store,
                None,
                meta_store_sql,
                address_info,
                max_cluster_heartbeat_interval,
                lease_interval_secs,
                opts,
                init_system_params,
            )
        }
    }
}

#[expect(clippy::type_complexity)]
pub fn rpc_serve_with_store(
    meta_store: MetaStoreRef,
    election_client: Option<ElectionClientRef>,
    meta_store_sql: Option<SqlMetaStore>,
    address_info: AddressInfo,
    max_cluster_heartbeat_interval: Duration,
    lease_interval_secs: u64,
    opts: MetaOpts,
    init_system_params: SystemParams,
) -> MetaResult<(JoinHandle<()>, Option<JoinHandle<()>>, WatchSender<()>)> {
    let (svc_shutdown_tx, svc_shutdown_rx) = watch::channel(());

    let leader_lost_handle = if let Some(election_client) = election_client.clone() {
        let stop_rx = svc_shutdown_tx.subscribe();

        let handle = tokio::spawn(async move {
            while let Err(e) = election_client
                .run_once(lease_interval_secs as i64, stop_rx.clone())
                .await
            {
                tracing::error!("election error happened, {}", e.to_string());
            }
        });

        Some(handle)
    } else {
        None
    };

    let join_handle = tokio::spawn(async move {
        if let Some(election_client) = election_client.clone() {
            let mut is_leader_watcher = election_client.subscribe();
            let mut svc_shutdown_rx_clone = svc_shutdown_rx.clone();
            let (follower_shutdown_tx, follower_shutdown_rx) = OneChannel::<()>();

            tokio::select! {
                _ = svc_shutdown_rx_clone.changed() => return,
                res = is_leader_watcher.changed() => {
                    if let Err(err) = res {
                        tracing::error!("leader watcher recv failed {}", err.to_string());
                    }
                }
            }
            let svc_shutdown_rx_clone = svc_shutdown_rx.clone();

            // If not the leader, spawn a follower.
            let follower_handle: Option<JoinHandle<()>> = if !*is_leader_watcher.borrow() {
                let address_info_clone = address_info.clone();

                let election_client_ = election_client.clone();
                Some(tokio::spawn(async move {
                    let _ = tracing::span!(tracing::Level::INFO, "follower services").enter();
                    start_service_as_election_follower(
                        svc_shutdown_rx_clone,
                        follower_shutdown_rx,
                        address_info_clone,
                        Some(election_client_),
                    )
                    .await;
                }))
            } else {
                None
            };

            let mut svc_shutdown_rx_clone = svc_shutdown_rx.clone();
            while !*is_leader_watcher.borrow_and_update() {
                tokio::select! {
                    _ = svc_shutdown_rx_clone.changed() => {
                        return;
                    }
                    res = is_leader_watcher.changed() => {
                        if let Err(err) = res {
                            tracing::error!("leader watcher recv failed {}", err.to_string());
                        }
                    }
                }
            }

            if let Some(handle) = follower_handle {
                let _res = follower_shutdown_tx.send(());
                let _ = handle.await;
            }
        };

        start_service_as_election_leader(
            meta_store,
            meta_store_sql,
            address_info,
            max_cluster_heartbeat_interval,
            opts,
            init_system_params,
            election_client,
            svc_shutdown_rx,
        )
        .await
        .expect("Unable to start leader services");
    });

    Ok((join_handle, leader_lost_handle, svc_shutdown_tx))
}

/// Starts all services needed for the meta follower node
pub async fn start_service_as_election_follower(
    mut svc_shutdown_rx: WatchReceiver<()>,
    follower_shutdown_rx: OneReceiver<()>,
    address_info: AddressInfo,
    election_client: Option<ElectionClientRef>,
) {
    let meta_member_srv = MetaMemberServiceImpl::new(match election_client {
        None => Either::Right(address_info.clone()),
        Some(election_client) => Either::Left(election_client),
    });

    let health_srv = HealthServiceImpl::new();
    tonic::transport::Server::builder()
        .layer(MetricsMiddlewareLayer::new(Arc::new(
            GLOBAL_META_METRICS.clone(),
        )))
        .layer(TracingExtractLayer::new())
        .add_service(MetaMemberServiceServer::new(meta_member_srv))
        .add_service(HealthServer::new(health_srv))
        .monitored_serve_with_shutdown(
            address_info.listen_addr,
            "grpc-meta-follower-service",
            TcpConfig {
                tcp_nodelay: true,
                keepalive_duration: None,
            },
            async move {
                tokio::select! {
                    // shutdown service if all services should be shut down
                    res = svc_shutdown_rx.changed() => {
                        match res {
                            Ok(_) => tracing::info!("Shutting down services"),
                            Err(_) => tracing::error!("Service shutdown sender dropped")
                        }
                    },
                    // shutdown service if follower becomes leader
                    res = follower_shutdown_rx => {
                        match res {
                            Ok(_) => tracing::info!("Shutting down follower services"),
                            Err(_) => tracing::error!("Follower service shutdown sender dropped")
                        }
                    },
                }
            },
        )
        .await;
}

/// Starts all services needed for the meta leader node
/// Only call this function once, since initializing the services multiple times will result in an
/// inconsistent state
///
/// ## Returns
/// Returns an error if the service initialization failed
pub async fn start_service_as_election_leader(
    meta_store: MetaStoreRef,
    meta_store_sql: Option<SqlMetaStore>,
    address_info: AddressInfo,
    max_cluster_heartbeat_interval: Duration,
    opts: MetaOpts,
    init_system_params: SystemParams,
    election_client: Option<ElectionClientRef>,
    mut svc_shutdown_rx: WatchReceiver<()>,
) -> MetaResult<()> {
    tracing::info!("Defining leader services");
    if let Some(sql_store) = &meta_store_sql {
        // Try to upgrade if any new model changes are added.
        Migrator::up(&sql_store.conn, None)
            .await
            .expect("Failed to upgrade models in meta store");
    }

    let prometheus_endpoint = opts.prometheus_endpoint.clone();
    let env = MetaSrvEnv::new(
        opts.clone(),
        init_system_params,
        meta_store.clone(),
        meta_store_sql.clone(),
    )
    .await?;
    let fragment_manager = Arc::new(FragmentManager::new(env.clone()).await.unwrap());

    let system_params_manager = env.system_params_manager_ref();
    let mut system_params_reader = system_params_manager.get_params().await;

    // Using new reader instead if the controller is set.
    let system_params_controller = env.system_params_controller_ref();
    if let Some(ctl) = &system_params_controller {
        system_params_reader = ctl.get_params().await;
    }

    let data_directory = system_params_reader.data_directory();
    if !is_correct_data_directory(data_directory) {
        return Err(MetaError::system_param(format!(
            "The data directory {:?} is misconfigured.
            Please use a combination of uppercase and lowercase letters and numbers, i.e. [a-z, A-Z, 0-9].
            The string cannot start or end with '/', and consecutive '/' are not allowed.
            The data directory cannot be empty and its length should not exceed 800 characters.",
            data_directory
        )));
    }

    let cluster_manager = Arc::new(
        ClusterManager::new(env.clone(), max_cluster_heartbeat_interval)
            .await
            .unwrap(),
    );
    let serving_vnode_mapping = Arc::new(ServingVnodeMapping::default());
    serving::on_meta_start(
        env.notification_manager_ref(),
        cluster_manager.clone(),
        fragment_manager.clone(),
        serving_vnode_mapping.clone(),
    )
    .await;
    let heartbeat_srv = HeartbeatServiceImpl::new(cluster_manager.clone());

    let compactor_manager = Arc::new(
        hummock::CompactorManager::with_meta(env.clone())
            .await
            .unwrap(),
    );

    let catalog_manager = Arc::new(CatalogManager::new(env.clone()).await.unwrap());
    let (compactor_streams_change_tx, compactor_streams_change_rx) =
        tokio::sync::mpsc::unbounded_channel();

    let meta_metrics = Arc::new(GLOBAL_META_METRICS.clone());

    let hummock_manager = hummock::HummockManager::new(
        env.clone(),
        cluster_manager.clone(),
        fragment_manager.clone(),
        meta_metrics.clone(),
        compactor_manager.clone(),
        catalog_manager.clone(),
        compactor_streams_change_tx,
    )
    .await
    .unwrap();

    let meta_member_srv = MetaMemberServiceImpl::new(match election_client.clone() {
        None => Either::Right(address_info.clone()),
        Some(election_client) => Either::Left(election_client),
    });

    #[cfg(not(madsim))]
    let dashboard_task = if let Some(ref dashboard_addr) = address_info.dashboard_addr {
        let dashboard_service = crate::dashboard::DashboardService {
            dashboard_addr: *dashboard_addr,
            prometheus_endpoint: prometheus_endpoint.clone(),
            prometheus_client: prometheus_endpoint.as_ref().map(|x| {
                use std::str::FromStr;
                prometheus_http_query::Client::from_str(x).unwrap()
            }),
            cluster_manager: cluster_manager.clone(),
            fragment_manager: fragment_manager.clone(),
            compute_clients: ComputeClientPool::default(),
            meta_store: env.meta_store_ref(),
            ui_path: address_info.ui_path,
        };
        let task = tokio::spawn(dashboard_service.serve());
        Some(task)
    } else {
        None
    };

    let (barrier_scheduler, scheduled_barriers) = BarrierScheduler::new_pair(
        hummock_manager.clone(),
        meta_metrics.clone(),
        system_params_reader.checkpoint_frequency() as usize,
    );

    let source_manager = Arc::new(
        SourceManager::new(
            env.clone(),
            barrier_scheduler.clone(),
            catalog_manager.clone(),
            fragment_manager.clone(),
            meta_metrics.clone(),
        )
        .await
        .unwrap(),
    );

    let (sink_manager, shutdown_handle) =
        SinkCoordinatorManager::start_worker(env.connector_client());
    let mut sub_tasks = vec![shutdown_handle];

    let barrier_manager = Arc::new(GlobalBarrierManager::new(
        scheduled_barriers,
        env.clone(),
        cluster_manager.clone(),
        catalog_manager.clone(),
        fragment_manager.clone(),
        hummock_manager.clone(),
        source_manager.clone(),
        sink_manager.clone(),
        meta_metrics.clone(),
    ));

    {
        let source_manager = source_manager.clone();
        tokio::spawn(async move {
            source_manager.run().await.unwrap();
        });
    }

    let stream_manager = Arc::new(
        GlobalStreamManager::new(
            env.clone(),
            fragment_manager.clone(),
            barrier_scheduler.clone(),
            cluster_manager.clone(),
            source_manager.clone(),
            hummock_manager.clone(),
        )
        .unwrap(),
    );

    hummock_manager
        .purge(
            &catalog_manager
                .list_tables()
                .await
                .into_iter()
                .map(|t| t.id)
                .collect_vec(),
        )
        .await?;

    // Initialize services.
    let backup_manager = BackupManager::new(
        env.clone(),
        hummock_manager.clone(),
        meta_metrics.clone(),
        system_params_reader.backup_storage_url(),
        system_params_reader.backup_storage_directory(),
    )
    .await?;
    let vacuum_manager = Arc::new(hummock::VacuumManager::new(
        env.clone(),
        hummock_manager.clone(),
        backup_manager.clone(),
        compactor_manager.clone(),
    ));

    let mut aws_cli = None;
    if let Some(my_vpc_id) = &env.opts.vpc_id
        && let Some(security_group_id) = &env.opts.security_group_id
    {
        let cli = AwsEc2Client::new(my_vpc_id, security_group_id).await;
        aws_cli = Some(cli);
    }

    let ddl_srv = DdlServiceImpl::new(
        env.clone(),
        aws_cli.clone(),
        catalog_manager.clone(),
        stream_manager.clone(),
        source_manager.clone(),
        cluster_manager.clone(),
        fragment_manager.clone(),
        barrier_manager.clone(),
        sink_manager.clone(),
    )
    .await;

    let user_srv = UserServiceImpl::new(env.clone(), catalog_manager.clone());

    let scale_srv = ScaleServiceImpl::new(
        fragment_manager.clone(),
        cluster_manager.clone(),
        source_manager,
        catalog_manager.clone(),
        stream_manager.clone(),
        barrier_manager.clone(),
    );

    let cluster_srv = ClusterServiceImpl::new(cluster_manager.clone());
    let stream_srv = StreamServiceImpl::new(
        env.clone(),
        barrier_scheduler.clone(),
        stream_manager.clone(),
        catalog_manager.clone(),
        fragment_manager.clone(),
    );
    let sink_coordination_srv = SinkCoordinationServiceImpl::new(sink_manager);
    let hummock_srv = HummockServiceImpl::new(
        hummock_manager.clone(),
        vacuum_manager.clone(),
        fragment_manager.clone(),
    );
    let notification_srv = NotificationServiceImpl::new(
        env.clone(),
        catalog_manager.clone(),
        cluster_manager.clone(),
        hummock_manager.clone(),
        fragment_manager.clone(),
        backup_manager.clone(),
        serving_vnode_mapping.clone(),
    );
    let health_srv = HealthServiceImpl::new();
    let backup_srv = BackupServiceImpl::new(backup_manager);
    let telemetry_srv = TelemetryInfoServiceImpl::new(meta_store.clone(), env.sql_meta_store());
    let system_params_srv = SystemParamsServiceImpl::new(
        system_params_manager.clone(),
        system_params_controller.clone(),
    );
    let serving_srv =
        ServingServiceImpl::new(serving_vnode_mapping.clone(), fragment_manager.clone());
    let cloud_srv = CloudServiceImpl::new(catalog_manager.clone(), aws_cli);

    if let Some(prometheus_addr) = address_info.prometheus_addr {
        MetricsManager::boot_metrics_service(prometheus_addr.to_string())
    }

    // sub_tasks executed concurrently. Can be shutdown via shutdown_all
    sub_tasks.extend(hummock::start_hummock_workers(
        hummock_manager.clone(),
        vacuum_manager,
        // compaction_scheduler,
        &env.opts,
    ));
    sub_tasks.push(
        start_worker_info_monitor(
            cluster_manager.clone(),
            election_client.clone(),
            Duration::from_secs(env.opts.node_num_monitor_interval_sec),
            meta_metrics.clone(),
        )
        .await,
    );
    sub_tasks.push(
        start_fragment_info_monitor(
            cluster_manager.clone(),
            catalog_manager,
            fragment_manager.clone(),
            hummock_manager.clone(),
            meta_metrics.clone(),
        )
        .await,
    );
    if let Some(system_params_ctl) = system_params_controller {
        sub_tasks.push(SystemParamsController::start_params_notifier(
            system_params_ctl,
        ));
    } else {
        sub_tasks.push(SystemParamsManager::start_params_notifier(
            system_params_manager.clone(),
        ));
    }
    sub_tasks.push(HummockManager::hummock_timer_task(hummock_manager.clone()));
    sub_tasks.push(HummockManager::compaction_event_loop(
        hummock_manager,
        compactor_streams_change_rx,
    ));
    sub_tasks.push(
        serving::start_serving_vnode_mapping_worker(
            env.notification_manager_ref(),
            cluster_manager.clone(),
            fragment_manager.clone(),
            serving_vnode_mapping,
        )
        .await,
    );

    if cfg!(not(test)) {
        sub_tasks.push(ClusterManager::start_heartbeat_checker(
            cluster_manager.clone(),
            Duration::from_secs(1),
        ));
        sub_tasks.push(GlobalBarrierManager::start(barrier_manager));
    }
    let (idle_send, idle_recv) = tokio::sync::oneshot::channel();
    sub_tasks.push(IdleManager::start_idle_checker(
        env.idle_manager_ref(),
        Duration::from_secs(30),
        idle_send,
    ));

    let (abort_sender, abort_recv) = tokio::sync::oneshot::channel();
    let notification_mgr = env.notification_manager_ref();
    let stream_abort_handler = tokio::spawn(async move {
        abort_recv.await.unwrap();
        notification_mgr.abort_all().await;
        compactor_manager.abort_all_compactors();
    });
    sub_tasks.push((stream_abort_handler, abort_sender));

    let telemetry_manager = TelemetryManager::new(
        Arc::new(MetaTelemetryInfoFetcher::new(env.cluster_id().clone())),
        Arc::new(MetaReportCreator::new(
            cluster_manager,
            meta_store.meta_store_type(),
        )),
    );

    // May start telemetry reporting
    if env.opts.telemetry_enabled && telemetry_env_enabled() {
        sub_tasks.push(telemetry_manager.start().await);
    } else {
        tracing::info!("Telemetry didn't start due to meta backend or config");
    }

    let shutdown_all = async move {
        let mut handles = Vec::with_capacity(sub_tasks.len());

        for (join_handle, shutdown_sender) in sub_tasks {
            if let Err(_err) = shutdown_sender.send(()) {
                continue;
            }

            handles.push(join_handle);
        }

        // The barrier manager can't be shutdown gracefully if it's under recovering, try to
        // abort it using timeout.
        match tokio::time::timeout(Duration::from_secs(1), join_all(handles)).await {
            Ok(results) => {
                for result in results {
                    if let Err(err) = result {
                        tracing::warn!("Failed to join shutdown: {:?}", err);
                    }
                }
            }
            Err(e) => {
                tracing::warn!("Join shutdown timeout: {:?}", e);
            }
        }
    };

    // Persist params before starting services so that invalid params that cause meta node
    // to crash will not be persisted.
    if meta_store_sql.is_none() {
        system_params_manager.flush_params().await?;
        env.cluster_id().put_at_meta_store(&meta_store).await?;
    }

    tracing::info!("Assigned cluster id {:?}", *env.cluster_id());
    tracing::info!("Starting meta services");

    tonic::transport::Server::builder()
        .layer(MetricsMiddlewareLayer::new(meta_metrics))
        .layer(TracingExtractLayer::new())
        .add_service(HeartbeatServiceServer::new(heartbeat_srv))
        .add_service(ClusterServiceServer::new(cluster_srv))
        .add_service(StreamManagerServiceServer::new(stream_srv))
        .add_service(
            HummockManagerServiceServer::new(hummock_srv).max_decoding_message_size(usize::MAX),
        )
        .add_service(NotificationServiceServer::new(notification_srv))
        .add_service(MetaMemberServiceServer::new(meta_member_srv))
        .add_service(DdlServiceServer::new(ddl_srv).max_decoding_message_size(usize::MAX))
        .add_service(UserServiceServer::new(user_srv))
        .add_service(ScaleServiceServer::new(scale_srv).max_decoding_message_size(usize::MAX))
        .add_service(HealthServer::new(health_srv))
        .add_service(BackupServiceServer::new(backup_srv))
        .add_service(SystemParamsServiceServer::new(system_params_srv))
        .add_service(TelemetryInfoServiceServer::new(telemetry_srv))
        .add_service(ServingServiceServer::new(serving_srv))
        .add_service(CloudServiceServer::new(cloud_srv))
        .add_service(SinkCoordinationServiceServer::new(sink_coordination_srv))
        .monitored_serve_with_shutdown(
            address_info.listen_addr,
            "grpc-meta-leader-service",
            TcpConfig {
                tcp_nodelay: true,
                keepalive_duration: None,
            },
            async move {
                tokio::select! {
                    res = svc_shutdown_rx.changed() => {
                        match res {
                            Ok(_) => tracing::info!("Shutting down services"),
                            Err(_) => tracing::error!("Service shutdown receiver dropped")
                        }
                        shutdown_all.await;
                    },
                    _ = idle_recv => {
                        shutdown_all.await;
                    },
                }
            },
        )
        .await;

    #[cfg(not(madsim))]
    if let Some(dashboard_task) = dashboard_task {
        // Join the task while ignoring the cancellation error.
        let _ = dashboard_task.await;
    }
    Ok(())
}

fn is_correct_data_directory(data_directory: &str) -> bool {
    let data_directory_regex = Regex::new(r"^[0-9a-zA-Z_/-]{1,}$").unwrap();
    if data_directory.is_empty()
        || !data_directory_regex.is_match(data_directory)
        || data_directory.ends_with('/')
        || data_directory.starts_with('/')
        || data_directory.contains("//")
        || data_directory.len() > 800
    {
        return false;
    }
    true
}
