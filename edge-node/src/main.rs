use axum::{
    body::{Body, Bytes},
    extract::{Path, State},
    http::{HeaderMap, Request, StatusCode},
    middleware::{self, Next},
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Response,
    },
    routing::{delete, get, post},
    Json, Router,
};
use charging_space_edge::{
    evaluate, CameraChannel, CameraState, ChargerAdapter, ChargerSnapshot, ChargerStatus,
    ConfigurationStore, Decision, DynamicChargerAdapter, EvaluationInput, EventLifecycleRegistry,
    EvidenceStorage, EvidenceWatermark, ExternalChargerUpdate, HardwareHealthMonitor,
    HardwareModbusRtuRelayController, HardwareRunMode, HardwareTransport, InferenceEngine,
    MemoryRelayController, MetricsCollector, MockTransport, ModbusTcpRelayController,
    OperatorApiConfig, OperatorCloudClient, PendingEvidenceUpload, PlatformConfiguration,
    PresencePhase, RawDetection, RecognitionRecord, RelayChannelStatus, RelayController,
    RemoteUpgradeManager, RuleConfig, SerialPortConfig, SerialPortTransport, SpacePresenceTracker,
    SpaceRuntimeStatus, SpaceStatusRegistry, SpaceTrackerRegistry, SqliteStorage,
    TcpRelayTransport, TelemetrySnapshot, TransitionEvent, UpgradeManifest, UpgradeReport,
    VideoChannelManager, VideoFrame, WatchdogMonitor, WhitelistRegistry,
};
use chrono::Utc;
use futures_core::Stream;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    convert::Infallible,
    path::PathBuf,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    task::{Context, Poll},
    time::Duration,
};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeSystemConfig {
    pub node_id: String,
    pub cloud_url: Option<String>,
    pub edge_api_key: Option<String>,
    pub local_api_key: Option<String>,
    #[serde(default)]
    pub lan_ip: Option<String>,
    #[serde(default)]
    pub lan_subnet: Option<String>,
    #[serde(default)]
    pub lan_gateway: Option<String>,
    #[serde(default)]
    pub lan_dns: Option<String>,
}

#[derive(Clone)]
struct AppState {
    configuration: Arc<tokio::sync::RwLock<PlatformConfiguration>>,
    relay: Arc<dyn RelayController>,
    system_config: Arc<std::sync::RwLock<NodeSystemConfig>>,
    data_directory: PathBuf,
    node_id: String,
    http: reqwest::Client,
    upload_lock: Arc<tokio::sync::Mutex<()>>,
    configuration_apply_lock: Arc<tokio::sync::Mutex<()>>,
    configuration_store: Arc<ConfigurationStore>,
    cameras: Arc<VideoChannelManager>,
    trackers: Arc<SpaceTrackerRegistry>,
    chargers: Arc<DynamicChargerAdapter>,
    sqlite: Arc<SqliteStorage>,
    watchdog: Arc<WatchdogMonitor>,
    hardware_monitor: Arc<HardwareHealthMonitor>,
    whitelist: Arc<WhitelistRegistry>,
    lifecycle: Arc<EventLifecycleRegistry>,
    space_statuses: Arc<SpaceStatusRegistry>,
    last_configuration_report: Arc<tokio::sync::RwLock<Option<ConfigurationApplyReport>>>,
    metrics: Arc<MetricsCollector>,
    upgrade_manager: Arc<RemoteUpgradeManager>,
    evidence_storage: Arc<EvidenceStorage>,
    /// 已启动的解码器/接收器标识（防止重复启动）
    decoder_active_keys: Arc<tokio::sync::RwLock<std::collections::HashSet<String>>>,
    cloud_connected: Arc<AtomicBool>,
    vision_mode: String,
    ai_engine: String,
    last_snapshot_upload: Arc<tokio::sync::Mutex<HashMap<String, std::time::Instant>>>,
}

impl AppState {
    pub fn node_id(&self) -> String {
        self.system_config
            .read()
            .ok()
            .map(|c| c.node_id.clone())
            .filter(|id| !id.trim().is_empty())
            .unwrap_or_else(|| self.node_id.clone())
    }
    pub fn cloud_url(&self) -> Option<String> {
        self.system_config.read().ok().and_then(|c| c.cloud_url.clone())
    }
    pub fn edge_api_key(&self) -> Option<String> {
        self.system_config.read().ok().and_then(|c| c.edge_api_key.clone())
    }
    pub fn local_api_key(&self) -> Option<String> {
        self.system_config.read().ok().and_then(|c| c.local_api_key.clone())
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Health {
    status: &'static str,
    max_video_channels: u8,
    node_id: String,
    cloud_url: Option<String>,
    cloud_configured: bool,
    cloud_connected: bool,
    vision_mode: String,
    ai_engine: String,
    version: u64,
    pending_events: usize,
    configuration_version: u64,
    configured_spaces: usize,
    camera_channels: usize,
    charger_channels: usize,
    watchdog_healthy: bool,
    hardware_status: String,
    active_alarms: usize,
    occupied_spaces: usize,
    charging_spaces: usize,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConfigurationApplyReport {
    node_id: String,
    version: u64,
    status: String,
    message: String,
    reported_at: chrono::DateTime<Utc>,
}

#[tokio::main]
async fn main() {
    let data_directory = std::env::var("EDGE_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("data"));
    let _ = std::fs::create_dir_all(data_directory.join("snapshots"));
    let sqlite_storage =
        SqliteStorage::open(data_directory.join("edge.db")).expect("open sqlite storage");
    let migrated = sqlite_storage
        .migrate_legacy_event_directory(data_directory.join("events"))
        .expect("migrate legacy event spool");
    if migrated > 0 {
        println!("migrated {} legacy event files into sqlite", migrated);
    }
    let (configuration_store, file_configuration) =
        ConfigurationStore::open(data_directory.join("configuration.json"))
            .await
            .expect("open rule configuration");
    let initial_configuration = match sqlite_storage.load_platform_config() {
        Ok(Some(db_cfg)) if db_cfg.version >= file_configuration.version => db_cfg,
        _ => {
            let _ = sqlite_storage.save_platform_config(&file_configuration);
            file_configuration
        }
    };

    let hardware_mode = HardwareRunMode::from_env();
    let (transport, relay_controller): (Arc<dyn HardwareTransport>, Arc<dyn RelayController>) =
        match hardware_mode {
            HardwareRunMode::ModbusTcp => {
                let addr_str = std::env::var("EDGE_MODBUS_TCP_ADDR")
                    .unwrap_or_else(|_| "127.0.0.1:502".into());
                match addr_str.parse::<std::net::SocketAddr>() {
                    Ok(addr) => {
                        println!("initializing Modbus TCP relay at {}", addr);
                        let tcp_trans = Arc::new(TcpRelayTransport::new(addr));
                        let controller = ModbusTcpRelayController::new(addr, 1);
                        for binding in &initial_configuration.bindings {
                            if binding.enabled {
                                let _ = controller
                                    .bind_space_channel(&binding.space_id, binding.relay_channel);
                            }
                        }
                        (tcp_trans, Arc::new(controller))
                    }
                    Err(e) => {
                        eprintln!(
                            "invalid EDGE_MODBUS_TCP_ADDR {}: {}, falling back to memory relay",
                            addr_str, e
                        );
                        let mock = Arc::new(MockTransport::new("EDGE_MOCK_BUS", "MOCK"));
                        let mem = MemoryRelayController::new();
                        for binding in &initial_configuration.bindings {
                            if binding.enabled {
                                let _ =
                                    mem.register_space(&binding.space_id, binding.relay_channel);
                            }
                        }
                        (mock, Arc::new(mem))
                    }
                }
            }
            HardwareRunMode::ModbusRtu => {
                let port_name = std::env::var("EDGE_MODBUS_RTU_PORT").unwrap_or_else(|_| {
                    if cfg!(windows) {
                        "COM3".into()
                    } else {
                        "/dev/ttyUSB0".into()
                    }
                });
                let cfg = SerialPortConfig {
                    port_name,
                    baud_rate: 9600,
                    data_bits: 8,
                    stop_bits: 1,
                    parity: "None".into(),
                };
                let serial_trans = Arc::new(SerialPortTransport::new(cfg));
                let slave_id: u8 = std::env::var("EDGE_MODBUS_SLAVE_ID")
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0x01);
                let controller =
                    HardwareModbusRtuRelayController::new(serial_trans.clone(), slave_id);
                for binding in &initial_configuration.bindings {
                    if binding.enabled {
                        let _ =
                            controller.bind_space_channel(&binding.space_id, binding.relay_channel);
                    }
                }
                (serial_trans, Arc::new(controller))
            }
            HardwareRunMode::Mock => {
                let mock = Arc::new(MockTransport::new("EDGE_MOCK_BUS", "MOCK"));
                let mem = MemoryRelayController::new();
                for binding in &initial_configuration.bindings {
                    if binding.enabled {
                        let _ = mem.register_space(&binding.space_id, binding.relay_channel);
                    }
                }
                (mock, Arc::new(mem))
            }
        };

    let chargers = Arc::new(DynamicChargerAdapter::default());
    chargers
        .apply_bindings(&initial_configuration.bindings)
        .expect("apply initial charger bindings");
    if let Ok(restored_chargers) = sqlite_storage.load_charger_snapshots() {
        let _ = chargers.restore_snapshots(restored_chargers);
    }
    let whitelist = Arc::new(WhitelistRegistry::default());
    whitelist
        .apply(&initial_configuration.whitelist)
        .expect("apply initial whitelist");
    let lifecycle = Arc::new(EventLifecycleRegistry::default());
    lifecycle
        .restore(sqlite_storage.active_events().expect("load active events"))
        .expect("restore active events");
    let node_id = std::env::var("EDGE_NODE_ID").unwrap_or_else(|_| "edge-local-01".into());
    let space_statuses = Arc::new(SpaceStatusRegistry::default());
    space_statuses
        .apply_bindings(&node_id, &initial_configuration.bindings)
        .expect("apply initial space statuses");
    // 恢复历史状态时，严格仅恢复当前有效绑定配置中存在的车位，防止未配置或已被云端删除的车位残留在边缘节点
    let active_space_ids: Vec<String> = initial_configuration
        .bindings
        .iter()
        .filter(|b| b.enabled)
        .map(|b| b.space_id.clone())
        .collect();
    let _ = sqlite_storage.prune_space_statuses(&active_space_ids);
    if let Ok(restored_statuses) = sqlite_storage.load_space_statuses() {
        let valid_restored: Vec<_> = restored_statuses
            .into_iter()
            .filter(|st| active_space_ids.contains(&st.space_id))
            .collect();
        let _ = space_statuses.restore(valid_restored);
    }

    let hardware_monitor = Arc::new(HardwareHealthMonitor::new(transport.clone()));
    if let Err(err) = hardware_monitor.verify_ready() {
        eprintln!("[FATAL] 生产硬件就绪校验失败: {}", err);
        std::process::exit(1);
    }
    let watchdog = Arc::new(WatchdogMonitor::default());
    let _wd_handle = watchdog.clone().start_loop();

    let evidence_storage = Arc::new(EvidenceStorage::new(data_directory.join("evidence")));
    let metrics = Arc::new(MetricsCollector::new(
        node_id.clone(),
        data_directory.to_str().unwrap_or("data").to_string(),
        std::env::var("EDGE_DISK_CAPACITY_BYTES")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(10 * 1024 * 1024 * 1024),
    ));
    let upgrade_secret = std::env::var("EDGE_UPGRADE_SECRET").unwrap_or_default();
    let current_exe = std::env::var("EDGE_BINARY_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("charging-space-edge"));
            // 如果处于容器只读根文件系统环境下且父目录不可写，将固件部署目标迁移至持久化数据目录下的 bin 子目录
            let is_parent_writable = exe.parent().map(|p| {
                let test_probe = p.join(".write_test");
                if std::fs::write(&test_probe, b"").is_ok() {
                    let _ = std::fs::remove_file(test_probe);
                    true
                } else {
                    false
                }
            }).unwrap_or(false);

            if !is_parent_writable {
                let persistent_bin_dir = data_directory.join("bin");
                let _ = std::fs::create_dir_all(&persistent_bin_dir);
                let target_bin = persistent_bin_dir.join("charging-space-edge");
                if !target_bin.exists() && exe.exists() {
                    let _ = std::fs::copy(&exe, &target_bin);
                }
                target_bin
            } else {
                exe
            }
        });
    let initial_version: u64 = {
        let version_file = current_exe.with_extension("version");
        let alt_version_file = data_directory.join("version");
        std::fs::read_to_string(&version_file)
            .ok()
            .or_else(|| std::fs::read_to_string(&alt_version_file).ok())
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(1)
    };
    let upgrade_manager = Arc::new(RemoteUpgradeManager::new(initial_version, upgrade_secret, current_exe));

    let env_default_sys_cfg = || NodeSystemConfig {
        node_id: node_id.clone(),
        cloud_url: std::env::var("EDGE_CLOUD_URL").ok().filter(|v| !v.is_empty()),
        edge_api_key: std::env::var("EDGE_API_KEY").ok().filter(|v| !v.is_empty()),
        local_api_key: std::env::var("EDGE_LOCAL_API_KEY").ok().filter(|v| !v.is_empty()),
        lan_ip: std::env::var("EDGE_LAN_IP").ok().filter(|v| !v.is_empty()),
        lan_subnet: None,
        lan_gateway: std::env::var("EDGE_LAN_GATEWAY").ok().filter(|v| !v.is_empty()),
        lan_dns: None,
    };
    let system_config_file = data_directory.join("node_system_config.json");
    let mut initial_system_config = sqlite_storage
        .load_system_config_json()
        .ok()
        .flatten()
        .and_then(|json| serde_json::from_str::<NodeSystemConfig>(&json).ok())
        .or_else(|| {
            std::fs::read_to_string(&system_config_file)
                .ok()
                .and_then(|content| serde_json::from_str::<NodeSystemConfig>(&content).ok())
        })
        .unwrap_or_else(env_default_sys_cfg);

    if let Ok(v) = std::env::var("EDGE_NODE_ID") {
        if !v.trim().is_empty() {
            initial_system_config.node_id = v.trim().to_string();
        }
    }
    if let Ok(v) = std::env::var("EDGE_CLOUD_URL") {
        if !v.trim().is_empty() {
            initial_system_config.cloud_url = Some(v.trim().to_string());
        }
    }
    if let Ok(v) = std::env::var("EDGE_API_KEY") {
        if !v.trim().is_empty() {
            initial_system_config.edge_api_key = Some(v.trim().to_string());
        }
    }
    if let Ok(v) = std::env::var("EDGE_LOCAL_API_KEY") {
        if !v.trim().is_empty() {
            initial_system_config.local_api_key = Some(v.trim().to_string());
        }
    }
    if let Ok(v) = std::env::var("EDGE_LAN_IP") {
        if !v.trim().is_empty() {
            initial_system_config.lan_ip = Some(v.trim().to_string());
        }
    }
    if let Ok(json_str) = serde_json::to_string_pretty(&initial_system_config) {
        let _ = sqlite_storage.save_system_config_json(&json_str);
    }

    let vision_mode = std::env::var("EDGE_VISION_MODE")
        .unwrap_or_else(|_| "external".into())
        .to_lowercase();
    let ai_engine = if vision_mode == "external" {
        "RTSP-Vision-Sidecar".to_string()
    } else {
        std::env::var("EDGE_AI_ENGINE").unwrap_or_else(|_| "auto".into())
    };

    let state = AppState {
        configuration: Arc::new(tokio::sync::RwLock::new(initial_configuration)),
        relay: relay_controller,
        system_config: Arc::new(std::sync::RwLock::new(initial_system_config)),
        data_directory: data_directory.clone(),
        node_id,
        http: reqwest::Client::new(),
        upload_lock: Arc::new(tokio::sync::Mutex::new(())),
        configuration_apply_lock: Arc::new(tokio::sync::Mutex::new(())),
        configuration_store: Arc::new(configuration_store),
        cameras: Arc::new(VideoChannelManager::new(16)),
        trackers: Arc::new(SpaceTrackerRegistry::new()),
        chargers: chargers.clone(),
        sqlite: Arc::new(sqlite_storage),
        watchdog: watchdog.clone(),
        hardware_monitor: hardware_monitor.clone(),
        whitelist,
        lifecycle,
        space_statuses,
        last_configuration_report: Arc::new(tokio::sync::RwLock::new(None)),
        metrics: metrics.clone(),
        upgrade_manager: upgrade_manager.clone(),
        evidence_storage: evidence_storage.clone(),
        decoder_active_keys: Arc::new(tokio::sync::RwLock::new(std::collections::HashSet::new())),
        cloud_connected: Arc::new(AtomicBool::new(false)),
        vision_mode,
        ai_engine,
        last_snapshot_upload: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
    };
    let heartbeat_state = state.clone();
    tokio::spawn(async move {
        heartbeat_loop(heartbeat_state).await;
    });
    let upgrade_state = state.clone();
    tokio::spawn(async move {
        upgrade_sync_loop(upgrade_state).await;
    });
    let upload_state = state.clone();
    tokio::spawn(async move {
        upload_loop(upload_state).await;
    });
    let config_state = state.clone();
    tokio::spawn(async move {
        configuration_sync_loop(config_state).await;
    });
    let status_state = state.clone();
    tokio::spawn(async move {
        space_status_upload_loop(status_state).await;
    });
    let alarm_protect_state = state.clone();
    tokio::spawn(async move {
        alarm_protection_loop(alarm_protect_state).await;
    });
    let charger_scan_state = state.clone();
    tokio::spawn(async move {
        charger_offline_detection_loop(charger_scan_state).await;
    });
    let storage_clean_state = state.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(300));
        loop {
            interval.tick().await;
            let _ = storage_clean_state.evidence_storage.cleanup_local_quota();
        }
    });
    if let Some(op_config) = OperatorApiConfig::from_env() {
        let op_client = Arc::new(OperatorCloudClient::new(op_config));
        let op_chargers = chargers.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(15));
            loop {
                interval.tick().await;
                if let Err(e) = op_client.sync_to_adapter(&op_chargers).await {
                    eprintln!("operator cloud sync error: {}", e);
                }
            }
        });
    }

    // 启动 GB28181 SIP 监听服务 (默认 5060 端口，可通过 EDGE_GB28181_SIP_PORT 配置)
    let sip_port: u16 = std::env::var("EDGE_GB28181_SIP_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(5060);
    let sip_realm = std::env::var("EDGE_GB28181_REALM").unwrap_or_else(|_| "3402000000".into());
    let sip_server_id =
        std::env::var("EDGE_GB28181_SERVER_ID").unwrap_or_else(|_| "34020000002000000001".into());
    let sip_password = std::env::var("EDGE_GB28181_PASSWORD").unwrap_or_default();
    let sip_ip = std::env::var("EDGE_GB28181_SIP_IP").unwrap_or_else(|_| "0.0.0.0".into());

    let gb_registry = Arc::new(charging_space_edge::gb28181::Gb28181DeviceRegistry::new(
        &sip_server_id,
        &sip_ip,
        sip_port,
        &sip_realm,
        &sip_password,
    ));

    let sip_bind_addr = format!("0.0.0.0:{}", sip_port);
    let gb_registry_for_sip = gb_registry.clone();
    tokio::spawn(async move {
        match charging_space_edge::gb28181::Gb28181SipServer::bind(&sip_bind_addr, (*gb_registry_for_sip).clone())
            .await
        {
            Ok(server) => {
                println!("GB28181 SIP service listening on {}", sip_bind_addr);
                let _ = server.run().await;
            }
            Err(e) => {
                eprintln!("GB28181 SIP service notice ({}): {}", sip_bind_addr, e);
            }
        }
    });

    // 创建共享的 FrameGrabber（RTSP/GB28181 管道与 AI 推理管道共享）
    let shared_frame_grabber = Arc::new(charging_space_edge::pipeline::FrameGrabber::new());

    // 启动 GB28181 RTP 流接收器（接收视频流并解码）
    let gb_state = state.clone();
    let gb_registry_for_stream = gb_registry.clone();
    let gb_grabber = shared_frame_grabber.clone();
    tokio::spawn(async move {
        start_gb28181_stream_receivers(gb_state, gb_grabber, gb_registry_for_stream).await;
    });

    // 启动 RTSP 拉流客户端（为配置了 RTSP URL 的摄像头通道）
    let rtsp_state = state.clone();
    let rtsp_grabber = shared_frame_grabber.clone();
    tokio::spawn(async move {
        start_rtsp_clients(rtsp_state, rtsp_grabber).await;
    });

    // 启动 1~16 路视频流主动拉流与 AI 视觉分析主循环任务
    let stream_pipeline_state = state.clone();
    let pipeline_grabber = shared_frame_grabber.clone();
    tokio::spawn(async move {
        video_stream_pipeline_loop(stream_pipeline_state, pipeline_grabber).await;
    });

    let initial_configuration = state.configuration.read().await.clone();
    state
        .cameras
        .apply_bindings(&initial_configuration.bindings)
        .expect("apply initial camera bindings");
    if let Ok(restored_cams) = state.sqlite.load_cameras() {
        let _ = state.cameras.restore(restored_cams);
    }
    let _ = state
        .cameras
        .apply_configuration_cameras(&initial_configuration.cameras);

    let protected = Router::new()
        .route("/api/v1/rules", get(get_rules).put(put_rules))
        .route(
            "/api/v1/configuration",
            get(get_configuration).put(put_configuration),
        )
        .route("/api/v1/vision/targets", get(get_vision_targets))
        .route("/api/v1/cameras", get(camera_states).post(register_camera))
        .route("/api/v1/cameras/:camera_id", delete(delete_camera))
        .route(
            "/api/v1/cameras/:camera_id/heartbeat",
            post(camera_heartbeat),
        )
        .route("/api/v1/chargers", get(list_chargers))
        .route("/api/v1/chargers/snapshots", post(update_charger_snapshot))
        .route("/api/v1/spaces/:space_id/evaluate", post(evaluate_space))
        .route(
            "/api/v1/spaces/:space_id/detections",
            post(handle_detection),
        )
        .route("/api/v1/spaces/:space_id/tracker", get(get_space_tracker))
        .route("/api/v1/spaces/:space_id/silence", post(silence_space))
        .route(
            "/api/v1/spaces/:space_id/snapshot",
            post(upload_space_snapshot),
        )
        .route(
            "/api/v1/events/:event_id/acknowledge",
            post(acknowledge_event_endpoint),
        )
        .route("/api/v1/spaces/status", get(get_space_statuses))
        .route("/api/v1/stream/spaces", get(stream_spaces))
        .route("/api/v1/stream/events", get(stream_events))
        .route("/api/v1/relays", get(relay_states))
        .route("/api/v1/relays/details", get(relay_details))
        .route("/api/v1/metrics", get(get_metrics))
        .route("/api/v1/telemetry", get(get_telemetry))
        .route("/api/v1/upgrade/report", get(get_upgrade_report))
        .route("/api/v1/upgrade/apply", post(post_upgrade_apply))
        .route(
            "/api/v1/system/config",
            get(get_system_config).post(post_system_config),
        )
        .route("/api/v1/system/network", get(get_system_network))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            require_local_api_key,
        ));
    let app = Router::new()
        .route("/health", get(health))
        .route("/", get(admin_ui))
        .route("/index.html", get(admin_ui))
        .route("/admin", get(admin_ui))
        .route("/admin.html", get(admin_ui))
        .route("/vendor/app.css", get(vendor_css))
        .route("/vendor/vue.global.prod.js", get(vendor_vue))
        .route("/api/v1/spaces/:space_id/snapshot", get(space_snapshot))
        .route("/api/v1/evidences/:filename", get(get_evidence_file))
        .merge(protected)
        .layer(axum::extract::DefaultBodyLimit::max(10 * 1024 * 1024))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("0.0.0.0:8081").await.unwrap();
    println!("edge node listening on {}", listener.local_addr().unwrap());
    axum::serve(listener, app).await.unwrap();
}
async fn admin_ui() -> axum::response::Html<String> {
    if let Ok(content) = tokio::fs::read_to_string("/data/edge/static/admin.html").await {
        return axum::response::Html(content);
    }
    axum::response::Html(include_str!("../static/admin.html").to_string())
}

