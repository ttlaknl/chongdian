use axum::{
    extract::{Path, State},
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use charging_space_edge::{
    evaluate, CameraChannel, ChargerAdapter, ChargerSnapshot, ChargerStatus, CloudEvent,
    ConfigurationStore, Decision, DynamicChargerAdapter, EvaluationInput, EventSpool,
    ExternalChargerUpdate, MemoryRelayController, ModbusTcpRelayController, OperatorApiConfig,
    OperatorCloudClient, PlatformConfiguration, PresencePhase, RawDetection, RelayChannelStatus,
    RelayController, RuleConfig, SpacePresenceTracker, SpaceTrackerRegistry, SqliteStorage,
    TransitionEvent, VideoChannelManager,
};
use chrono::Utc;
use serde::Serialize;
use std::{collections::HashMap, path::PathBuf, sync::Arc};

#[derive(Clone)]
struct AppState {
    configuration: Arc<tokio::sync::RwLock<PlatformConfiguration>>,
    relay: Arc<dyn RelayController>,
    cloud_url: Option<String>,
    node_id: String,
    http: reqwest::Client,
    event_spool: Arc<EventSpool>,
    upload_lock: Arc<tokio::sync::Mutex<()>>,
    configuration_store: Arc<ConfigurationStore>,
    cameras: Arc<VideoChannelManager>,
    trackers: Arc<SpaceTrackerRegistry>,
    chargers: Arc<DynamicChargerAdapter>,
    sqlite: Arc<SqliteStorage>,
}
#[derive(Serialize)]
struct Health {
    status: &'static str,
    max_video_channels: u8,
    node_id: String,
    pending_events: usize,
    configuration_version: u64,
    configured_spaces: usize,
    camera_channels: usize,
    charger_channels: usize,
}

const MAX_PENDING_EVENTS: usize = 10_000;

#[tokio::main]
async fn main() {
    let data_directory = std::env::var("EDGE_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("data"));
    let sqlite_storage = SqliteStorage::open(data_directory.join("edge.db"))
        .expect("open sqlite storage");
    let event_spool = EventSpool::open(data_directory.join("events"), MAX_PENDING_EVENTS)
        .await
        .expect("open event spool");
    let (configuration_store, initial_configuration) =
        ConfigurationStore::open(data_directory.join("configuration.json"))
            .await
            .expect("open rule configuration");

    let relay_controller: Arc<dyn RelayController> = if let Ok(addr_str) = std::env::var("EDGE_MODBUS_TCP_ADDR") {
        match addr_str.parse::<std::net::SocketAddr>() {
            Ok(addr) => {
                println!("initializing Modbus TCP relay at {}", addr);
                let controller = ModbusTcpRelayController::new(addr, 1);
                for binding in &initial_configuration.bindings {
                    if binding.enabled {
                        let _ = controller.bind_space_channel(&binding.space_id, binding.relay_channel);
                    }
                }
                Arc::new(controller)
            }
            Err(e) => {
                eprintln!("invalid EDGE_MODBUS_TCP_ADDR {}: {}, falling back to memory relay", addr_str, e);
                let mem = MemoryRelayController::new();
                for binding in &initial_configuration.bindings {
                    if binding.enabled {
                        let _ = mem.register_space(&binding.space_id, binding.relay_channel);
                    }
                }
                Arc::new(mem)
            }
        }
    } else {
        let mem = MemoryRelayController::new();
        for binding in &initial_configuration.bindings {
            if binding.enabled {
                let _ = mem.register_space(&binding.space_id, binding.relay_channel);
            }
        }
        Arc::new(mem)
    };

    let chargers = Arc::new(DynamicChargerAdapter::default());
    chargers
        .apply_bindings(&initial_configuration.bindings)
        .expect("apply initial charger bindings");

    let state = AppState {
        configuration: Arc::new(tokio::sync::RwLock::new(initial_configuration)),
        relay: relay_controller,
        cloud_url: std::env::var("EDGE_CLOUD_URL")
            .ok()
            .filter(|v| !v.is_empty()),
        node_id: std::env::var("EDGE_NODE_ID").unwrap_or_else(|_| "edge-local-01".into()),
        http: reqwest::Client::new(),
        event_spool: Arc::new(event_spool),
        upload_lock: Arc::new(tokio::sync::Mutex::new(())),
        configuration_store: Arc::new(configuration_store),
        cameras: Arc::new(VideoChannelManager::new(16)),
        trackers: Arc::new(SpaceTrackerRegistry::new()),
        chargers: chargers.clone(),
        sqlite: Arc::new(sqlite_storage),
    };
    let heartbeat_state = state.clone();
    tokio::spawn(async move {
        heartbeat_loop(heartbeat_state).await;
    });
    let upload_state = state.clone();
    tokio::spawn(async move {
        upload_loop(upload_state).await;
    });
    let config_state = state.clone();
    tokio::spawn(async move {
        configuration_sync_loop(config_state).await;
    });
    let alarm_protect_state = state.clone();
    tokio::spawn(async move {
        alarm_protection_loop(alarm_protect_state).await;
    });
    let charger_scan_state = state.clone();
    tokio::spawn(async move {
        charger_offline_detection_loop(charger_scan_state).await;
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
    let initial_configuration = state.configuration.read().await.clone();
    state
        .cameras
        .apply_bindings(&initial_configuration.bindings)
        .expect("apply initial camera bindings");
    let app = Router::new()
        .route("/health", get(health))
        .route("/api/v1/rules", get(get_rules).put(put_rules))
        .route(
            "/api/v1/configuration",
            get(get_configuration).put(put_configuration),
        )
        .route("/api/v1/cameras", get(camera_states).post(register_camera))
        .route(
            "/api/v1/cameras/:camera_id/heartbeat",
            post(camera_heartbeat),
        )
        .route("/api/v1/chargers", get(list_chargers))
        .route(
            "/api/v1/chargers/snapshots",
            post(update_charger_snapshot),
        )
        .route("/api/v1/spaces/:space_id/evaluate", post(evaluate_space))
        .route(
            "/api/v1/spaces/:space_id/detections",
            post(handle_detection),
        )
        .route("/api/v1/spaces/:space_id/tracker", get(get_space_tracker))
        .route("/api/v1/relays", get(relay_states))
        .route("/api/v1/relays/details", get(relay_details))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("0.0.0.0:8081").await.unwrap();
    println!("edge node listening on {}", listener.local_addr().unwrap());
    axum::serve(listener, app).await.unwrap();
}
async fn health(State(s): State<AppState>) -> Json<Health> {
    let charger_count = s.chargers.list_snapshots().map(|v| v.len()).unwrap_or(0);
    let pending = s.sqlite.pending_count().unwrap_or(0);
    Json(Health {
        status: "UP",
        max_video_channels: 16,
        node_id: s.node_id.clone(),
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
async fn alarm_protection_loop(s: AppState) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
    loop {
        interval.tick().await;
        let limit = {
            let cfg = s.configuration.read().await;
            cfg.rules.alarm_auto_stop_seconds
        };
        let _ = s.relay.tick_timeout_protection(limit);
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
async fn register_camera(
    State(s): State<AppState>,
    Json(camera): Json<CameraChannel>,
) -> Result<Json<CameraChannel>, (StatusCode, String)> {
    s.cameras
        .register(camera.clone())
        .map_err(|error| (StatusCode::BAD_REQUEST, error))?;
    Ok(Json(camera))
}
async fn camera_heartbeat(
    Path(camera_id): Path<String>,
    State(s): State<AppState>,
) -> Result<Json<CameraChannel>, (StatusCode, String)> {
    s.cameras
        .heartbeat(&camera_id, Utc::now())
        .map(Json)
        .map_err(|error| (StatusCode::NOT_FOUND, error))
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
    let result = {
        let configuration = s.configuration.read().await;
        evaluate(&input, &configuration.rules)
    };
    s.relay
        .set_alarm(&space_id, result.should_alarm)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    if result.decision == Decision::Violation {
        upload_event(&s, &input, &result).await;
    }
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
    s.chargers
        .update(update.clone())
        .map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    Ok(Json(update))
}

async fn charger_offline_detection_loop(s: AppState) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(10));
    loop {
        interval.tick().await;
        if let Ok(degraded) = s.chargers.tick_offline_detection() {
            for space_id in degraded {
                eprintln!("charger for space {} offline, soft-degraded", space_id);
            }
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

async fn handle_detection(
    Path(space_id): Path<String>,
    State(s): State<AppState>,
    Json(mut detection): Json<RawDetection>,
) -> Result<Json<DetectionResponse>, (StatusCode, String)> {
    detection.space_id = space_id.clone();
    let rules = s.configuration.read().await.rules.clone();
    let transition = s
        .trackers
        .process_detection(&detection, &rules)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;

    let tracker = s
        .trackers
        .get_tracker(&space_id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?
        .unwrap_or_else(|| SpacePresenceTracker::new(space_id.clone()));

    let mut evaluation_result = None;

    match &transition {
        TransitionEvent::VehicleLeft { .. } => {
            // 车辆确认离场，立即关闭对应车位继电器消警
            s.relay
                .set_alarm(&space_id, false)
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
        }
        TransitionEvent::VehicleEntered { .. } | TransitionEvent::None => {
            if tracker.phase == PresencePhase::Occupied || tracker.phase == PresencePhase::Leaving {
                let now = detection.captured_at;
                // 深度联动充电桩实时状态
                let charger_snapshot = s.chargers.snapshot(&space_id).ok();
                let charger_status = charger_snapshot
                    .as_ref()
                    .map(|c| c.status.clone())
                    .unwrap_or(ChargerStatus::Unknown);
                let completed_at = charger_snapshot.as_ref().and_then(|c| c.completed_at);

                if let Some(eval_input) = tracker.build_evaluation_input(
                    now,
                    charger_status,
                    completed_at,
                    false,
                ) {
                    let result = evaluate(&eval_input, &rules);
                    s.relay
                        .set_alarm(&space_id, result.should_alarm)
                        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
                    if result.decision == Decision::Violation {
                        upload_event(&s, &eval_input, &result).await;
                    }
                    evaluation_result = Some(result);
                }
            }
        }
    }

    let transition_str = match transition {
        TransitionEvent::None => "NONE",
        TransitionEvent::VehicleEntered { .. } => "VEHICLE_ENTERED",
        TransitionEvent::VehicleLeft { .. } => "VEHICLE_LEFT",
    };

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

async fn upload_event(
    s: &AppState,
    input: &EvaluationInput,
    result: &charging_space_edge::EvaluationResult,
) {
    let evidence_url = s
        .trackers
        .get_tracker(&input.space_id)
        .ok()
        .flatten()
        .and_then(|t| t.last_evidence_path);

    let event = CloudEvent {
        event_id: result.event_id,
        space_id: input.space_id.clone(),
        plate: input.plate.clone(),
        vehicle_type: input.vehicle_type.clone(),
        violation_type: result.violation_type.clone(),
        occurred_at: input.now,
        evidence_url,
    };
    if let Err(error) = s.sqlite.save_event(&event) {
        eprintln!("event {} could not be saved to sqlite: {}", event.event_id, error);
    }
    let _ = s.event_spool.enqueue(&event).await;
    if let Some(base) = &s.cloud_url {
        flush_pending(s, base).await;
    }
}

async fn flush_pending(s: &AppState, base: &str) {
    let _guard = s.upload_lock.lock().await;
    let url = format!("{}/api/v1/events", base.trim_end_matches('/'));
    
    // 优先从 SQLite 获取待同步事件
    if let Ok(pending_events) = s.sqlite.fetch_pending_events(20) {
        for event in pending_events {
            let delivered = match tokio::time::timeout(
                std::time::Duration::from_secs(3),
                s.http.post(&url).json(&event).send(),
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
                println!("event {} uploaded to cloud by {}", event.event_id, s.node_id);
                let _ = s.sqlite.mark_event_synced(&event.event_id);
            } else {
                return;
            }
        }
    }
}

async fn upload_loop(s: AppState) {
    let Some(base) = s.cloud_url.clone() else {
        return;
    };
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
    loop {
        interval.tick().await;
        flush_pending(&s, &base).await;
    }
}

async fn apply_configuration(
    s: &AppState,
    configuration: PlatformConfiguration,
) -> Result<(), (StatusCode, String)> {
    configuration
        .validate()
        .map_err(|error| (StatusCode::BAD_REQUEST, error))?;
    let mut current = s.configuration.write().await;
    if configuration.version < current.version {
        return Err((
            StatusCode::CONFLICT,
            format!(
                "configuration version {} is older than current version {}",
                configuration.version, current.version
            ),
        ));
    }
    s.configuration_store
        .save(&configuration)
        .await
        .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?;
    s.cameras
        .apply_bindings(&configuration.bindings)
        .map_err(|error| (StatusCode::BAD_REQUEST, error))?;
    *current = configuration;
    Ok(())
}

async fn configuration_sync_loop(s: AppState) {
    let Some(base) = s.cloud_url.clone() else {
        return;
    };
    let url = format!("{}/api/v1/configuration", base.trim_end_matches('/'));
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(15));
    loop {
        interval.tick().await;
        let response =
            match tokio::time::timeout(std::time::Duration::from_secs(5), s.http.get(&url).send())
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
                Ok(()) => println!("configuration updated to version {}", incoming_version),
                Err((_, error)) => eprintln!("configuration apply failed: {}", error),
            }
        }
    }
}

async fn heartbeat_loop(s: AppState) {
    let Some(base) = s.cloud_url.clone() else {
        return;
    };
    let url = format!(
        "{}/api/v1/nodes/{}/heartbeat",
        base.trim_end_matches('/'),
        s.node_id
    );
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
    loop {
        interval.tick().await;
        if let Err(error) = s.http.post(&url).send().await {
            eprintln!("heartbeat failed: {}", error);
        }
    }
}
