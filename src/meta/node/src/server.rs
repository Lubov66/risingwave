// Copyright 2024 RisingWave Labs
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

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use etcd_client::ConnectOptions;
use otlp_embedded::TraceServiceServer;
use regex::Regex;
use risingwave_common::monitor::{RouterExt, TcpConfig};
use risingwave_common::secret::LocalSecretManager;
use risingwave_common::session_config::SessionConfig;
use risingwave_common::system_param::reader::SystemParamsRead;
use risingwave_common::telemetry::manager::TelemetryManager;
use risingwave_common::telemetry::{report_scarf_enabled, report_to_scarf, telemetry_env_enabled};
use risingwave_common::util::tokio_util::sync::CancellationToken;
use risingwave_common_service::{MetricsManager, TracingExtractLayer};
use risingwave_meta::barrier::StreamRpcManager;
use risingwave_meta::controller::catalog::CatalogController;
use risingwave_meta::controller::cluster::ClusterController;
use risingwave_meta::manager::{
    MetaStoreImpl, MetadataManager, SystemParamsManagerImpl, META_NODE_ID,
};
use risingwave_meta::rpc::election::dummy::DummyElectionClient;
use risingwave_meta::rpc::intercept::MetricsMiddlewareLayer;
use risingwave_meta::rpc::ElectionClientRef;
use risingwave_meta::stream::ScaleController;
use risingwave_meta::MetaStoreBackend;
use risingwave_meta_service::backup_service::BackupServiceImpl;
use risingwave_meta_service::cloud_service::CloudServiceImpl;
use risingwave_meta_service::cluster_service::ClusterServiceImpl;
use risingwave_meta_service::ddl_service::DdlServiceImpl;
use risingwave_meta_service::event_log_service::EventLogServiceImpl;
use risingwave_meta_service::health_service::HealthServiceImpl;
use risingwave_meta_service::heartbeat_service::HeartbeatServiceImpl;
use risingwave_meta_service::hummock_service::HummockServiceImpl;
use risingwave_meta_service::meta_member_service::MetaMemberServiceImpl;
use risingwave_meta_service::notification_service::NotificationServiceImpl;
use risingwave_meta_service::scale_service::ScaleServiceImpl;
use risingwave_meta_service::serving_service::ServingServiceImpl;
use risingwave_meta_service::session_config::SessionParamsServiceImpl;
use risingwave_meta_service::sink_coordination_service::SinkCoordinationServiceImpl;
use risingwave_meta_service::stream_service::StreamServiceImpl;
use risingwave_meta_service::system_params_service::SystemParamsServiceImpl;
use risingwave_meta_service::telemetry_service::TelemetryInfoServiceImpl;
use risingwave_meta_service::user_service::UserServiceImpl;
use risingwave_meta_service::AddressInfo;
use risingwave_pb::backup_service::backup_service_server::BackupServiceServer;
use risingwave_pb::cloud_service::cloud_service_server::CloudServiceServer;
use risingwave_pb::connector_service::sink_coordination_service_server::SinkCoordinationServiceServer;
use risingwave_pb::ddl_service::ddl_service_server::DdlServiceServer;
use risingwave_pb::health::health_server::HealthServer;
use risingwave_pb::hummock::hummock_manager_service_server::HummockManagerServiceServer;
use risingwave_pb::meta::cluster_service_server::ClusterServiceServer;
use risingwave_pb::meta::event_log_service_server::EventLogServiceServer;
use risingwave_pb::meta::heartbeat_service_server::HeartbeatServiceServer;
use risingwave_pb::meta::meta_member_service_server::MetaMemberServiceServer;
use risingwave_pb::meta::notification_service_server::NotificationServiceServer;
use risingwave_pb::meta::scale_service_server::ScaleServiceServer;
use risingwave_pb::meta::serving_service_server::ServingServiceServer;
use risingwave_pb::meta::session_param_service_server::SessionParamServiceServer;
use risingwave_pb::meta::stream_manager_service_server::StreamManagerServiceServer;
use risingwave_pb::meta::system_params_service_server::SystemParamsServiceServer;
use risingwave_pb::meta::telemetry_info_service_server::TelemetryInfoServiceServer;
use risingwave_pb::meta::SystemParams;
use risingwave_pb::user::user_service_server::UserServiceServer;
use risingwave_rpc_client::ComputeClientPool;
use sea_orm::{ConnectionTrait, DbBackend};
use thiserror_ext::AsReport;
use tokio::sync::watch;

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
use crate::rpc::election::sql::{
    MySqlDriver, PostgresDriver, SqlBackendElectionClient, SqliteDriver,
};
use crate::rpc::metrics::{
    start_fragment_info_monitor, start_worker_info_monitor, GLOBAL_META_METRICS,
};
use crate::serving::ServingVnodeMapping;
use crate::storage::{EtcdMetaStore, MemStore, MetaStoreBoxExt, WrappedEtcdClient as EtcdClient};
use crate::stream::{GlobalStreamManager, SourceManager};
use crate::telemetry::{MetaReportCreator, MetaTelemetryInfoFetcher};
use crate::{hummock, serving, MetaError, MetaResult};