async fn vendor_css() -> impl IntoResponse {
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/css; charset=utf-8",
        )],
        include_str!("../static/vendor/app.css"),
    )
}

async fn vendor_vue() -> impl IntoResponse {
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "application/javascript; charset=utf-8",
        )],
        include_str!("../static/vendor/vue.global.prod.js"),
    )
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct NetworkStatus {
    interfaces: Vec<NetworkInterfaceInfo>,
    primary_ip: String,
    configured_ip: Option<String>,
    subnet: Option<String>,
    gateway: Option<String>,
    dns: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct NetworkInterfaceInfo {
    name: String,
    ip: String,
    mac: String,
    status: String,
    speed: String,
}

async fn get_system_config(State(s): State<AppState>) -> Json<NodeSystemConfig> {
    Json(s.system_config.read().unwrap().clone())
}

async fn post_system_config(
    State(s): State<AppState>,
    Json(new_config): Json<NodeSystemConfig>,
) -> Result<Json<NodeSystemConfig>, (StatusCode, String)> {
    let config_file = s.data_directory.join("node_system_config.json");
    if let Ok(json_str) = serde_json::to_string_pretty(&new_config) {
        let _ = tokio::fs::write(&config_file, json_str.as_bytes()).await;
        let _ = s.sqlite.save_system_config_json(&json_str);
    }
    *s.system_config.write().unwrap() = new_config.clone();
    println!(
        "system config updated: node_id={}, cloud_url={:?}, lan_ip={:?}",
        new_config.node_id, new_config.cloud_url, new_config.lan_ip
    );
    Ok(Json(new_config))
}

#[cfg(target_os = "linux")]
fn collect_linux_interfaces() -> Vec<NetworkInterfaceInfo> {
    use std::collections::HashMap;
    use std::ffi::CStr;

    let mut ipv4_map: HashMap<String, String> = HashMap::new();
    unsafe {
        let mut ifap: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut ifap) == 0 && !ifap.is_null() {
            let mut cur = ifap;
            while !cur.is_null() {
                let ifa = &*cur;
                if !ifa.ifa_name.is_null() && !ifa.ifa_addr.is_null() {
                    let family = (*ifa.ifa_addr).sa_family as i32;
                    if family == libc::AF_INET {
                        let name = CStr::from_ptr(ifa.ifa_name).to_string_lossy().into_owned();
                        let sin = &*(ifa.ifa_addr as *const libc::sockaddr_in);
                        let ip = std::net::Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr));
                        ipv4_map.entry(name).or_insert_with(|| ip.to_string());
                    }
                }
                cur = ifa.ifa_next;
            }
            libc::freeifaddrs(ifap);
        }
    }

    let mut interfaces = Vec::new();
    if let Ok(entries) = std::fs::read_dir("/sys/class/net") {
        let mut names: Vec<String> = entries
            .flatten()
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| n != "lo")
            .collect();
        names.sort();
        for name in names {
            let base = std::path::Path::new("/sys/class/net").join(&name);
            let mac = std::fs::read_to_string(base.join("address"))
                .map(|s| s.trim().to_uppercase())
                .unwrap_or_default();
            let operstate = std::fs::read_to_string(base.join("operstate"))
                .map(|s| s.trim().to_uppercase())
                .unwrap_or_else(|_| "UNKNOWN".into());
            let speed = std::fs::read_to_string(base.join("speed"))
                .ok()
                .and_then(|s| s.trim().parse::<i64>().ok())
                .filter(|&sp| sp > 0)
                .map(|sp| format!("{} Mbps", sp))
                .unwrap_or_default();
            let ip = ipv4_map.get(&name).cloned().unwrap_or_default();
            interfaces.push(NetworkInterfaceInfo {
                name,
                ip,
                mac,
                status: operstate,
                speed,
            });
        }
    }
    interfaces
}

fn resolve_primary_ip(s: &AppState) -> String {
    let cfg = s.system_config.read().unwrap().clone();
    if let Some(ip) = cfg.lan_ip.filter(|ip| !ip.trim().is_empty()) {
        return ip;
    }

    let detected_ip = std::net::UdpSocket::bind("0.0.0.0:0")
        .and_then(|sock| {
            sock.connect("8.8.8.8:80")?;
            sock.local_addr()
        })
        .map(|addr| addr.ip().to_string())
        .unwrap_or_else(|_| "127.0.0.1".into());

    #[cfg(target_os = "linux")]
    {
        let interfaces = collect_linux_interfaces();
        if let Some(ip) = interfaces
            .into_iter()
            .find(|i| !i.ip.is_empty() && i.status == "UP")
            .map(|i| i.ip)
        {
            return ip;
        }
    }

    detected_ip
}

async fn get_system_network(State(s): State<AppState>) -> Json<NetworkStatus> {
    let cfg = s.system_config.read().unwrap().clone();

    #[cfg(target_os = "linux")]
    let interfaces = collect_linux_interfaces();

    #[cfg(not(target_os = "linux"))]
    let interfaces = vec![NetworkInterfaceInfo {
        name: "local0".into(),
        ip: resolve_primary_ip(&s),
        mac: String::new(),
        status: "UP".into(),
        speed: String::new(),
    }];

    let primary_ip = resolve_primary_ip(&s);

    Json(NetworkStatus {
        interfaces,
        primary_ip,
        configured_ip: cfg.lan_ip,
        subnet: cfg.lan_subnet,
        gateway: cfg.lan_gateway,
        dns: cfg.lan_dns,
    })
}

fn sanitize_path_segment(raw: &str) -> String {
    raw.trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn is_valid_jpeg(bytes: &[u8]) -> bool {
    bytes.len() >= 4 && bytes[0] == 0xFF && bytes[1] == 0xD8
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct VisionTarget {
    site_id: String,
    space_id: String,
    camera_id: String,
    protocol: String,
    stream_url: Option<String>,
    roi_json: Option<String>,
    enabled: bool,
}

async fn get_vision_targets(
    State(s): State<AppState>,
) -> Result<Json<Vec<VisionTarget>>, (StatusCode, String)> {
    let config = s.configuration.read().await.clone();
    let cameras = s
        .cameras
        .snapshot()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;

    let mut targets = Vec::new();
    for binding in config.bindings.iter().filter(|b| b.enabled) {
        let cam_opt = cameras.iter().find(|c| c.camera_id == binding.camera_id);
        let protocol = cam_opt
            .and_then(|c| c.protocol.clone())
            .unwrap_or_else(|| "RTSP".into());
        let stream_url = cam_opt
            .and_then(|c| c.stream_url.clone())
            .filter(|u| !u.trim().is_empty());
        let roi_json = cam_opt
            .and_then(|c| c.roi_json.clone())
            .filter(|r| !r.trim().is_empty());
        targets.push(VisionTarget {
            site_id: binding.site_id.clone(),
            space_id: binding.space_id.clone(),
            camera_id: binding.camera_id.clone(),
            protocol,
            stream_url,
            roi_json,
            enabled: binding.enabled,
        });
    }
    for cam in cameras {
        if !targets.iter().any(|t| t.camera_id == cam.camera_id) {
            if let Some(url) = cam.stream_url.clone().filter(|u| !u.trim().is_empty()) {
                targets.push(VisionTarget {
                    site_id: "default".into(),
                    space_id: cam.space_id.clone(),
                    camera_id: cam.camera_id.clone(),
                    protocol: cam.protocol.clone().unwrap_or_else(|| "RTSP".into()),
                    stream_url: Some(url),
                    roi_json: cam.roi_json.clone().filter(|r| !r.trim().is_empty()),
                    enabled: true,
                });
            }
        }
    }
    Ok(Json(targets))
}

async fn upload_space_snapshot(
    Path(space_id): Path<String>,
    State(s): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    if !is_valid_jpeg(&body) {
        return Err((
            StatusCode::BAD_REQUEST,
            "snapshot body must be valid JPEG bytes".into(),
        ));
    }
    if body.len() > 8 * 1024 * 1024 {
        return Err((
            StatusCode::PAYLOAD_TOO_LARGE,
            "snapshot exceeds 8MB limit".into(),
        ));
    }
    let snapshots_dir = s.data_directory.join("snapshots");
    let _ = tokio::fs::create_dir_all(&snapshots_dir).await;
    let safe_space = sanitize_path_segment(&space_id);
    let target = snapshots_dir.join(format!("{}.jpg", safe_space));
    let tmp = snapshots_dir.join(format!("{}.jpg.tmp", safe_space));
    tokio::fs::write(&tmp, &body)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    tokio::fs::rename(&tmp, &target)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    // 更新关联摄像头在线心跳
    if let Ok(cams) = s.cameras.snapshot() {
        for cam in cams.into_iter().filter(|c| c.space_id == space_id) {
            if let Ok(updated) = s.cameras.heartbeat(&cam.camera_id, Utc::now()) {
                let _ = s.sqlite.upsert_camera(&updated);
            }
        }
    }

    // 节流上报最新实景抓拍至云端（每车位 5 秒一次，或携带 X-Force-Cloud-Sync: true 立即上报）
    let force_sync = headers
        .get("X-Force-Cloud-Sync")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("true") || v == "1");
    if let Some(base) = s.cloud_url() {
        let should_upload = {
            let mut map = s.last_snapshot_upload.lock().await;
            let now = std::time::Instant::now();
            match map.get(&space_id) {
                Some(&last) if !force_sync && now.duration_since(last).as_secs() < 5 => false,
                _ => {
                    map.insert(space_id.clone(), now);
                    true
                }
            }
        };
        if should_upload {
            let s_clone = s.clone();
            let space_clone = space_id.clone();
            let bytes_vec = body.to_vec();
            tokio::spawn(async move {
                let node_id = s_clone.node_id();
                let url = format!(
                    "{}/api/v1/nodes/{}/spaces/{}/snapshot",
                    base.trim_end_matches('/'),
                    node_id,
                    space_clone
                );
                let req = add_edge_api_key_with_body(
                    s_clone.edge_api_key().as_deref(),
                    &node_id,
                    s_clone.http.post(&url),
                    &bytes_vec,
                )
                .header("Content-Type", "image/jpeg")
                .body(bytes_vec);
                let _ = tokio::time::timeout(Duration::from_secs(5), req.send()).await;
            });
        }
    }

    Ok(Json(serde_json::json!({
        "accepted": true,
        "spaceId": space_id,
        "snapshotUrl": format!("/api/v1/spaces/{}/snapshot", safe_space),
        "bytes": body.len()
    })))
}

async fn space_snapshot(Path(space_id): Path<String>, State(s): State<AppState>) -> Response {
    let safe_space = sanitize_path_segment(&space_id);
    let live_path = s
        .data_directory
        .join("snapshots")
        .join(format!("{}.jpg", safe_space));

    // 检查该车位绑定的摄像头在线状态
    let camera_online = s
        .cameras
        .snapshot()
        .ok()
        .and_then(|cams| cams.into_iter().find(|c| c.space_id == space_id))
        .map(|c| c.state == charging_space_edge::CameraState::Online)
        .unwrap_or(true);

    // 检查当前快照的时效性（修改时间需在 30 秒内）
    let is_fresh = if let Ok(meta) = tokio::fs::metadata(&live_path).await {
        if let Ok(mtime) = meta.modified() {
            mtime.elapsed().map(|d| d.as_secs() < 30).unwrap_or(false)
        } else {
            false
        }
    } else {
        false
    };

    if live_path.exists() && camera_online && is_fresh {
        if let Ok(bytes) = tokio::fs::read(&live_path).await {
            if is_valid_jpeg(&bytes) {
                return (
                    [
                        (axum::http::header::CONTENT_TYPE, "image/jpeg"),
                        (axum::http::header::CACHE_CONTROL, "no-store, no-cache, must-revalidate"),
                    ],
                    bytes,
                )
                    .into_response();
            }
        }
    }

    let evidence_path = s
        .trackers
        .get_tracker(&space_id)
        .ok()
        .flatten()
        .and_then(|t| t.last_evidence_path);

    let is_occupied = s
        .trackers
        .get_tracker(&space_id)
        .ok()
        .flatten()
        .map(|t| t.phase == charging_space_edge::PresencePhase::Occupied)
        .unwrap_or(false);

    if is_occupied {
        if let Some(path_str) = evidence_path {
            let p = if path_str.starts_with("/api/v1/evidences/") || path_str.starts_with("/evidences/")
            {
                let fname = path_str.rsplit('/').next().unwrap_or("");
                s.data_directory.join("evidence").join(fname)
            } else {
                std::path::PathBuf::from(&path_str)
            };
            if p.exists() {
                if let Ok(bytes) = tokio::fs::read(&p).await {
                    if is_valid_jpeg(&bytes) {
                        return (
                            [
                                (axum::http::header::CONTENT_TYPE, "image/jpeg"),
                                (axum::http::header::CACHE_CONTROL, "no-store, no-cache, must-revalidate"),
                            ],
                            bytes,
                        )
                            .into_response();
                    }
                }
            }
        }
    }

    (
        StatusCode::NOT_FOUND,
        [
            (axum::http::header::CONTENT_TYPE, "application/json"),
            (axum::http::header::CACHE_CONTROL, "no-store, no-cache, must-revalidate"),
        ],
        Json(serde_json::json!({
            "error": "camera_offline",
            "message": "摄像头已离线或视频流中断",
            "spaceId": space_id
        })),
    )
        .into_response()
}

async fn get_evidence_file(
    Path(filename): Path<String>,
    State(s): State<AppState>,
) -> Response {
    if filename.contains("..") || filename.contains('/') || filename.contains('\\') {
        return StatusCode::FORBIDDEN.into_response();
    }
    let path = s.data_directory.join("evidence").join(&filename);
    if !path.exists() || !path.is_file() {
        return StatusCode::NOT_FOUND.into_response();
    }
    match tokio::fs::read(&path).await {
        Ok(bytes) => ([(axum::http::header::CONTENT_TYPE, "image/jpeg")], bytes).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

async fn health(State(s): State<AppState>) -> Json<Health> {
    let charger_snapshots = s.chargers.list_snapshots().unwrap_or_default();
    let charger_count = charger_snapshots.len();
    let charging_count = charger_snapshots
        .iter()
        .filter(|c| c.status == charging_space_edge::ChargerStatus::Charging)
        .count();

    let space_statuses = s.space_statuses.snapshot().unwrap_or_default();
    let active_alarms = space_statuses.iter().filter(|st| st.alarming).count();
    let occupied_spaces = space_statuses
        .iter()
        .filter(|st| st.phase == charging_space_edge::PresencePhase::Occupied)
        .count();

    let pending = s.sqlite.pending_count().unwrap_or(0);
    let wd_report = s.watchdog.tick();
    let hw_report = s.hardware_monitor.report();
    let cloud_url = s.cloud_url();
    let cloud_configured = cloud_url.is_some();

    Json(Health {
        status: if wd_report.all_subsystems_healthy {
            "UP"
        } else {
            "DEGRADED"
        },
        max_video_channels: 16,
        node_id: s.node_id(),
        cloud_url,
        cloud_configured,
        cloud_connected: cloud_configured && s.cloud_connected.load(Ordering::Relaxed),
        vision_mode: s.vision_mode.clone(),
        ai_engine: s.ai_engine.clone(),
        version: s.upgrade_manager.get_current_version(),
        pending_events: pending,
        configuration_version: s.configuration.read().await.version,
        configured_spaces: s
            .configuration
            .read()
            .await
            .bindings
            .iter()
            .filter(|binding| binding.enabled)
            .count(),
        camera_channels: s.cameras.count().unwrap_or(0),
        charger_channels: charger_count,
        watchdog_healthy: wd_report.all_subsystems_healthy,
        hardware_status: hw_report.status,
        active_alarms,
        occupied_spaces,
        charging_spaces: charging_count,
    })
}
async fn relay_states(
    State(s): State<AppState>,
) -> Result<Json<HashMap<String, bool>>, (StatusCode, String)> {
    s.relay
        .snapshot()
        .map(Json)
        .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error))
}
async fn relay_details(
    State(s): State<AppState>,
) -> Result<Json<HashMap<String, RelayChannelStatus>>, (StatusCode, String)> {
    s.relay
        .detailed_snapshot()
        .map(Json)
        .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error))
}

pub struct ReceiverStream<T> {
    inner: tokio::sync::mpsc::Receiver<T>,
}

impl<T> ReceiverStream<T> {
    pub fn new(inner: tokio::sync::mpsc::Receiver<T>) -> Self {
        Self { inner }
    }
}

impl<T> Stream for ReceiverStream<T> {
    type Item = T;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.inner.poll_recv(cx)
    }
}

async fn stream_spaces(
    State(s): State<AppState>,
) -> Sse<ReceiverStream<Result<Event, Infallible>>> {
    let (tx, rx) = tokio::sync::mpsc::channel(16);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(1500));
        loop {
            interval.tick().await;
            let snapshot = s.space_statuses.snapshot();
            let payload = serde_json::to_string(&snapshot).unwrap_or_else(|_| "[]".into());
            let event = Event::default().event("spaces").data(payload);
            if tx.send(Ok(event)).await.is_err() {
                break;
            }
        }
    });
    Sse::new(ReceiverStream::new(rx)).keep_alive(KeepAlive::default())
}

async fn stream_events(
    State(s): State<AppState>,
) -> Sse<ReceiverStream<Result<Event, Infallible>>> {
    let (tx, rx) = tokio::sync::mpsc::channel(16);
    tokio::spawn(async move {
        let init_event = Event::default().event("connected").data(format!(
            "{{\"nodeId\":\"{}\",\"status\":\"CONNECTED\"}}",
            s.node_id()
        ));
        if tx.send(Ok(init_event)).await.is_err() {
            return;
        }

        let mut interval = tokio::time::interval(Duration::from_millis(2000));
        loop {
            interval.tick().await;
            let active_events = s.lifecycle.active_events().unwrap_or_default();
            let payload = serde_json::to_string(&active_events).unwrap_or_else(|_| "[]".into());
            let event = Event::default().event("events").data(payload);
            if tx.send(Ok(event)).await.is_err() {
                break;
            }
        }
    });
    Sse::new(ReceiverStream::new(rx)).keep_alive(KeepAlive::default())
}

async fn alarm_protection_loop(s: AppState) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
    loop {
        interval.tick().await;
        let limit = {
            let cfg = s.configuration.read().await;
            cfg.rules.alarm_auto_stop_seconds
        };
        match s.relay.tick_timeout_protection(limit) {
            Ok(()) => s.watchdog.feed("hardware"),
            Err(error) => eprintln!("alarm protection failed: {}", error),
        }
    }
}
async fn get_rules(State(s): State<AppState>) -> Json<RuleConfig> {
    Json(s.configuration.read().await.rules.clone())
}
async fn put_rules(
    State(s): State<AppState>,
    Json(r): Json<RuleConfig>,
) -> Result<Json<RuleConfig>, (StatusCode, String)> {
    r.validate()
        .map_err(|error| (StatusCode::BAD_REQUEST, error))?;
    let mut configuration = s.configuration.read().await.clone();
    configuration.version = configuration.version.saturating_add(1);
    configuration.updated_at = Utc::now();
    configuration.rules = r.clone();
    apply_configuration(&s, configuration).await?;
    Ok(Json(r))
}
async fn get_configuration(State(s): State<AppState>) -> Json<PlatformConfiguration> {
    Json(s.configuration.read().await.clone())
}
async fn camera_states(
    State(s): State<AppState>,
) -> Result<Json<Vec<CameraChannel>>, (StatusCode, String)> {
    s.cameras
        .snapshot()
        .map(Json)
        .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error))
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RegisterCameraRequest {
    camera_id: String,
    #[serde(default)]
    channel_id: Option<String>,
    space_id: String,
    #[serde(default)]
    gb_device_id: Option<String>,
    #[serde(default)]
    stream_url: Option<String>,
    #[serde(default)]
    protocol: Option<String>,
    #[serde(default)]
    roi_json: Option<String>,
}

async fn register_camera(
    State(s): State<AppState>,
    Json(req): Json<RegisterCameraRequest>,
) -> Result<Json<CameraChannel>, (StatusCode, String)> {
    let channel_id = req
        .channel_id
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| req.camera_id.clone());
    let gb_device_id = req
        .gb_device_id
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| req.camera_id.clone());
    let camera = CameraChannel {
        camera_id: req.camera_id,
        channel_id,
        space_id: req.space_id,
        gb_device_id,
        state: CameraState::Registered,
        stream_url: req.stream_url.filter(|v| !v.trim().is_empty()),
        last_heartbeat: None,
        protocol: Some(
            req.protocol
                .filter(|v| !v.trim().is_empty())
                .unwrap_or_else(|| "RTSP".into()),
        ),
        roi_json: req.roi_json.filter(|v| !v.trim().is_empty()),
    };
    s.cameras
        .register(camera.clone())
        .map_err(|error| (StatusCode::BAD_REQUEST, error))?;
    let _ = s.sqlite.upsert_camera(&camera);
    Ok(Json(camera))
}

async fn delete_camera(
    Path(camera_id): Path<String>,
    State(s): State<AppState>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let removed = s
        .cameras
        .remove(&camera_id)
        .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error))?;
    let _ = s.sqlite.delete_camera(&camera_id);
    if !removed {
        return Err((
            StatusCode::NOT_FOUND,
            format!("camera {} not found", camera_id),
        ));
    }
    Ok(Json(serde_json::json!({
        "deleted": true,
        "cameraId": camera_id
    })))
}