/// Used for standalone mode checking the status of the meta service.
/// This can be easier and more accurate than checking the TCP connection.
pub mod started {
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering::Relaxed;

    static STARTED: AtomicBool = AtomicBool::new(false);

    /// Mark the meta service as started.
    pub(crate) fn set() {
        STARTED.store(true, Relaxed);
    }

    /// Check if the meta service has started.
    pub fn get() -> bool {
        STARTED.load(Relaxed)
    }
}

/// A wrapper around [`rpc_serve_with_store`] that dispatches different store implementations.
///
/// For the timing of returning, see [`rpc_serve_with_store`].
pub async fn rpc_serve(
    address_info: AddressInfo,
    meta_store_backend: MetaStoreBackend,
    max_cluster_heartbeat_interval: Duration,
    lease_interval_secs: u64,
    opts: MetaOpts,
    init_system_params: SystemParams,
    init_session_config: SessionConfig,
    shutdown: CancellationToken,
) -> MetaResult<()> {
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
                    .context("failed to connect etcd")?;
            let meta_store = EtcdMetaStore::new(client).into_ref();

            // `with_keep_alive` option will break the long connection in election client.
            let mut election_options = ConnectOptions::default();
            if let Some((username, password)) = &credentials {
                election_options = election_options.with_user(username, password)
            }

            let election_client: ElectionClientRef = Arc::new(
                EtcdElectionClient::new(
                    endpoints,
                    Some(election_options),
                    auth_enabled,
                    address_info.advertise_addr.clone(),
                )
                .await?,
            );

            rpc_serve_with_store(
                MetaStoreImpl::Kv(meta_store),
                election_client,
                address_info,
                max_cluster_heartbeat_interval,
                lease_interval_secs,
                opts,
                init_system_params,
                init_session_config,
                shutdown,
            )
            .await
        }
        MetaStoreBackend::Mem => {
            let meta_store = MemStore::new().into_ref();
            let dummy_election_client = Arc::new(DummyElectionClient::new(
                address_info.advertise_addr.clone(),
            ));
            rpc_serve_with_store(
                MetaStoreImpl::Kv(meta_store),
                dummy_election_client,
                address_info,
                max_cluster_heartbeat_interval,
                lease_interval_secs,
                opts,
                init_system_params,
                init_session_config,
                shutdown,
            )
            .await
        }
        MetaStoreBackend::Sql { endpoint } => {
            let max_connection = if DbBackend::Sqlite.is_prefix_of(&endpoint) {
                // Due to the fact that Sqlite is prone to the error "(code: 5) database is locked" under concurrent access,
                // here we forcibly specify the number of connections as 1.
                1
            } else {
                10
            };

            let mut options = sea_orm::ConnectOptions::new(endpoint);
            options
                .max_connections(max_connection)
                .connect_timeout(Duration::from_secs(10))
                .idle_timeout(Duration::from_secs(30));
            let conn = sea_orm::Database::connect(options).await?;
            let meta_store_sql = SqlMetaStore::new(conn);

            // Init election client.
            let id = address_info.advertise_addr.clone();
            let conn = meta_store_sql.conn.clone();
            let election_client: ElectionClientRef = match conn.get_database_backend() {
                DbBackend::Sqlite => {
                    Arc::new(SqlBackendElectionClient::new(id, SqliteDriver::new(conn)))
                }
                DbBackend::Postgres => {
                    Arc::new(SqlBackendElectionClient::new(id, PostgresDriver::new(conn)))
                }
                DbBackend::MySql => {
                    Arc::new(SqlBackendElectionClient::new(id, MySqlDriver::new(conn)))
                }
            };
            election_client.init().await?;

            rpc_serve_with_store(
                MetaStoreImpl::Sql(meta_store_sql),
                election_client,
                address_info,
                max_cluster_heartbeat_interval,
                lease_interval_secs,
                opts,
                init_system_params,
                init_session_config,
                shutdown,
            )
            .await
        }
    }
}

/// Bootstraps the follower or leader service based on the election status.
///
/// Returns when the `shutdown` token is triggered, or when leader status is lost, or if the leader
/// service fails to start.
pub async fn rpc_serve_with_store(
    meta_store_impl: MetaStoreImpl,
    election_client: ElectionClientRef,
    address_info: AddressInfo,
    max_cluster_heartbeat_interval: Duration,
    lease_interval_secs: u64,
    opts: MetaOpts,
    init_system_params: SystemParams,
    init_session_config: SessionConfig,
    shutdown: CancellationToken,
) -> MetaResult<()> {
    // TODO(shutdown): directly use cancellation token
    let (election_shutdown_tx, election_shutdown_rx) = watch::channel(());

    let election_handle = tokio::spawn({
        let shutdown = shutdown.clone();
        let election_client = election_client.clone();

        async move {
            while let Err(e) = election_client
                .run_once(lease_interval_secs as i64, election_shutdown_rx.clone())
                .await
            {
                tracing::error!(error = %e.as_report(), "election error happened");
            }
            // Leader lost, shutdown the service.
            shutdown.cancel();
        }
    });

    // Spawn and run the follower service if not the leader.
    // Watch the leader status and switch to the leader service when elected.
    // TODO: the branch seems to be always hit since the default value of `is_leader` is false until
    // the election is done (unless using `DummyElectionClient`).
    if !election_client.is_leader() {
        // The follower service can be shutdown separately if we're going to be the leader.
        let follower_shutdown = shutdown.child_token();

        let follower_handle = tokio::spawn(start_service_as_election_follower(
            follower_shutdown.clone(),
            address_info.clone(),
            election_client.clone(),
        ));

        // Watch and wait until we become the leader.
        let mut is_leader_watcher = election_client.subscribe();

        while !*is_leader_watcher.borrow_and_update() {
            tokio::select! {
                // External shutdown signal. Directly return without switching to leader.
                _ = shutdown.cancelled() => return Ok(()),

                res = is_leader_watcher.changed() => {
                    if res.is_err() {
                        tracing::error!("leader watcher recv failed");
                    }
                }
            }
        }

        tracing::info!("elected as leader, shutting down follower services");
        follower_shutdown.cancel();
        let _ = follower_handle.await;
    }

    // Run the leader service.
    let result = start_service_as_election_leader(
        meta_store_impl,
        address_info,
        max_cluster_heartbeat_interval,
        opts,
        init_system_params,
        init_session_config,
        election_client,
        shutdown,
    )
    .await;

    // Leader service has stopped, shutdown the election service to gracefully resign.
    election_shutdown_tx.send(()).ok();
    let _ = election_handle.await;

    result
}

/// Starts all services needed for the meta follower node.
///
/// Returns when the `shutdown` token is triggered.
pub async fn start_service_as_election_follower(
    shutdown: CancellationToken,
    address_info: AddressInfo,
    election_client: ElectionClientRef,
) {
    tracing::info!("starting follower services");

    let meta_member_srv = MetaMemberServiceImpl::new(election_client);

    let health_srv = HealthServiceImpl::new();

    let server = tonic::transport::Server::builder()
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
            shutdown.clone().cancelled_owned(),
        );
    let server_handle = tokio::spawn(server);
    started::set();

    // Wait for the shutdown signal.
    shutdown.cancelled().await;
    // Wait for the server to shutdown. This is necessary because we may be transitioning from follower
    // to leader, and conflicts on the services must be avoided.
    let _ = server_handle.await;
}