async fn camera_heartbeat(
    Path(camera_id): Path<String>,
    State(s): State<AppState>,
) -> Result<Json<CameraChannel>, (StatusCode, String)> {
    let updated = s
        .cameras
        .heartbeat(&camera_id, Utc::now())
        .map_err(|error| (StatusCode::NOT_FOUND, error))?;
    let _ = s.sqlite.upsert_camera(&updated);
    Ok(Json(updated))
}
async fn put_configuration(
    State(s): State<AppState>,
    Json(configuration): Json<PlatformConfiguration>,
) -> Result<Json<PlatformConfiguration>, (StatusCode, String)> {
    apply_configuration(&s, configuration.clone()).await?;
    Ok(Json(configuration))
}
async fn evaluate_space(
    Path(space_id): Path<String>,
    State(s): State<AppState>,
    Json(mut input): Json<EvaluationInput>,
) -> Result<Json<charging_space_edge::EvaluationResult>, (StatusCode, String)> {
    input.space_id = space_id.clone();
    input.whitelisted = s
        .whitelist
        .contains(input.plate.as_deref(), input.now)
        .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error))?;
    let mut result = {
        let configuration = s.configuration.read().await;
        evaluate(&input, &configuration.rules)
    };
    let is_acknowledged = s
        .lifecycle
        .active_event(&space_id)
        .ok()
        .flatten()
        .is_some_and(|e| e.status == charging_space_edge::EventStatus::Acknowledged);
    let alarming = result.should_alarm && !is_acknowledged;
    s.relay
        .set_alarm(&space_id, alarming)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    persist_lifecycle_changes(&s, &input, &mut result).await;
    update_space_status_from_evaluation(&s, &input, alarming)
        .await
        .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error))?;
    Ok(Json(result))
}

async fn list_chargers(
    State(s): State<AppState>,
) -> Result<Json<Vec<ChargerSnapshot>>, (StatusCode, String)> {
    s.chargers
        .list_snapshots()
        .map(Json)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))
}

async fn update_charger_snapshot(
    State(s): State<AppState>,
    Json(update): Json<ExternalChargerUpdate>,
) -> Result<Json<ExternalChargerUpdate>, (StatusCode, String)> {
    let space_id = update.space_id.clone();
    s.chargers
        .update(update.clone())
        .map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    if let Ok(snap) = s.chargers.snapshot(&space_id) {
        let _ = s.sqlite.save_charger_snapshot(&snap);
    }
    s.watchdog.feed("operator");
    refresh_space_status(&s, &space_id, Utc::now())
        .await
        .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error))?;
    Ok(Json(update))
}

async fn charger_offline_detection_loop(s: AppState) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(10));
    loop {
        interval.tick().await;
        s.chargers.touch_heartbeats();
        match s.chargers.tick_offline_detection() {
            Ok(degraded) => {
                s.watchdog.feed("operator");
                for space_id in degraded {
                    eprintln!("charger for space {} offline, soft-degraded", space_id);
                    let _ = refresh_space_status(&s, &space_id, Utc::now()).await;
                }
            }
            Err(error) => eprintln!("charger offline scan failed: {}", error),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DetectionResponse {
    space_id: String,
    phase: PresencePhase,
    transition: String,
    evaluation: Option<charging_space_edge::EvaluationResult>,
}

fn build_snapshot_url(s: &AppState, site_id: &str, space_id: &str) -> Option<String> {
    let safe_space = sanitize_path_segment(space_id);
    let snap_path = s
        .data_directory
        .join("snapshots")
        .join(format!("{}.jpg", safe_space));
    if snap_path.exists() {
        Some(format!("/api/v1/sites/{}/spaces/{}/snapshot", site_id, space_id))
    } else {
        None
    }
}

fn infer_plate_color(
    plate_color: Option<&str>,
    vehicle_type: &charging_space_edge::VehicleType,
) -> String {
    if let Some(c) = plate_color.map(str::trim).filter(|c| !c.is_empty()) {
        return c.to_uppercase();
    }
    match vehicle_type {
        charging_space_edge::VehicleType::NewEnergy => "GREEN".into(),
        charging_space_edge::VehicleType::Fuel => "BLUE".into(),
        charging_space_edge::VehicleType::SpecialVehicle => "WHITE".into(),
        charging_space_edge::VehicleType::HeavyVehicle => "YELLOW".into(),
        charging_space_edge::VehicleType::NonMotor => "TWO_WHEELER".into(),
        charging_space_edge::VehicleType::Obstacle => "OBSTACLE".into(),
        charging_space_edge::VehicleType::Unknown => "UNKNOWN".into(),
    }
}

fn decode_base64(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut buf = 0u32;
    let mut bits = 0;
    for &b in s.as_bytes() {
        let val = match b {
            b'A'..=b'Z' => (b - b'A') as u32,
            b'a'..=b'z' => (b - b'a' + 26) as u32,
            b'0'..=b'9' => (b - b'0' + 52) as u32,
            b'+' => 62,
            b'/' => 63,
            b'=' | b'\r' | b'\n' | b' ' => continue,
            _ => return None,
        };
        buf = (buf << 6) | val;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    Some(out)
}

async fn maybe_capture_space_evidence(
    s: &AppState,
    space_id: &str,
    plate_str: &str,
    violation_type: &str,
    is_simulated: bool,
    stream_type: &str,
    event_id: Option<String>,
    override_jpeg: Option<&[u8]>,
) -> Option<String> {
    let jpeg_bytes = if let Some(b) = override_jpeg {
        b.to_vec()
    } else {
        let safe_space = sanitize_path_segment(space_id);
        let snapshot_path = s
            .data_directory
            .join("snapshots")
            .join(format!("{}.jpg", safe_space));
        tokio::fs::read(&snapshot_path).await.ok()?
    };
    if !is_valid_jpeg(&jpeg_bytes) {
        return None;
    }
    let frame_obj = VideoFrame {
        channel_id: 0,
        space_id: space_id.to_string(),
        timestamp: Utc::now(),
        width: 1920,
        height: 1080,
        raw_data: jpeg_bytes,
        is_simulated,
        data_source: stream_type.to_string(),
    };
    let watermarked = EvidenceWatermark::apply(&frame_obj, plate_str, space_id, violation_type);
    let (target_url, sidecar) = s
        .evidence_storage
        .upload_evidence_with_context(
            space_id,
            &watermarked.data,
            "jpg",
            Some(plate_str),
            Some(violation_type),
            is_simulated,
        )
        .await
        .ok()?;
    let evidence_url = if target_url.starts_with("http://") || target_url.starts_with("https://") {
        target_url
    } else {
        format!("/api/v1/evidences/{}", sidecar.file_name)
    };
    let _ = s.trackers.update_evidence_path(space_id, evidence_url.clone());
    let local_path = s
        .data_directory
        .join("evidence")
        .join(&sidecar.file_name)
        .to_string_lossy()
        .to_string();
    let _ = s.sqlite.queue_evidence_upload(&PendingEvidenceUpload {
        file_name: sidecar.file_name,
        local_path,
        space_id: space_id.to_string(),
        event_id,
    });
    Some(evidence_url)
}

async fn archive_recognition_snapshot(
    s: &AppState,
    space_id: &str,
    record_id: &str,
    site_id: &str,
    override_jpeg: Option<&[u8]>,
) -> Option<String> {
    let bytes = if let Some(b) = override_jpeg {
        b.to_vec()
    } else {
        let safe_space = sanitize_path_segment(space_id);
        let snap_source = s
            .data_directory
            .join("snapshots")
            .join(format!("{}.jpg", safe_space));
        if !snap_source.exists() {
            return build_snapshot_url(s, site_id, space_id);
        }
        match tokio::fs::read(&snap_source).await {
            Ok(b) if !b.is_empty() => b,
            _ => return build_snapshot_url(s, site_id, space_id),
        }
    };

    let evidence_filename = format!("rec_{}.jpg", record_id);
    let evidence_dir = s.data_directory.join("evidence");
    let _ = tokio::fs::create_dir_all(&evidence_dir).await;
    let evidence_path = evidence_dir.join(&evidence_filename);
    if tokio::fs::write(&evidence_path, &bytes).await.is_ok() {
        let _ = s.sqlite.queue_evidence_upload(&PendingEvidenceUpload {
            file_name: evidence_filename.clone(),
            local_path: evidence_path.to_string_lossy().to_string(),
            space_id: space_id.to_string(),
            event_id: None,
        });
        Some(format!("/api/v1/evidences/{}", evidence_filename))
    } else {
        build_snapshot_url(s, site_id, space_id)
    }
}

async fn handle_detection(
    Path(space_id): Path<String>,
    State(s): State<AppState>,
    Json(mut detection): Json<RawDetection>,
) -> Result<Json<DetectionResponse>, (StatusCode, String)> {
    detection.space_id = space_id.clone();
    s.metrics.record_frame();

    let incoming_jpeg = detection
        .image_base64
        .as_deref()
        .and_then(decode_base64)
        .filter(|b| is_valid_jpeg(b));

    // 若携带了识别帧真实抓拍 JPEG 数据，立即落盘为本车位最新实景快照
    if let Some(ref jpeg) = incoming_jpeg {
        let safe_space = sanitize_path_segment(&space_id);
        let snapshots_dir = s.data_directory.join("snapshots");
        let _ = tokio::fs::create_dir_all(&snapshots_dir).await;
        let snap_path = snapshots_dir.join(format!("{}.jpg", safe_space));
        let _ = tokio::fs::write(&snap_path, jpeg).await;
    }

    let (mut rules, site_id, bound_camera_id) = {
        let cfg = s.configuration.read().await;
        let site = cfg
            .bindings
            .iter()
            .find(|b| b.space_id == space_id)
            .map(|b| b.site_id.clone())
            .unwrap_or_else(|| "default".into());
        let cam = cfg
            .bindings
            .iter()
            .find(|b| b.space_id == space_id)
            .map(|b| b.camera_id.clone());
        (cfg.rules.clone(), site, cam)
    };

    if let Ok(sec_str) = std::env::var("EDGE_ENTER_CONFIRM_SECONDS") {
        if let Ok(sec) = sec_str.parse::<i64>() {
            rules.vehicle_enter_confirm_seconds = sec;
        }
    }
    if let Ok(sec_str) = std::env::var("EDGE_LEAVE_CONFIRM_SECONDS") {
        if let Ok(sec) = sec_str.parse::<i64>() {
            rules.vehicle_leave_confirm_seconds = sec;
        }
    }

    let transition = s
        .trackers
        .process_detection(&detection, &rules)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    s.watchdog.feed("pipeline");

    let tracker = s
        .trackers
        .get_tracker(&space_id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?
        .unwrap_or_else(|| SpacePresenceTracker::new(space_id.clone()));

    let mut evaluation_result = None;
    let mut is_alarming = false;

    match &transition {
        TransitionEvent::VehicleEntered { .. } => {
            let _ = s.sqlite.record_presence_log(
                &space_id,
                tracker
                    .candidate_plate
                    .as_deref()
                    .or(detection.plate.as_deref()),
                &tracker.candidate_vehicle_type,
                "OCCUPIED",
                "ENTER",
            );
        }
        TransitionEvent::VehicleLeft { occupied_duration_seconds: _, plate, vehicle_type } => {
            let _ = s.sqlite.record_presence_log(
                &space_id,
                plate.as_deref().or(detection.plate.as_deref()),
                vehicle_type,
                "EMPTY",
                "LEAVE",
            );
            // 车辆确认离场，立即关闭对应车位继电器消警
            s.relay
                .set_alarm(&space_id, false)
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
            persist_cleared_event(&s, &space_id, detection.captured_at).await;

            // 归档离场抓拍快照并保存离场流水记录 (LEAVE)，确保前端流水表显示明确的驶离事件
            let rec_id = Uuid::new_v4().to_string();
            let leave_snap_url = archive_recognition_snapshot(&s, &space_id, &rec_id, &site_id, incoming_jpeg.as_deref()).await;
            let camera_id = detection
                .camera_id
                .clone()
                .filter(|c| !c.trim().is_empty())
                .or(bound_camera_id.clone())
                .unwrap_or_else(|| "unknown".into());
            let leave_rec = RecognitionRecord {
                record_id: rec_id,
                space_id: space_id.clone(),
                camera_id,
                plate: plate.clone().or_else(|| detection.plate.clone()),
                plate_color: infer_plate_color(detection.plate_color.as_deref(), vehicle_type),
                vehicle_type: vehicle_type.clone(),
                confidence: 1.0,
                snapshot_url: leave_snap_url,
                direction: "LEAVE".into(),
                judgment_status: "NORMAL".into(),
                is_simulated: detection.is_simulated,
                recognized_at: detection.captured_at,
            };
            let _ = s.sqlite.save_recognition_record(&leave_rec);
        }
        TransitionEvent::None => {}
    }

    if !matches!(&transition, TransitionEvent::VehicleLeft { .. })
        && (tracker.phase == PresencePhase::Occupied || tracker.phase == PresencePhase::Leaving)
    {
        let now = detection.captured_at;
        let charger_snapshot = s.chargers.snapshot(&space_id).ok();
        let charger_status = charger_snapshot
            .as_ref()
            .map(|c| c.status.clone())
            .unwrap_or(ChargerStatus::Unknown);
        let completed_at = charger_snapshot.as_ref().and_then(|c| c.completed_at);

        let is_whitelisted = s
            .whitelist
            .contains(tracker.candidate_plate.as_deref(), now)
            .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error))?;
        if let Some(eval_input) =
            tracker.build_evaluation_input(now, charger_status, completed_at, is_whitelisted)
        {
            let mut result = evaluate(&eval_input, &rules);
            let is_acknowledged = s
                .lifecycle
                .active_event(&space_id)
                .ok()
                .flatten()
                .is_some_and(|e| e.status == charging_space_edge::EventStatus::Acknowledged);
            let alarming = result.should_alarm && !is_acknowledged;
            is_alarming = alarming;
            s.relay
                .set_alarm(&space_id, alarming)
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
            if alarming {
                s.metrics.record_alarm();
            }
            if result.decision == Decision::Violation {
                s.metrics.record_violation();
            }
            if alarming && tracker.last_evidence_path.is_none() {
                let plate_str = eval_input.plate.as_deref().unwrap_or("UNKNOWN");
                let vtype = result.violation_type.as_deref().unwrap_or("VIOLATION");
                let _ = maybe_capture_space_evidence(
                    &s,
                    &space_id,
                    plate_str,
                    vtype,
                    detection.is_simulated,
                    &detection.data_source,
                    None,
                    incoming_jpeg.as_deref(),
                )
                .await;
            }
            persist_lifecycle_changes(&s, &eval_input, &mut result).await;
            evaluation_result = Some(result);
        }
    }

    if tracker.phase == PresencePhase::Empty
        && !detection.has_vehicle
        && !matches!(&transition, TransitionEvent::VehicleLeft { .. })
    {
        s.relay
            .set_alarm(&space_id, false)
            .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error))?;
        persist_cleared_event(&s, &space_id, detection.captured_at).await;
    }

    let entered = matches!(&transition, TransitionEvent::VehicleEntered { .. });
    if detection.has_vehicle && (detection.plate.is_some() || entered) {
        let camera_id = detection
            .camera_id
            .clone()
            .filter(|c| !c.trim().is_empty())
            .or(bound_camera_id)
            .unwrap_or_else(|| "unknown".into());

        let rec_plate = detection
            .plate
            .clone()
            .or_else(|| tracker.candidate_plate.clone());

        let rec_color = infer_plate_color(
            detection.plate_color.as_deref(),
            &detection.vehicle_type,
        );

        let is_fuel = detection.vehicle_type == charging_space_edge::VehicleType::Fuel
            || rec_color == "BLUE"
            || rec_color == "YELLOW"
            || tracker.candidate_vehicle_type == charging_space_edge::VehicleType::Fuel;

        let is_obstacle_or_nonmotor = detection.vehicle_type == charging_space_edge::VehicleType::NonMotor
            || detection.vehicle_type == charging_space_edge::VehicleType::Obstacle
            || detection.vehicle_type == charging_space_edge::VehicleType::HeavyVehicle;

        let is_whitelisted = s
            .whitelist
            .contains(rec_plate.as_deref(), detection.captured_at)
            .unwrap_or(false);

        let judgment_status = if is_whitelisted {
            "WHITELIST_ALLOWED".into()
        } else if is_fuel || is_obstacle_or_nonmotor {
            "VIOLATION".into()
        } else if evaluation_result
            .as_ref()
            .is_some_and(|r| r.decision == Decision::Violation)
            || is_alarming
        {
            "VIOLATION".into()
        } else {
            "NORMAL".into()
        };

        // 如果是燃油车或非法占用（两轮车/桌椅杂物/大货车）且未在白名单中，即便在进场观察期也立即启动声光告警与抓拍取证
        if (is_fuel || is_obstacle_or_nonmotor) && !is_whitelisted {
            let _ = s.relay.set_alarm(&space_id, true);
            is_alarming = true;
            s.metrics.record_alarm();
            s.metrics.record_violation();
            if tracker.last_evidence_path.is_none() {
                let p_str = rec_plate.as_deref().unwrap_or("UNKNOWN");
                let viol_type = match detection.vehicle_type {
                    charging_space_edge::VehicleType::NonMotor => "NON_MOTOR_VEHICLE",
                    charging_space_edge::VehicleType::Obstacle => "OBSTACLE_OCCUPATION",
                    charging_space_edge::VehicleType::HeavyVehicle => "HEAVY_VEHICLE",
                    _ => "FUEL_VEHICLE",
                };
                let _ = maybe_capture_space_evidence(
                    &s,
                    &space_id,
                    p_str,
                    viol_type,
                    detection.is_simulated,
                    &detection.data_source,
                    None,
                    incoming_jpeg.as_deref(),
                )
                .await;
            }
        }

        let direction = if entered { "ENTER" } else { "STAY" };
        let rec_id = Uuid::new_v4().to_string();
        let snapshot_url = archive_recognition_snapshot(&s, &space_id, &rec_id, &site_id, incoming_jpeg.as_deref()).await;

        let rec = RecognitionRecord {
            record_id: rec_id,
            space_id: space_id.clone(),
            camera_id,
            plate: rec_plate,
            plate_color: rec_color,
            vehicle_type: if is_fuel { charging_space_edge::VehicleType::Fuel } else { detection.vehicle_type.clone() },
            confidence: detection.confidence.unwrap_or(0.0),
            snapshot_url,
            direction: direction.into(),
            judgment_status,
            is_simulated: detection.is_simulated,
            recognized_at: detection.captured_at,
        };
        let _ = s.sqlite.save_recognition_record(&rec);
    }

    let transition_str = match transition {
        TransitionEvent::None => "NONE",
        TransitionEvent::VehicleEntered { .. } => "VEHICLE_ENTERED",
        TransitionEvent::VehicleLeft { .. } => "VEHICLE_LEFT",
    };
    refresh_space_status_with_source(
        &s,
        &space_id,
        detection.captured_at,
        Some((
            detection.is_simulated,
            detection.data_source.clone(),
            detection.confidence,
        )),
    )
    .await
    .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error))?;

    Ok(Json(DetectionResponse {
        space_id,
        phase: tracker.phase,
        transition: transition_str.into(),
        evaluation: evaluation_result,
    }))
}

async fn get_space_tracker(
    Path(space_id): Path<String>,
    State(s): State<AppState>,
) -> Result<Json<Option<SpacePresenceTracker>>, (StatusCode, String)> {
    s.trackers
        .get_tracker(&space_id)
        .map(Json)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AckResponse {
    accepted: bool,
    space_id: Option<String>,
    event_id: Option<Uuid>,
    status: String,
    message: String,
}

async fn silence_space(
    Path(space_id): Path<String>,
    State(s): State<AppState>,
) -> Result<Json<AckResponse>, (StatusCode, String)> {
    let now = Utc::now();
    // 1. 断开该车位声光继电器输出
    let _ = s.relay.set_alarm(&space_id, false);
    // 2. 将生命周期事件标记为 Acknowledged
    let event = s
        .lifecycle
        .acknowledge_space(&space_id, now)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;

    if let Some(event) = event {
        let _ = s.sqlite.save_event(&event);
        if let Some(base) = s.cloud_url() {
            flush_pending(&s, &base).await;
        }
        let _ = refresh_space_status(&s, &space_id, now).await;
        Ok(Json(AckResponse {
            accepted: true,
            space_id: Some(space_id),
            event_id: Some(event.event_id),
            status: "ACKNOWLEDGED".into(),
            message: "车位声光已就地消音，违规事件已标记为人工确认闭环".into(),
        }))
    } else {
        let _ = refresh_space_status(&s, &space_id, now).await;
        Ok(Json(AckResponse {
            accepted: true,
            space_id: Some(space_id),
            event_id: None,
            status: "SILENCED".into(),
            message: "车位声光已就地复位（当前无活跃违规事件）".into(),
        }))
    }
}

async fn acknowledge_event_endpoint(
    Path(event_id): Path<Uuid>,
    State(s): State<AppState>,
) -> Result<Json<AckResponse>, (StatusCode, String)> {
    let now = Utc::now();
    let event = s
        .lifecycle
        .acknowledge_event_by_id(event_id, now)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;

    if let Some(event) = event {
        let space_id = event.space_id.clone();
        let _ = s.relay.set_alarm(&space_id, false);
        let _ = s.sqlite.save_event(&event);
        if let Some(base) = s.cloud_url() {
            flush_pending(&s, &base).await;
        }
        let _ = refresh_space_status(&s, &space_id, now).await;
        Ok(Json(AckResponse {
            accepted: true,
            space_id: Some(space_id),
            event_id: Some(event_id),
            status: "ACKNOWLEDGED".into(),
            message: "违规事件已就地确认，关联车位声光已自动关闭".into(),
        }))
    } else {
        Err((
            StatusCode::NOT_FOUND,
            format!("活跃事件 {} 不存在或已闭环", event_id),
        ))
    }
}

async fn get_space_statuses(
    State(s): State<AppState>,
) -> Result<Json<Vec<SpaceRuntimeStatus>>, (StatusCode, String)> {
    s.space_statuses
        .snapshot()
        .map(Json)
        .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error))
}

async fn update_space_status_from_evaluation(
    s: &AppState,
    input: &EvaluationInput,
    alarming: bool,
) -> Result<(), String> {
    let configuration = s.configuration.read().await;
    let site_id = configuration
        .bindings
        .iter()
        .find(|binding| binding.space_id == input.space_id)
        .map(|binding| binding.site_id.clone())
        .ok_or_else(|| format!("space {} is not configured", input.space_id))?;
    drop(configuration);
    let active_event_id = s
        .lifecycle
        .active_event(&input.space_id)?
        .map(|event| event.event_id);
    let existing = s
        .space_statuses
        .snapshot()
        .ok()
        .and_then(|all| all.into_iter().find(|st| st.space_id == input.space_id));
    let (is_simulated, stream_type, prev_conf, prev_snap) = existing
        .map(|st| (st.is_simulated, st.stream_type, st.confidence, st.snapshot_url))
        .unwrap_or((false, None, 0.0, None));
    let tracker_conf = s
        .trackers
        .get_tracker(&input.space_id)
        .ok()
        .flatten()
        .map(|t| t.last_confidence)
        .filter(|c| *c > 0.0);

    let status = SpaceRuntimeStatus {
        node_id: s.node_id(),
        site_id: site_id.clone(),
        space_id: input.space_id.clone(),
        phase: PresencePhase::Occupied,
        charger_status: input.charger_status.clone(),
        plate: input.plate.clone(),
        vehicle_type: input.vehicle_type.clone(),
        alarming,
        active_event_id,
        observed_at: input.now,
        is_simulated,
        stream_type,
        snapshot_url: build_snapshot_url(s, &site_id, &input.space_id).or(prev_snap),
        confidence: tracker_conf.unwrap_or(prev_conf),
    };
    s.space_statuses.update(status.clone())?;
    let _ = s.sqlite.save_space_status(&status);
    Ok(())
}

async fn refresh_space_status(
    s: &AppState,
    space_id: &str,
    observed_at: chrono::DateTime<Utc>,
) -> Result<(), String> {
    refresh_space_status_with_source(s, space_id, observed_at, None).await
}

async fn refresh_space_status_with_source(
    s: &AppState,
    space_id: &str,
    observed_at: chrono::DateTime<Utc>,
    source_override: Option<(bool, String, Option<f64>)>,
) -> Result<(), String> {
    let tracker = s
        .trackers
        .get_tracker(space_id)?
        .unwrap_or_else(|| SpacePresenceTracker::new(space_id.to_string()));
    let charger_status = s
        .chargers
        .snapshot(space_id)
        .map(|snapshot| snapshot.status)
        .unwrap_or(ChargerStatus::Unknown);
    let alarming = s.relay.snapshot()?.get(space_id).copied().unwrap_or(false);
    let active_event_id = s
        .lifecycle
        .active_event(space_id)?
        .map(|event| event.event_id);
    let configuration = s.configuration.read().await;
    let site_id = configuration
        .bindings
        .iter()
        .find(|binding| binding.space_id == space_id)
        .map(|binding| binding.site_id.clone())
        .ok_or_else(|| format!("space {space_id} is not configured"))?;
    drop(configuration);

    let existing = s
        .space_statuses
        .snapshot()
        .ok()
        .and_then(|all| all.into_iter().find(|st| st.space_id == space_id));
    let (prev_sim, prev_stream, prev_conf, prev_snap) = existing
        .map(|st| (st.is_simulated, st.stream_type, st.confidence, st.snapshot_url))
        .unwrap_or((false, None, 0.0, None));

    let tracker_conf = if tracker.last_confidence > 0.0 {
        Some(tracker.last_confidence)
    } else {
        None
    };

    let (is_simulated, stream_type, confidence) = match source_override {
        Some((sim, src, conf)) => (
            sim,
            Some(src),
            conf.or(tracker_conf).unwrap_or(prev_conf),
        ),
        None => (
            prev_sim,
            prev_stream,
            tracker_conf.unwrap_or(if tracker.phase == PresencePhase::Empty { 0.0 } else { prev_conf }),
        ),
    };

    let status = SpaceRuntimeStatus {
        node_id: s.node_id(),
        site_id: site_id.clone(),
        space_id: space_id.to_string(),
        phase: tracker.phase,
        charger_status,
        plate: tracker.candidate_plate,
        vehicle_type: tracker.candidate_vehicle_type,
        alarming,
        active_event_id,
        observed_at,
        is_simulated,
        stream_type,
        snapshot_url: build_snapshot_url(s, &site_id, space_id).or(prev_snap),
        confidence,
    };
    s.space_statuses.update(status.clone())?;
    let _ = s.sqlite.save_space_status(&status);
    Ok(())
}

async fn persist_lifecycle_changes(
    s: &AppState,
    input: &EvaluationInput,
    result: &mut charging_space_edge::EvaluationResult,
) {
    let changes = match s.lifecycle.process_evaluation(
        input,
        result,
        s.trackers
            .get_tracker(&input.space_id)
            .ok()
            .flatten()
            .and_then(|tracker| tracker.last_evidence_path),
    ) {
        Ok(changes) => changes,
        Err(error) => {
            eprintln!("event lifecycle failed: {}", error);
            return;
        }
    };
    for event in changes {
        if let Err(error) = s.sqlite.save_event(&event) {
            eprintln!(
                "event {} could not be saved to sqlite: {}",
                event.event_id, error
            );
        }
    }
    if let Some(base) = s.cloud_url() {
        flush_pending(s, &base).await;
    }
}

async fn persist_cleared_event(s: &AppState, space_id: &str, now: chrono::DateTime<Utc>) {
    let changes = match s.lifecycle.clear_space(space_id, now) {
        Ok(changes) => changes,
        Err(error) => {
            eprintln!("event clear failed: {}", error);
            return;
        }
    };
    for event in changes {
        if let Err(error) = s.sqlite.save_event(&event) {
            eprintln!(
                "cleared event {} could not be saved to sqlite: {}",
                event.event_id, error
            );
        }
    }
    if let Some(base) = s.cloud_url() {
        flush_pending(s, &base).await;
    }
}