/// Starts all services needed for the meta leader node.
///
/// Returns when the `shutdown` token is triggered, or if the service initialization fails.
pub async fn start_service_as_election_leader(
    meta_store_impl: MetaStoreImpl,
    address_info: AddressInfo,
    max_cluster_heartbeat_interval: Duration,
    opts: MetaOpts,
    init_system_params: SystemParams,
    init_session_config: SessionConfig,
    election_client: ElectionClientRef,
    shutdown: CancellationToken,
) -> MetaResult<()> {
    tracing::info!("starting leader services");

    let env = MetaSrvEnv::new(
        opts.clone(),
        init_system_params,
        init_session_config,
        meta_store_impl,
    )
    .await?;
    let system_params_reader = env.system_params_reader().await;

    let data_directory = system_params_reader.data_directory();
    if !is_correct_data_directory(data_directory) {
        return Err(MetaError::system_params(format!(
            "The data directory {:?} is misconfigured.
            Please use a combination of uppercase and lowercase letters and numbers, i.e. [a-z, A-Z, 0-9].
            The string cannot start or end with '/', and consecutive '/' are not allowed.
            The data directory cannot be empty and its length should not exceed 800 characters.",
            data_directory
        )));
    }

    let metadata_manager = match env.meta_store() {
        MetaStoreImpl::Kv(_) => MetadataManager::new_v1(
            Arc::new(
                ClusterManager::new(env.clone(), max_cluster_heartbeat_interval)
                    .await
                    .unwrap(),
            ),
            Arc::new(CatalogManager::new(env.clone()).await.unwrap()),
            Arc::new(FragmentManager::new(env.clone()).await.unwrap()),
        ),
        MetaStoreImpl::Sql(_) => {
            let cluster_controller = Arc::new(
                ClusterController::new(env.clone(), max_cluster_heartbeat_interval)
                    .await
                    .unwrap(),
            );
            let catalog_controller = Arc::new(CatalogController::new(env.clone()).await?);
            MetadataManager::new_v2(cluster_controller, catalog_controller)
        }
    };

    let serving_vnode_mapping = Arc::new(ServingVnodeMapping::default());
    serving::on_meta_start(
        env.notification_manager_ref(),
        &metadata_manager,
        serving_vnode_mapping.clone(),
    )
    .await;

    let compactor_manager = Arc::new(
        hummock::CompactorManager::with_meta(env.clone())
            .await
            .unwrap(),
    );

    let heartbeat_srv = HeartbeatServiceImpl::new(metadata_manager.clone());

    let (compactor_streams_change_tx, compactor_streams_change_rx) =
        tokio::sync::mpsc::unbounded_channel();

    let meta_metrics = Arc::new(GLOBAL_META_METRICS.clone());

    let hummock_manager = hummock::HummockManager::new(
        env.clone(),
        metadata_manager.clone(),
        meta_metrics.clone(),
        compactor_manager.clone(),
        compactor_streams_change_tx,
    )
    .await
    .unwrap();
    let object_store_media_type = hummock_manager.object_store_media_type();

    let meta_member_srv = MetaMemberServiceImpl::new(election_client.clone());

    let prometheus_client = opts.prometheus_endpoint.as_ref().map(|x| {
        use std::str::FromStr;
        prometheus_http_query::Client::from_str(x).unwrap()
    });
    let prometheus_selector = opts.prometheus_selector.unwrap_or_default();
    let diagnose_command = Arc::new(risingwave_meta::manager::diagnose::DiagnoseCommand::new(
        metadata_manager.clone(),
        hummock_manager.clone(),
        env.event_log_manager_ref(),
        prometheus_client.clone(),
        prometheus_selector.clone(),
    ));

    let trace_state = otlp_embedded::State::new(otlp_embedded::Config {
        max_length: opts.cached_traces_num,
        max_memory_usage: opts.cached_traces_memory_limit_bytes,
    });
    let trace_srv = otlp_embedded::TraceServiceImpl::new(trace_state.clone());

    #[cfg(not(madsim))]
    let _dashboard_task = if let Some(ref dashboard_addr) = address_info.dashboard_addr {
        let dashboard_service = crate::dashboard::DashboardService {
            dashboard_addr: *dashboard_addr,
            prometheus_client,
            prometheus_selector,
            metadata_manager: metadata_manager.clone(),
            compute_clients: ComputeClientPool::new(1), // typically no need for plural clients
            diagnose_command,
            trace_state,
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

    // Initialize services.
    let backup_manager = BackupManager::new(
        env.clone(),
        hummock_manager.clone(),
        meta_metrics.clone(),
        system_params_reader.backup_storage_url(),
        system_params_reader.backup_storage_directory(),
    )
    .await?;

    LocalSecretManager::init(
        opts.temp_secret_file_dir,
        env.cluster_id().to_string(),
        META_NODE_ID,
    );

    let notification_srv = NotificationServiceImpl::new(
        env.clone(),
        metadata_manager.clone(),
        hummock_manager.clone(),
        backup_manager.clone(),
        serving_vnode_mapping.clone(),
    )
    .await?;

    let source_manager = Arc::new(
        SourceManager::new(
            barrier_scheduler.clone(),
            metadata_manager.clone(),
            meta_metrics.clone(),
        )
        .await
        .unwrap(),
    );

    let (sink_manager, shutdown_handle) = SinkCoordinatorManager::start_worker();
    // TODO(shutdown): remove this as there's no need to gracefully shutdown some of these sub-tasks.
    let mut sub_tasks = vec![shutdown_handle];

    let stream_rpc_manager = StreamRpcManager::new(env.clone());

    let scale_controller = Arc::new(ScaleController::new(
        &metadata_manager,
        source_manager.clone(),
        stream_rpc_manager.clone(),
        env.clone(),
    ));

    let barrier_manager = GlobalBarrierManager::new(
        scheduled_barriers,
        env.clone(),
        metadata_manager.clone(),
        hummock_manager.clone(),
        source_manager.clone(),
        sink_manager.clone(),
        meta_metrics.clone(),
        stream_rpc_manager.clone(),
        scale_controller.clone(),
    )
    .await;

    {
        let source_manager = source_manager.clone();
        tokio::spawn(async move {
            source_manager.run().await.unwrap();
        });
    }

    let stream_manager = Arc::new(
        GlobalStreamManager::new(
            env.clone(),
            metadata_manager.clone(),
            barrier_scheduler.clone(),
            source_manager.clone(),
            stream_rpc_manager,
            scale_controller.clone(),
        )
        .unwrap(),
    );

    hummock_manager
        .may_fill_backward_state_table_info()
        .await
        .unwrap();

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
        metadata_manager.clone(),
        stream_manager.clone(),
        source_manager.clone(),
        barrier_manager.context().clone(),
        sink_manager.clone(),
    )
    .await;

    let user_srv = UserServiceImpl::new(env.clone(), metadata_manager.clone());

    let scale_srv = ScaleServiceImpl::new(
        metadata_manager.clone(),
        source_manager,
        stream_manager.clone(),
        barrier_manager.context().clone(),
        scale_controller.clone(),
    );

    let cluster_srv =
        ClusterServiceImpl::new(metadata_manager.clone(), barrier_manager.context().clone());
    let stream_srv = StreamServiceImpl::new(
        env.clone(),
        barrier_scheduler.clone(),
        stream_manager.clone(),
        metadata_manager.clone(),
    );
    let sink_coordination_srv = SinkCoordinationServiceImpl::new(sink_manager);
    let hummock_srv = HummockServiceImpl::new(
        hummock_manager.clone(),
        vacuum_manager.clone(),
        metadata_manager.clone(),
    );

    let health_srv = HealthServiceImpl::new();
    let backup_srv = BackupServiceImpl::new(backup_manager);
    let telemetry_srv = TelemetryInfoServiceImpl::new(env.meta_store());
    let system_params_srv = SystemParamsServiceImpl::new(env.system_params_manager_impl_ref());
    let session_params_srv = SessionParamsServiceImpl::new(env.session_params_manager_impl_ref());
    let serving_srv =
        ServingServiceImpl::new(serving_vnode_mapping.clone(), metadata_manager.clone());
    let cloud_srv = CloudServiceImpl::new(metadata_manager.clone(), aws_cli);
    let event_log_srv = EventLogServiceImpl::new(env.event_log_manager_ref());

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
    sub_tasks.push(start_worker_info_monitor(
        metadata_manager.clone(),
        election_client.clone(),
        Duration::from_secs(env.opts.node_num_monitor_interval_sec),
        meta_metrics.clone(),
    ));
    sub_tasks.push(start_fragment_info_monitor(
        metadata_manager.clone(),
        hummock_manager.clone(),
        meta_metrics.clone(),
    ));
    match env.system_params_manager_impl_ref() {
        SystemParamsManagerImpl::Kv(mgr) => {
            sub_tasks.push(SystemParamsManager::start_params_notifier(mgr));
        }
        SystemParamsManagerImpl::Sql(mgr) => {
            sub_tasks.push(SystemParamsController::start_params_notifier(mgr));
        }
    }
    sub_tasks.push(HummockManager::hummock_timer_task(hummock_manager.clone()));
    sub_tasks.extend(HummockManager::compaction_event_loop(
        hummock_manager,
        compactor_streams_change_rx,
    ));
    sub_tasks.push(
        serving::start_serving_vnode_mapping_worker(
            env.notification_manager_ref(),
            metadata_manager.clone(),
            serving_vnode_mapping,
        )
        .await,
    );

    if cfg!(not(test)) {
        let task = match &metadata_manager {
            MetadataManager::V1(mgr) => ClusterManager::start_heartbeat_checker(
                mgr.cluster_manager.clone(),
                Duration::from_secs(1),
            ),
            MetadataManager::V2(mgr) => ClusterController::start_heartbeat_checker(
                mgr.cluster_controller.clone(),
                Duration::from_secs(1),
            ),
        };
        sub_tasks.push(task);
        sub_tasks.push(GlobalBarrierManager::start(barrier_manager));

        if !env.opts.disable_automatic_parallelism_control {
            sub_tasks.push(stream_manager.start_auto_parallelism_monitor());
        }
    }

    let _idle_checker_handle = IdleManager::start_idle_checker(
        env.idle_manager_ref(),
        Duration::from_secs(30),
        shutdown.clone(),
    );

    let (abort_sender, abort_recv) = tokio::sync::oneshot::channel();
    let notification_mgr = env.notification_manager_ref();
    let stream_abort_handler = tokio::spawn(async move {
        let _ = abort_recv.await;
        notification_mgr.abort_all().await;
        compactor_manager.abort_all_compactors();
    });
    sub_tasks.push((stream_abort_handler, abort_sender));

    let telemetry_manager = TelemetryManager::new(
        Arc::new(MetaTelemetryInfoFetcher::new(env.cluster_id().clone())),
        Arc::new(MetaReportCreator::new(
            metadata_manager.clone(),
            env.meta_store().backend(),
            object_store_media_type,
        )),
    );

    // May start telemetry reporting
    if env.opts.telemetry_enabled && telemetry_env_enabled() {
        sub_tasks.push(telemetry_manager.start().await);
    } else {
        tracing::info!("Telemetry didn't start due to meta backend or config");
    }
    if report_scarf_enabled() {
        tokio::spawn(report_to_scarf());
    } else {
        tracing::info!("Scarf reporting is disabled");
    };

    if let Some(pair) = env.event_log_manager_ref().take_join_handle() {
        sub_tasks.push(pair);
    }

    tracing::info!("Assigned cluster id {:?}", *env.cluster_id());
    tracing::info!("Starting meta services");

    let event = risingwave_pb::meta::event_log::EventMetaNodeStart {
        advertise_addr: address_info.advertise_addr,
        listen_addr: address_info.listen_addr.to_string(),
        opts: serde_json::to_string(&env.opts).unwrap(),
    };
    env.event_log_manager_ref().add_event_logs(vec![
        risingwave_pb::meta::event_log::Event::MetaNodeStart(event),
    ]);

    let server_builder = tonic::transport::Server::builder()
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
        .add_service(SessionParamServiceServer::new(session_params_srv))
        .add_service(TelemetryInfoServiceServer::new(telemetry_srv))
        .add_service(ServingServiceServer::new(serving_srv))
        .add_service(CloudServiceServer::new(cloud_srv))
        .add_service(SinkCoordinationServiceServer::new(sink_coordination_srv))
        .add_service(EventLogServiceServer::new(event_log_srv));
    #[cfg(not(madsim))] // `otlp-embedded` does not use madsim-patched tonic
    let server_builder = server_builder.add_service(TraceServiceServer::new(trace_srv));

    let server = server_builder.monitored_serve_with_shutdown(
        address_info.listen_addr,
        "grpc-meta-leader-service",
        TcpConfig {
            tcp_nodelay: true,
            keepalive_duration: None,
        },
        shutdown.clone().cancelled_owned(),
    );
    started::set();
    let _server_handle = tokio::spawn(server);

    // Wait for the shutdown signal.
    shutdown.cancelled().await;
    // TODO(shutdown): may warn user if there's any other node still running in the cluster.
    // TODO(shutdown): do we have any other shutdown tasks?
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