async fn flush_pending(s: &AppState, base: &str) {
    let _guard = s.upload_lock.lock().await;
    let base_trimmed = base.trim_end_matches('/');
    let node_id = s.node_id();

    // 1. 先上传本地取证 JPEG 图片至云端并回填事件 URL
    if let Ok(pending_evidences) = s.sqlite.fetch_pending_evidence_uploads(10) {
        for item in pending_evidences {
            if !std::path::Path::new(&item.local_path).exists() {
                let _ = s.sqlite.mark_evidence_uploaded(&item.file_name);
                continue;
            }
            let bytes = match tokio::fs::read(&item.local_path).await {
                Ok(b) if !b.is_empty() => b,
                _ => {
                    let _ = s.sqlite.mark_evidence_uploaded(&item.file_name);
                    continue;
                }
            };
            let ev_url = format!("{}/api/v1/evidences?filename={}", base_trimmed, item.file_name);
            let req = add_edge_api_key_with_body(
                s.edge_api_key().as_deref(),
                &node_id,
                s.http.post(&ev_url),
                &bytes,
            )
            .header("Content-Type", "image/jpeg")
            .body(bytes);

            match tokio::time::timeout(std::time::Duration::from_secs(5), req.send()).await {
                Ok(Ok(resp)) if resp.status().is_success() => {
                    let cloud_url = resp
                        .json::<serde_json::Value>()
                        .await
                        .ok()
                        .and_then(|v| {
                            v.get("evidenceUrl")
                                .and_then(|u| u.as_str())
                                .map(str::to_string)
                        })
                        .unwrap_or_else(|| format!("/api/v1/evidences/{}", item.file_name));
                    let _ = s.sqlite.mark_evidence_uploaded(&item.file_name);
                    if let Some(eid_str) = item.event_id.as_deref() {
                        if let Ok(eid) = Uuid::parse_str(eid_str) {
                            let _ = s.sqlite.update_event_evidence_url(&eid, &cloud_url);
                        }
                    }
                }
                Ok(Ok(resp)) => {
                    eprintln!("cloud evidence upload returned {}", resp.status());
                    break;
                }
                Ok(Err(error)) => {
                    eprintln!("cloud evidence upload failed: {}", error);
                    break;
                }
                Err(_) => {
                    eprintln!("cloud evidence upload timed out");
                    break;
                }
            }
        }
    }

    // 2. 上传违规事件队列
    let url = format!("{}/api/v1/events", base_trimmed);
    if let Ok(pending_events) = s.sqlite.fetch_pending_events(20) {
        for event in pending_events {
            let req = match authenticated_json_request(s, s.http.post(&url), &event) {
                Ok(req) => req,
                Err(error) => {
                    eprintln!("serialize event failed: {}", error);
                    continue;
                }
            };
            let delivered = match tokio::time::timeout(
                std::time::Duration::from_secs(3),
                req.send(),
            )
            .await
            {
                Ok(Ok(response)) if response.status().is_success() => true,
                Ok(Ok(response)) => {
                    eprintln!("cloud event upload returned {}", response.status());
                    false
                }
                Ok(Err(error)) => {
                    eprintln!("cloud event upload failed: {}", error);
                    false
                }
                Err(_) => {
                    eprintln!("cloud event upload timed out");
                    false
                }
            };

            if delivered {
                s.cloud_connected.store(true, Ordering::Relaxed);
                println!("event {} uploaded to cloud by {}", event.event_id, node_id);
                let _ = s.sqlite.mark_event_synced(&event.event_id);
            } else {
                s.cloud_connected.store(false, Ordering::Relaxed);
                return;
            }
        }
    }

    // 3. 批量上传车牌识别流水记录
    if let Ok(pending_lpr) = s.sqlite.fetch_pending_recognition_records(50) {
        if !pending_lpr.is_empty() {
            let ids: Vec<String> = pending_lpr.iter().map(|r| r.record_id.clone()).collect();
            let lpr_url = format!("{}/api/v1/nodes/{}/lpr/records", base_trimmed, node_id);
            if let Ok(req) = authenticated_json_request(s, s.http.post(&lpr_url), &pending_lpr) {
                match tokio::time::timeout(std::time::Duration::from_secs(5), req.send()).await {
                    Ok(Ok(resp)) if resp.status().is_success() => {
                        let _ = s.sqlite.mark_recognition_synced(&ids);
                    }
                    Ok(Ok(resp)) => {
                        eprintln!("cloud LPR upload returned {}", resp.status());
                    }
                    Ok(Err(error)) => {
                        eprintln!("cloud LPR upload failed: {}", error);
                    }
                    Err(_) => {
                        eprintln!("cloud LPR upload timed out");
                    }
                }
            }
        }
    }
}

async fn upload_loop(s: AppState) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
    loop {
        interval.tick().await;

        // 无论网络状态如何，优先校验并刷新本地 SQLite 状态并喂狗，杜绝因网络重试/超时阻塞导致看门狗误杀
        match s.sqlite.pending_count() {
            Ok(cnt) => {
                s.metrics.set_sqlite_unuploaded(cnt as u64);
                s.watchdog.feed("sqlite");
            }
            Err(e) => {
                eprintln!("[WATCHDOG] sqlite storage probe error: {}", e);
            }
        }

        if let Some(base) = s.cloud_url() {
            flush_pending(&s, &base).await;
        }
    }
}

async fn space_status_upload_loop(s: AppState) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(3));
    loop {
        interval.tick().await;
        let Some(base) = s.cloud_url() else {
            s.cloud_connected.store(false, Ordering::Relaxed);
            continue;
        };
        let url = format!(
            "{}/api/v1/nodes/{}/spaces/status",
            base.trim_end_matches('/'),
            s.node_id()
        );
        let space_ids = s
            .configuration
            .read()
            .await
            .bindings
            .iter()
            .filter(|binding| binding.enabled)
            .map(|binding| binding.space_id.clone())
            .collect::<Vec<_>>();
        for space_id in space_ids {
            let _ = refresh_space_status(&s, &space_id, Utc::now()).await;
        }
        let statuses = match s.space_statuses.snapshot() {
            Ok(statuses) => statuses,
            Err(error) => {
                eprintln!("space status snapshot failed: {}", error);
                continue;
            }
        };
        s.watchdog.feed("pipeline");
        let req = match authenticated_json_request(&s, s.http.post(&url), &statuses) {
            Ok(req) => req,
            Err(error) => {
                eprintln!("serialize statuses failed: {}", error);
                continue;
            }
        };
        match tokio::time::timeout(std::time::Duration::from_secs(3), req.send()).await {
            Ok(Ok(response)) if response.status().is_success() => {
                s.cloud_connected.store(true, Ordering::Relaxed);
            }
            Ok(Ok(response)) => eprintln!("space status upload returned {}", response.status()),
            Ok(Err(error)) => eprintln!("space status upload failed: {}", error),
            Err(_) => eprintln!("space status upload timed out"),
        }
    }
}

async fn apply_configuration(
    s: &AppState,
    configuration: PlatformConfiguration,
) -> Result<(), (StatusCode, String)> {
    let _apply_guard = s.configuration_apply_lock.lock().await;
    configuration
        .validate()
        .map_err(|error| (StatusCode::BAD_REQUEST, error))?;
    let previous = s.configuration.read().await.clone();
    if configuration.version <= previous.version {
        return Err((
            StatusCode::CONFLICT,
            format!(
                "configuration version {} must be greater than current version {}",
                configuration.version, previous.version
            ),
        ));
    }
    let node_id = s.node_id();
    let apply_devices = || -> Result<(), String> {
        s.cameras.apply_bindings(&configuration.bindings)?;
        s.cameras
            .apply_configuration_cameras(&configuration.cameras)?;
        s.chargers.apply_bindings(&configuration.bindings)?;
        s.relay.apply_bindings(&configuration.bindings)?;
        s.whitelist.apply(&configuration.whitelist)?;
        s.space_statuses
            .apply_bindings(&node_id, &configuration.bindings)?;
        Ok(())
    };
    if let Err(error) = apply_devices() {
        let _ = s.cameras.apply_bindings(&previous.bindings);
        let _ = s.cameras.apply_configuration_cameras(&previous.cameras);
        let _ = s.chargers.apply_bindings(&previous.bindings);
        let _ = s.relay.apply_bindings(&previous.bindings);
        let _ = s.whitelist.apply(&previous.whitelist);
        let _ = s
            .space_statuses
            .apply_bindings(&node_id, &previous.bindings);
        return Err((StatusCode::BAD_REQUEST, error));
    }
    if let Err(error) = s.configuration_store.save(&configuration).await {
        let _ = s.cameras.apply_bindings(&previous.bindings);
        let _ = s.cameras.apply_configuration_cameras(&previous.cameras);
        let _ = s.chargers.apply_bindings(&previous.bindings);
        let _ = s.relay.apply_bindings(&previous.bindings);
        let _ = s.whitelist.apply(&previous.whitelist);
        let _ = s
            .space_statuses
            .apply_bindings(&node_id, &previous.bindings);
        return Err((StatusCode::INTERNAL_SERVER_ERROR, error.to_string()));
    }
    let _ = s.sqlite.save_platform_config(&configuration);
    let active_space_ids: Vec<String> = configuration
        .bindings
        .iter()
        .filter(|b| b.enabled)
        .map(|b| b.space_id.clone())
        .collect();
    let _ = s.sqlite.prune_space_statuses(&active_space_ids);
    let _ = s.trackers.prune_unbound_trackers(&active_space_ids);
    let _ = s
        .lifecycle
        .prune_unbound_events(&active_space_ids, chrono::Utc::now());
    if let Ok(cams) = s.cameras.snapshot() {
        let _ = s.sqlite.replace_cameras(&cams);
    }
    *s.configuration.write().await = configuration;
    Ok(())
}

async fn configuration_sync_loop(s: AppState) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(15));
    loop {
        interval.tick().await;
        let Some(base) = s.cloud_url() else {
            continue;
        };
        let url = format!("{}/api/v1/configuration", base.trim_end_matches('/'));
        let response = match tokio::time::timeout(
            std::time::Duration::from_secs(5),
            authenticated_request(&s, s.http.get(&url)).send(),
        )
        .await
        {
            Ok(Ok(response)) => response,
            Ok(Err(error)) => {
                eprintln!("configuration sync failed: {}", error);
                continue;
            }
            Err(_) => {
                eprintln!("configuration sync timed out");
                continue;
            }
        };
        if !response.status().is_success() {
            eprintln!("configuration sync returned {}", response.status());
            continue;
        }
        let incoming = match response.json::<PlatformConfiguration>().await {
            Ok(configuration) => configuration,
            Err(error) => {
                eprintln!("configuration decode failed: {}", error);
                continue;
            }
        };
        let current_version = s.configuration.read().await.version;
        if incoming.version > current_version {
            let incoming_version = incoming.version;
            match apply_configuration(&s, incoming).await {
                Ok(()) => {
                    println!("configuration updated to version {}", incoming_version);
                    report_configuration_status(
                        &s,
                        incoming_version,
                        "APPLIED",
                        "configuration applied",
                    )
                    .await;
                }
                Err((_, error)) => {
                    eprintln!("configuration apply failed: {}", error);
                    report_configuration_status(&s, incoming_version, "REJECTED", &error).await;
                }
            }
        }
    }
}

async fn report_configuration_status(s: &AppState, version: u64, status: &str, message: &str) {
    let Some(base) = s.cloud_url() else {
        return;
    };
    let node_id = s.node_id();
    let url = format!(
        "{}/api/v1/nodes/{}/configuration-status",
        base.trim_end_matches('/'),
        node_id
    );
    let report = ConfigurationApplyReport {
        node_id,
        version,
        status: status.to_string(),
        message: message.to_string(),
        reported_at: Utc::now(),
    };
    *s.last_configuration_report.write().await = Some(report.clone());
    if let Ok(req) = authenticated_json_request(s, s.http.post(url), &report) {
        if let Err(error) = req.send().await {
            eprintln!("configuration status report failed: {}", error);
        }
    }
}

async fn report_upgrade_status(
    s: &AppState,
    target_version: Option<u64>,
    state: &str,
    message: &str,
    last_error: Option<&str>,
) {
    let Some(base) = s.cloud_url() else {
        return;
    };
    let node_id = s.node_id();
    let url = format!(
        "{}/api/v1/nodes/{}/upgrade-status",
        base.trim_end_matches('/'),
        node_id
    );
    let payload = serde_json::json!({
        "nodeId": node_id,
        "currentVersion": s.upgrade_manager.get_current_version(),
        "targetVersion": target_version,
        "state": state,
        "message": message,
        "lastError": last_error,
        "updatedAt": Utc::now()
    });
    if let Ok(req) = authenticated_json_request(s, s.http.post(url), &payload) {
        if let Err(err) = req.send().await {
            eprintln!("[OTA] 上报升级进度失败: {}", err);
        }
    }
}

async fn execute_ota_upgrade(s: &AppState, manifest: &charging_space_edge::UpgradeManifest) {
    let cur_ver = s.upgrade_manager.get_current_version();
    if manifest.version <= cur_ver {
        println!(
            "[OTA] 目标版本 v{} 不高于当前版本 v{}，已同步最新生效状态",
            manifest.version, cur_ver
        );
        report_upgrade_status(
            s,
            Some(manifest.version),
            "COMMITTED",
            &format!("工控机已成功运行在版本 v{}", cur_ver),
            None,
        )
        .await;
        return;
    }

    println!(
        "[OTA] 触发 OTA 固件升级: v{} -> v{}, 下载地址: {}",
        cur_ver, manifest.version, manifest.package_url
    );

    // 1. 下载阶段
    report_upgrade_status(
        s,
        Some(manifest.version),
        "DOWNLOADING",
        &format!("正在下载固件升级包: {}", manifest.package_url),
        None,
    )
    .await;

    // 解析固件下载 URL：如果是相对路径或以 / 开头，自动拼接云端 base_url
    let download_url = if manifest.package_url.starts_with("http://")
        || manifest.package_url.starts_with("https://")
    {
        manifest.package_url.clone()
    } else if let Some(base) = s.cloud_url() {
        format!("{}/{}", base.trim_end_matches('/'), manifest.package_url.trim_start_matches('/'))
    } else {
        manifest.package_url.clone()
    };

    let download_resp = match s.http.get(&download_url).send().await {
        Ok(resp) if resp.status().is_success() => resp,
        Ok(resp) => {
            let err_msg = format!("下载固件失败，云端返回状态码: {}", resp.status());
            eprintln!("[OTA] {}", err_msg);
            report_upgrade_status(
                s,
                Some(manifest.version),
                "ROLLED_BACK",
                "固件下载失败",
                Some(&err_msg),
            )
            .await;
            return;
        }
        Err(e) => {
            let err_msg = format!("网络请求异常: {}", e);
            eprintln!("[OTA] {}", err_msg);
            report_upgrade_status(
                s,
                Some(manifest.version),
                "ROLLED_BACK",
                "固件下载异常",
                Some(&err_msg),
            )
            .await;
            return;
        }
    };

    let pkg_bytes = match download_resp.bytes().await {
        Ok(b) => b.to_vec(),
        Err(e) => {
            let err_msg = format!("读取固件数据流失败: {}", e);
            eprintln!("[OTA] {}", err_msg);
            report_upgrade_status(
                s,
                Some(manifest.version),
                "ROLLED_BACK",
                "读取固件流失败",
                Some(&err_msg),
            )
            .await;
            return;
        }
    };

    // 2. 校验阶段上报
    report_upgrade_status(
        s,
        Some(manifest.version),
        "VERIFYING",
        "正在校验固件签名与 SHA-256 哈希...",
        None,
    )
    .await;

    // 3. 执行就地原子升级与自检
    let sql = s.sqlite.clone();
    let apply_res = s
        .upgrade_manager
        .apply_upgrade(manifest, &pkg_bytes, move || {
            sql.pending_count().map(|_| ()).map_err(|e| e.to_string())
        });

    match apply_res {
        Ok(rep) if rep.state == charging_space_edge::UpgradeState::Committed => {
            println!("[OTA] 固件升级成功并已原子提交: v{}", manifest.version);
            report_upgrade_status(
                s,
                Some(manifest.version),
                "COMMITTED",
                &format!("版本 v{} 升级成功并已提交生效", manifest.version),
                None,
            )
            .await;

            // 升级完成，2 秒后优雅退出看门狗，交由系统守护进程拉起新版本
            let wd = s.watchdog.clone();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                wd.stop();
                std::process::exit(0);
            });
        }
        Ok(rep) => {
            eprintln!("[OTA] 固件升级未完全生效: {:?}", rep.state);
            report_upgrade_status(
                s,
                Some(manifest.version),
                "ROLLED_BACK",
                &rep.message,
                rep.last_error.as_deref(),
            )
            .await;
        }
        Err(err) => {
            eprintln!("[OTA] 固件升级应用失败并已自动回滚: {}", err);
            report_upgrade_status(
                s,
                Some(manifest.version),
                "ROLLED_BACK",
                "固件升级校验或应用失败，已回滚",
                Some(&err),
            )
            .await;
        }
    }
}

async fn upgrade_sync_loop(s: AppState) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(20));
    loop {
        interval.tick().await;
        let Some(base) = s.cloud_url() else {
            continue;
        };
        let manifest_url = format!("{}/api/v1/upgrade/manifest", base.trim_end_matches('/'));
        let req = authenticated_request(&s, s.http.get(&manifest_url));
        let resp = match req.send().await {
            Ok(r) if r.status().is_success() => r,
            Ok(_r) => {
                continue;
            }
            Err(_) => {
                continue;
            }
        };

        if let Ok(manifest) = resp.json::<charging_space_edge::UpgradeManifest>().await {
            let cur_ver = s.upgrade_manager.get_current_version();
            if manifest.version > cur_ver {
                println!(
                    "[OTA] 巡检发现云端发布新版本 v{} (当前运行 v{})",
                    manifest.version, cur_ver
                );
                execute_ota_upgrade(&s, &manifest).await;
            }
        }
    }
}

async fn heartbeat_loop(s: AppState) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(10));
    loop {
        interval.tick().await;
        let Some(base) = s.cloud_url() else {
            s.cloud_connected.store(false, Ordering::Relaxed);
            continue;
        };
        let node_id = s.node_id();
        let url = format!("{}/api/v1/nodes/{}/heartbeat", base.trim_end_matches('/'), node_id);
        let snap = s.metrics.snapshot();
        let cur_version_str = format!("v{}", s.upgrade_manager.get_current_version());
        let ip_address = resolve_primary_ip(&s);
        let payload = serde_json::json!({
            "nodeId": node_id,
            "ipAddress": ip_address,
            "uptimeSeconds": snap.uptime_seconds,
            "framesProcessed": snap.frames_processed_total,
            "violationsDetected": snap.violations_detected_total,
            "alarmsTriggered": snap.alarms_triggered_total,
            "sqlitePending": snap.sqlite_unuploaded_count,
            "diskWatermark": snap.disk.watermark_percent,
            "diskUsedBytes": snap.disk.used_bytes,
            "diskTotalBytes": snap.disk.total_bytes,
            "cpuUsage": snap.cpu_usage,
            "memoryUsage": snap.memory_usage,
            "temperature": snap.temperature,
            "version": cur_version_str
        });

        match authenticated_json_request(&s, s.http.post(&url), &payload) {
            Ok(req) => match req.send().await {
                Ok(resp) => {
                    let ok = resp.status().is_success();
                    s.cloud_connected.store(ok, Ordering::Relaxed);
                    if let Ok(body) = resp.json::<serde_json::Value>().await {
                        // 检查云端心跳是否带回待执行 OTA 升级任务
                        if let Some(pending) = body.get("pendingUpgrade") {
                            if !pending.is_null() {
                                if let Ok(manifest) =
                                    serde_json::from_value::<charging_space_edge::UpgradeManifest>(
                                        pending.clone(),
                                    )
                                {
                                    let s_clone = s.clone();
                                    tokio::spawn(async move {
                                        execute_ota_upgrade(&s_clone, &manifest).await;
                                    });
                                }
                            }
                        }
                        if body
                            .get("pendingReboot")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false)
                        {
                            println!(
                                "[SYSTEM] 收到云端远程重启指令，将于 2 秒后优雅退出以由守护进程拉起"
                            );
                            let wd = s.watchdog.clone();
                            tokio::spawn(async move {
                                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                                wd.stop();
                                std::process::exit(0);
                            });
                        }
                    }
                }
                Err(error) => {
                    s.cloud_connected.store(false, Ordering::Relaxed);
                    eprintln!("heartbeat failed: {}", error);
                }
            },
            Err(e) => {
                s.cloud_connected.store(false, Ordering::Relaxed);
                eprintln!("heartbeat serialize failed: {}", e);
                let _ = authenticated_request(&s, s.http.post(&url)).send().await;
            }
        }
        if let Some(report) = s.last_configuration_report.read().await.clone() {
            let report_url = format!(
                "{}/api/v1/nodes/{}/configuration-status",
                base.trim_end_matches('/'),
                node_id
            );
            if let Ok(req) = authenticated_json_request(&s, s.http.post(report_url), &report) {
                if let Err(error) = req.send().await {
                    eprintln!("configuration status retry failed: {}", error);
                }
            }
        }
    }
}

/// 启动 GB28181 RTP 流接收器，为每个绑定到 GB28181 设备的通道接收视频流
async fn start_gb28181_stream_receivers(
    s: AppState,
    grabber: Arc<charging_space_edge::pipeline::FrameGrabber>,
    registry: Arc<charging_space_edge::gb28181::Gb28181DeviceRegistry>,
) {
    // 等待设备注册完成，然后为每个在线设备启动 RTP 流接收
    let mut check_interval = tokio::time::interval(std::time::Duration::from_secs(5));

    loop {
        check_interval.tick().await;

        let config = s.configuration.read().await.clone();
        let online_devices = registry.list_online_devices().await;

        for device in &online_devices {
            // 查找设备对应的绑定通道
            for binding in &config.bindings {
                if !binding.enabled {
                    continue;
                }

                // 检查摄像头是否匹配此 GB28181 设备
                if let Ok(cameras) = s.cameras.snapshot() {
                    if cameras.iter().any(|c| {
                        c.camera_id == binding.camera_id && c.gb_device_id == device.device_id
                    }) {
                        // 为此设备启动 RTP 流接收
                        let rtp_port = 15000 + binding.relay_channel;
                        let ssrc = format!("{:010X}", device.device_id.parse::<u16>().unwrap_or(1));
                        let grabber = grabber.clone();

                        let channel_id = binding.camera_id.parse::<u8>().unwrap_or(0);
                        let space_id = binding.space_id.clone();

                        // 检查是否已为此设备启动接收器
                        let device_key = format!("gb28181_{}", device.device_id);
                        let is_active = s.decoder_active_keys.read().await.contains(&device_key);
                        if !is_active {
                            s.decoder_active_keys.write().await.insert(device_key.clone());

                            let dev_id = device.device_id.clone();
                            let ssrc_val = ssrc.clone();

                            tokio::spawn(async move {
                                match charging_space_edge::gb28181::Gb28181StreamReceiver::bind(
                                    rtp_port,
                                    &ssrc_val,
                                )
                                .await
                                {
                                    Ok(receiver) => {
                                        println!(
                                            "GB28181 RTP receiver started for device {} on port {}",
                                            dev_id, rtp_port
                                        );
                                        let (tx, mut rx) = tokio::sync::mpsc::channel::<
                                            charging_space_edge::gb28181::rtp_ps::DecodedVideoFrame,
                                        >(16);
                                        receiver.start_receiving(tx);

                                        while let Some(frame) = rx.recv().await {
                                            let video_frame =
                                                charging_space_edge::pipeline::VideoFrame {
                                                    channel_id,
                                                    space_id: space_id.clone(),
                                                    timestamp: Utc::now(),
                                                    width: frame.width,
                                                    height: frame.height,
                                                    raw_data: frame.frame_data,
                                                    is_simulated: false,
                                                    data_source: "GB28181_DECODED".to_string(),
                                                };
                                            grabber.push_decoded_frame(channel_id, video_frame);
                                        }
                                    }
                                    Err(e) => {
                                        eprintln!(
                                            "GB28181 RTP receiver bind failed for device {}: {}",
                                            dev_id, e
                                        );
                                    }
                                }
                            });
                        }
                    }
                }
            }
        }
    }
}

/// 启动 RTSP 拉流客户端，为每个配置了 RTSP URL 的通道建立连接
async fn start_rtsp_clients(s: AppState, grabber: Arc<charging_space_edge::pipeline::FrameGrabber>) {
    let config = s.configuration.read().await.clone();

    for binding in &config.bindings {
        if !binding.enabled {
            continue;
        }

        // 检查摄像头是否配置了 RTSP URL（通过摄像头注册信息）
        if let Ok(cameras) = s.cameras.snapshot() {
            if let Some(camera) = cameras.iter().find(|c| c.camera_id == binding.camera_id) {
                if let Some(rtsp_url) = &camera.stream_url {
                    if !rtsp_url.is_empty() {
                        let url = rtsp_url.clone();
                        let space_id = binding.space_id.clone();
                        let grabber = grabber.clone();

                        let ch_id = binding.camera_id.parse::<u8>().unwrap_or(0);
                        tokio::spawn(async move {
                            let client = charging_space_edge::rtsp::RtspClient::new(
                                charging_space_edge::rtsp::RtspClientConfig {
                                    rtsp_url: url,
                                    space_id: space_id.clone(),
                                    ..Default::default()
                                },
                            );
                            let (tx, mut rx) = tokio::sync::mpsc::channel::<charging_space_edge::gb28181::rtp_ps::DecodedVideoFrame>(16);
                            client.start(tx);

                            while let Some(frame) = rx.recv().await {
                                // 将解码后的帧推送到 FrameGrabber
                                let video_frame = charging_space_edge::pipeline::VideoFrame {
                                    channel_id: ch_id,
                                    space_id: space_id.clone(),
                                    timestamp: Utc::now(),
                                    width: frame.width,
                                    height: frame.height,
                                    raw_data: frame.frame_data,
                                    is_simulated: false,
                                    data_source: "RTSP_DECODED".to_string(),
                                };
                                grabber.push_decoded_frame(ch_id, video_frame);
                            }
                        });
                    }
                }
            }
        }
    }
}

async fn video_stream_pipeline_loop(
    s: AppState,
    frame_grabber: Arc<charging_space_edge::pipeline::FrameGrabber>,
) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(3));
    let real_engine = Arc::new(charging_space_edge::ai::RealVisualInferenceEngine::new());

    let ai_engine_type = std::env::var("EDGE_AI_ENGINE")
        .unwrap_or_else(|_| "auto".into())
        .to_lowercase();
    let rknn_engine = Arc::new(charging_space_edge::ai::RknnInferenceEngine::new(
        charging_space_edge::ai::RknnConfig::from_env(),
    ));

    loop {
        interval.tick().await;
        if s.vision_mode == "external" {
            if let Ok(changed) = s.cameras.mark_stale_offline(Utc::now(), 45) {
                for cam in changed {
                    let _ = s.sqlite.upsert_camera(&cam);
                }
            }
            s.watchdog.feed("pipeline");
            continue;
        }
        let config = s.configuration.read().await.clone();
        let rules = config.rules.clone();

        for (idx, binding) in config.bindings.iter().enumerate() {
            if !binding.enabled {
                continue;
            }

            let space_id = &binding.space_id;
            s.metrics.record_frame();

            // 严禁任何 Mock 假数据与假车牌：只接受真实摄相机解码帧，无流或离线时车位严格为空闲
            let (detection, is_simulated, stream_type) = match frame_grabber.grab_frame(idx as u8) {
                Ok(frame_obj) if !frame_obj.is_simulated => {
                    if ai_engine_type == "rknn" && rknn_engine.is_hardware_active() {
                        let det = rknn_engine.infer(&frame_obj).unwrap_or_else(|_| {
                            charging_space_edge::pipeline::VehicleDetection {
                                space_id: space_id.clone(),
                                is_occupied: false,
                                plate: None,
                                vehicle_type: charging_space_edge::VehicleType::Unknown,
                                confidence: 0.0,
                                roi_box: [0.0; 4],
                                is_simulated: false,
                                data_source: rknn_engine.engine_name().to_string(),
                            }
                        });
                        let det_res = charging_space_edge::ai::DetectionResult {
                            occupied: det.is_occupied,
                            is_new_energy: det.vehicle_type == charging_space_edge::VehicleType::NewEnergy,
                            plate_number: det.plate,
                            confidence: det.confidence as f64,
                            bbox: [
                                det.roi_box[0] as f64,
                                det.roi_box[1] as f64,
                                det.roi_box[2] as f64,
                                det.roi_box[3] as f64,
                            ],
                            model_version: rknn_engine.engine_name().to_string(),
                            is_simulated: false,
                            data_source: det.data_source,
                        };
                        (det_res, false, "REAL_RKNN_INFERENCE".to_string())
                    } else {
                        let det = real_engine.infer_frame(
                            &frame_obj.raw_data,
                            frame_obj.width,
                            frame_obj.height,
                        );
                        (det, false, "REAL_CAMERA_INFERENCE".to_string())
                    }
                }
                _ => {
                    // 无视频流或摄相机离线：车位空闲，无车辆，严禁伪造任何车辆与车牌
                    (
                        charging_space_edge::ai::DetectionResult {
                            occupied: false,
                            is_new_energy: false,
                            plate_number: None,
                            confidence: 0.0,
                            bbox: [0.0, 0.0, 0.0, 0.0],
                            model_version: "v2.0.0".to_string(),
                            is_simulated: false,
                            data_source: "CAMERA_OFFLINE".to_string(),
                        },
                        false,
                        "CAMERA_OFFLINE".to_string(),
                    )
                }
            };

            let vtype = if !detection.occupied {
                charging_space_edge::VehicleType::Unknown
            } else if detection.is_new_energy {
                charging_space_edge::VehicleType::NewEnergy
            } else {
                charging_space_edge::VehicleType::Fuel
            };

            let effective_plate = if !detection.occupied {
                None
            } else {
                detection.plate_number.clone()
            };

            let raw = charging_space_edge::RawDetection {
                space_id: space_id.clone(),
                has_vehicle: detection.occupied,
                plate: effective_plate,
                vehicle_type: vtype,
                captured_at: Utc::now(),
                evidence_image_path: None,
                is_simulated: false,
                data_source: stream_type.clone(),
                confidence: Some(detection.confidence).filter(|c| *c > 0.0),
                plate_color: None,
                camera_id: Some(binding.camera_id.clone()),
                image_base64: None,
            };

            if let Ok(_transition) = s.trackers.process_detection(&raw, &rules) {
                if let Ok(Some(tr)) = s.trackers.get_tracker(space_id) {
                    let now = Utc::now();
                    let snapshot_url = build_snapshot_url(&s, &binding.site_id, space_id);
                    if tr.phase == charging_space_edge::PresencePhase::Occupied {
                        let occupied_since = tr.confirmed_enter_at.unwrap_or(now);
                        let charger_status = s
                            .chargers
                            .snapshot(space_id)
                            .map(|snap| snap.status)
                            .unwrap_or(charging_space_edge::ChargerStatus::Idle);
                        let plate = tr.candidate_plate.clone().or_else(|| raw.plate.clone());
                        let whitelisted =
                            s.whitelist.contains(plate.as_deref(), now).unwrap_or(false);

                        let input = charging_space_edge::EvaluationInput {
                            space_id: space_id.clone(),
                            plate,
                            vehicle_type: tr.candidate_vehicle_type.clone(),
                            charger_status,
                            occupied_since,
                            charge_completed_at: None,
                            now,
                            whitelisted,
                        };

                        let mut eval_res = charging_space_edge::evaluate(&input, &rules);
                        let is_acknowledged = s
                            .lifecycle
                            .active_event(space_id)
                            .ok()
                            .flatten()
                            .is_some_and(|e| {
                                e.status == charging_space_edge::EventStatus::Acknowledged
                            });
                        let alarming = eval_res.should_alarm && !is_acknowledged;
                        let _ = s.relay.set_alarm(space_id, alarming);

                        if alarming {
                            s.metrics.record_alarm();
                        }
                        if eval_res.decision == Decision::Violation {
                            s.metrics.record_violation();
                        }

                        // 若违规报警且尚未生成现场证据图片，仅当存在真实摄像头 JPEG 抓拍时生成带水印防篡改取证
                        if alarming && tr.last_evidence_path.is_none() {
                            let violation_type =
                                eval_res.violation_type.as_deref().unwrap_or("FUEL_VEHICLE");
                            let plate_str = input.plate.as_deref().unwrap_or("UNKNOWN");
                            let _ = maybe_capture_space_evidence(
                                &s,
                                space_id,
                                plate_str,
                                violation_type,
                                is_simulated,
                                &stream_type,
                                None,
                                None,
                            )
                            .await;
                        }

                        let active_event_id = s
                            .lifecycle
                            .active_event(space_id)
                            .ok()
                            .flatten()
                            .map(|e| e.event_id);
                        let st = charging_space_edge::SpaceRuntimeStatus {
                            node_id: s.node_id(),
                            site_id: binding.site_id.clone(),
                            space_id: space_id.clone(),
                            phase: charging_space_edge::PresencePhase::Occupied,
                            charger_status: input.charger_status.clone(),
                            plate: input.plate.clone(),
                            vehicle_type: input.vehicle_type.clone(),
                            alarming,
                            active_event_id,
                            observed_at: now,
                            is_simulated,
                            stream_type: Some(stream_type.clone()),
                            snapshot_url,
                            confidence: raw.confidence.unwrap_or(0.0),
                        };
                        let _ = s.space_statuses.update(st.clone());
                        let _ = s.sqlite.save_space_status(&st);

                        persist_lifecycle_changes(&s, &input, &mut eval_res).await;
                    } else if tr.phase == charging_space_edge::PresencePhase::Empty {
                        let _ = s.relay.set_alarm(space_id, false);
                        persist_cleared_event(&s, space_id, now).await;
                        let st = charging_space_edge::SpaceRuntimeStatus {
                            node_id: s.node_id(),
                            site_id: binding.site_id.clone(),
                            space_id: space_id.clone(),
                            phase: charging_space_edge::PresencePhase::Empty,
                            charger_status: charging_space_edge::ChargerStatus::Idle,
                            plate: None,
                            vehicle_type: charging_space_edge::VehicleType::Unknown,
                            alarming: false,
                            active_event_id: None,
                            observed_at: now,
                            is_simulated,
                            stream_type: Some(stream_type.clone()),
                            snapshot_url,
                            confidence: 0.0,
                        };
                        let _ = s.space_statuses.update(st.clone());
                        let _ = s.sqlite.save_space_status(&st);
                    }
                }
            }
        }
        // 每个处理轮次结束时主动喂狗，确保视频解码与 AI 推理管道未死锁
        s.watchdog.feed("pipeline");
    }
}

fn authenticated_request(
    state: &AppState,
    request: reqwest::RequestBuilder,
) -> reqwest::RequestBuilder {
    let key = state.edge_api_key();
    let node_id = state.node_id();
    add_edge_api_key_with_body(key.as_deref(), &node_id, request, &[])
}

fn authenticated_json_request<T: serde::Serialize>(
    state: &AppState,
    request: reqwest::RequestBuilder,
    json: &T,
) -> Result<reqwest::RequestBuilder, serde_json::Error> {
    let body_bytes = serde_json::to_vec(json)?;
    let key = state.edge_api_key();
    let node_id = state.node_id();
    let request_with_headers =
        add_edge_api_key_with_body(key.as_deref(), &node_id, request, &body_bytes);
    Ok(request_with_headers
        .header("Content-Type", "application/json")
        .body(body_bytes))
}

fn add_edge_api_key_with_body(
    api_key: Option<&str>,
    node_id: &str,
    request: reqwest::RequestBuilder,
    body: &[u8],
) -> reqwest::RequestBuilder {
    match api_key {
        Some(api_key) => {
            let timestamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs()
                .to_string();
            let nonce = Uuid::new_v4().to_string();
            let body_hash = format!("{:x}", Sha256::digest(body));
            let signature = request
                .try_clone()
                .and_then(|preview| preview.build().ok())
                .and_then(|preview| {
                    sign_edge_request(
                        api_key,
                        &timestamp,
                        &nonce,
                        preview.method().as_str(),
                        preview.url().path(),
                        &body_hash,
                        node_id,
                    )
                });
            match signature {
                Some(signature) => request
                    .header("X-Api-Key", api_key)
                    .header("X-Edge-Timestamp", timestamp)
                    .header("X-Edge-Nonce", nonce)
                    .header("X-Content-SHA256", body_hash)
                    .header("X-Node-Id", node_id)
                    .header("X-Edge-Signature", signature),
                None => request.header("X-Api-Key", api_key),
            }
        }
        None => request,
    }
}

async fn require_local_api_key(
    State(state): State<AppState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let local_key = state.local_api_key();
    let Some(expected) = local_key.as_deref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": "edge_management_api_disabled"})),
        )
            .into_response();
    };
    let actual = request
        .headers()
        .get("X-Edge-Local-Key")
        .and_then(|value| value.to_str().ok());
    if !constant_time_key_equals(actual, expected) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "unauthorized"})),
        )
            .into_response();
    }
    next.run(request).await
}

fn constant_time_key_equals(actual: Option<&str>, expected: &str) -> bool {
    let Some(actual) = actual else {
        return false;
    };
    let mut expected_mac = match Hmac::<Sha256>::new_from_slice(expected.as_bytes()) {
        Ok(mac) => mac,
        Err(_) => return false,
    };
    expected_mac.update(b"edge-local-management-key");
    let mut actual_mac = match Hmac::<Sha256>::new_from_slice(actual.as_bytes()) {
        Ok(mac) => mac,
        Err(_) => return false,
    };
    actual_mac.update(b"edge-local-management-key");
    expected_mac
        .verify_slice(&actual_mac.finalize().into_bytes())
        .is_ok()
}

async fn get_metrics(State(s): State<AppState>) -> String {
    s.metrics
        .set_sqlite_unuploaded(s.sqlite.pending_count().unwrap_or(0) as u64);
    s.metrics.export_prometheus_text()
}

async fn get_telemetry(State(s): State<AppState>) -> Json<TelemetrySnapshot> {
    s.metrics
        .set_sqlite_unuploaded(s.sqlite.pending_count().unwrap_or(0) as u64);
    Json(s.metrics.snapshot())
}

async fn get_upgrade_report(State(s): State<AppState>) -> Json<UpgradeReport> {
    Json(s.upgrade_manager.get_report())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpgradeApplyRequest {
    manifest: UpgradeManifest,
    #[serde(default)]
    package_hex: String,
}

async fn post_upgrade_apply(
    State(s): State<AppState>,
    Json(payload): Json<UpgradeApplyRequest>,
) -> Result<Json<UpgradeReport>, (StatusCode, String)> {
    // 支持两种固件获取模式：优先支持通过 package_url 远端流式下载，若提供 package_hex 则使用内联固件
    let pkg_bytes = if !payload.package_hex.is_empty() {
        decode_hex_str(&payload.package_hex).map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                format!("Invalid packageHex: {}", e),
            )
        })?
    } else if !payload.manifest.package_url.is_empty() {
        println!("[OTA] 开始从云端下载升级包: {}", payload.manifest.package_url);
        let resp = s
            .http
            .get(&payload.manifest.package_url)
            .send()
            .await
            .map_err(|e| (StatusCode::BAD_GATEWAY, format!("下载固件升级包失败: {}", e)))?;
        if !resp.status().is_success() {
            return Err((
                StatusCode::BAD_GATEWAY,
                format!("固件下载服务器返回异常状态码: {}", resp.status()),
            ));
        }
        resp.bytes()
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("读取固件数据流失败: {}", e)))?
            .to_vec()
    } else {
        return Err((
            StatusCode::BAD_REQUEST,
            "缺少固件数据：packageHex 与 packageUrl 不能同时为空".into(),
        ));
    };

    let sql = s.sqlite.clone();
    let rep = s
        .upgrade_manager
        .apply_upgrade(&payload.manifest, &pkg_bytes, move || {
            sql.pending_count().map(|_| ()).map_err(|e| e.to_string())
        })
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;

    // 升级成功且版本已原子提交 (Committed)，安排安全重启延迟任务由守护进程 (systemd/Docker) 拉起新固件
    if rep.state == charging_space_edge::UpgradeState::Committed {
        println!("[OTA] 固件升级已成功提交生效！将在 2 秒后重启进程以载入新程序版本...");
        let wd = s.watchdog.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            // 安全退出看门狗 (Magic Close)，防止重启过渡阶段被内核看门狗硬复位
            wd.stop();
            std::process::exit(0);
        });
    }

    Ok(Json(rep))
}

fn decode_hex_str(s: &str) -> Result<Vec<u8>, String> {
    let clean = s.trim();
    if !clean.len().is_multiple_of(2) {
        return Err("hex length must be even".into());
    }
    (0..clean.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&clean[i..i + 2], 16).map_err(|e| e.to_string()))
        .collect()
}

fn sign_edge_request(
    api_key: &str,
    timestamp: &str,
    nonce: &str,
    method: &str,
    path: &str,
    body_hash: &str,
    node_id: &str,
) -> Option<String> {
    let mut mac = Hmac::<Sha256>::new_from_slice(api_key.as_bytes()).ok()?;
    mac.update(
        format!("{timestamp}\n{nonce}\n{method}\n{path}\n{body_hash}\n{node_id}").as_bytes(),
    );
    Some(
        mac.finalize()
            .into_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use charging_space_edge::{DeviceBinding, WhitelistEntry};
    use uuid::Uuid;

    #[tokio::test]
    async fn configuration_update_rebinds_every_runtime_component() {
        let directory =
            std::env::temp_dir().join(format!("charging-edge-runtime-{}", Uuid::new_v4()));
        let (store, initial) = ConfigurationStore::open(directory.join("configuration.json"))
            .await
            .unwrap();
        let relay: Arc<dyn RelayController> = Arc::new(MemoryRelayController::new());
        relay.apply_bindings(&initial.bindings).unwrap();
        let cameras = Arc::new(VideoChannelManager::new(16));
        cameras.apply_bindings(&initial.bindings).unwrap();
        let chargers = Arc::new(DynamicChargerAdapter::default());
        chargers.apply_bindings(&initial.bindings).unwrap();
        let whitelist = Arc::new(WhitelistRegistry::default());
        let statuses = Arc::new(SpaceStatusRegistry::default());
        statuses
            .apply_bindings("edge-test", &initial.bindings)
            .unwrap();
        let transport: Arc<dyn HardwareTransport> = Arc::new(MockTransport::new("test", "MOCK"));
        let metrics = Arc::new(MetricsCollector::new(
            "edge-test".into(),
            directory.to_str().unwrap().into(),
            10 * 1024 * 1024,
        ));
        let upgrade_manager = Arc::new(RemoteUpgradeManager::new(
            1,
            "test-secret".into(),
            directory.join("test_bin"),
        ));
        let state = AppState {
            configuration: Arc::new(tokio::sync::RwLock::new(initial)),
            relay,
            system_config: Arc::new(std::sync::RwLock::new(NodeSystemConfig {
                node_id: "edge-test".into(),
                cloud_url: None,
                edge_api_key: None,
                local_api_key: None,
                lan_ip: None,
                lan_subnet: None,
                lan_gateway: None,
                lan_dns: None,
            })),
            data_directory: directory.clone(),
            node_id: "edge-test".into(),
            http: reqwest::Client::new(),
            upload_lock: Arc::new(tokio::sync::Mutex::new(())),
            configuration_apply_lock: Arc::new(tokio::sync::Mutex::new(())),
            configuration_store: Arc::new(store),
            cameras,
            trackers: Arc::new(SpaceTrackerRegistry::new()),
            chargers,
            sqlite: Arc::new(SqliteStorage::open(directory.join("edge.db")).unwrap()),
            watchdog: Arc::new(WatchdogMonitor::default()),
            hardware_monitor: Arc::new(HardwareHealthMonitor::new(transport)),
            whitelist,
            lifecycle: Arc::new(EventLifecycleRegistry::default()),
            space_statuses: statuses,
            last_configuration_report: Arc::new(tokio::sync::RwLock::new(None)),
            metrics,
            upgrade_manager,
            evidence_storage: Arc::new(EvidenceStorage::new(directory.join("evidence"))),
            decoder_active_keys: Arc::new(tokio::sync::RwLock::new(
                std::collections::HashSet::new(),
            )),
            cloud_connected: Arc::new(AtomicBool::new(false)),
            vision_mode: "external".into(),
            ai_engine: "python-hyperlpr3".into(),
            last_snapshot_upload: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        };
        let now = Utc::now();
        let configuration = PlatformConfiguration {
            version: 1,
            updated_at: now,
            rules: RuleConfig::default(),
            bindings: vec![DeviceBinding {
                site_id: "site-2".into(),
                space_id: "B-01".into(),
                camera_id: "camera-B-01".into(),
                charger_id: "charger-B-01".into(),
                relay_channel: 7,
                speaker_channel: None,
                enabled: true,
                ..Default::default()
            }],
            whitelist: vec![WhitelistEntry {
                plate: "粤A12345".into(),
                vehicle_type: None,
                reason: Some("test".into()),
                added_at: now,
                expires_at: None,
                enabled: true,
            }],
            cameras: Vec::new(),
        };
        apply_configuration(&state, configuration).await.unwrap();
        assert_eq!(state.configuration.read().await.version, 1);
        assert_eq!(state.cameras.snapshot().unwrap()[0].space_id, "B-01");
        assert_eq!(
            state.chargers.snapshot("B-01").unwrap().charger_id,
            "charger-B-01"
        );
        assert_eq!(state.relay.detailed_snapshot().unwrap()["B-01"].channel, 7);
        assert!(state.whitelist.contains(Some("粤a12345"), now).unwrap());
        assert_eq!(state.space_statuses.snapshot().unwrap()[0].space_id, "B-01");

        let stale = state.configuration.read().await.clone();
        assert!(apply_configuration(&state, stale).await.is_err());
        drop(state);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn cloud_request_uses_edge_api_key_when_configured() {
        let client = reqwest::Client::new();
        let request = client.get("http://127.0.0.1:1").header("X-Test", "value");
        let built = add_edge_api_key_with_body(Some("edge-secret"), "edge-test", request, &[])
            .build()
            .unwrap();
        assert_eq!(
            built.headers()["X-Api-Key"].to_str().unwrap(),
            "edge-secret"
        );
        assert!(built.headers().contains_key("X-Edge-Timestamp"));
        assert!(built.headers().contains_key("X-Edge-Signature"));
    }

    #[test]
    fn hmac_signature_is_deterministic_and_well_formed() {
        let sig = sign_edge_request(
            "edge-secret",
            "1700000000",
            "test-nonce",
            "POST",
            "/api/v1/events",
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            "edge-test",
        )
        .unwrap();
        // HMAC-SHA256 produces 32 bytes = 64 hex characters
        assert_eq!(sig.len(), 64);
        assert!(sig.chars().all(|c| c.is_ascii_hexdigit()));
        // Same inputs must produce the same signature (deterministic)
        let sig2 = sign_edge_request(
            "edge-secret",
            "1700000000",
            "test-nonce",
            "POST",
            "/api/v1/events",
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            "edge-test",
        )
        .unwrap();
        assert_eq!(sig, sig2);
        // Different nonce must produce a different signature
        let sig3 = sign_edge_request(
            "edge-secret",
            "1700000000",
            "different-nonce",
            "POST",
            "/api/v1/events",
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            "edge-test",
        )
        .unwrap();
        assert_ne!(sig, sig3);
    }
}
