use crate::{
    CameraChannel, CameraState, ChargerSnapshot, ChargerStatus, CloudEvent, EventStatus,
    PlatformConfiguration, SpaceRuntimeStatus, StalenessStatus, VehicleType,
};
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, Row};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecognitionRecord {
    pub record_id: String,
    pub space_id: String,
    pub camera_id: String,
    pub plate: Option<String>,
    pub plate_color: String,
    pub vehicle_type: VehicleType,
    pub confidence: f64,
    pub snapshot_url: Option<String>,
    pub direction: String,
    pub judgment_status: String,
    pub is_simulated: bool,
    pub recognized_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingEvidenceUpload {
    pub file_name: String,
    pub local_path: String,
    pub space_id: String,
    pub event_id: Option<String>,
}

const SCHEMA_SQL: &str = "
    CREATE TABLE IF NOT EXISTS violation_events (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        event_id TEXT UNIQUE NOT NULL,
        space_id TEXT NOT NULL,
        plate TEXT,
        vehicle_type TEXT NOT NULL,
        violation_type TEXT NOT NULL,
        occurred_at TEXT NOT NULL,
        evidence_url TEXT,
        session_id TEXT NOT NULL DEFAULT '',
        status TEXT NOT NULL DEFAULT 'ALARMING',
        cleared_at TEXT,
        updated_at TEXT,
        synced INTEGER DEFAULT 0,
        retry_count INTEGER DEFAULT 0,
        created_at TEXT NOT NULL
    );

    CREATE INDEX IF NOT EXISTS idx_events_synced ON violation_events(synced, occurred_at);

    CREATE TABLE IF NOT EXISTS presence_logs (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        space_id TEXT NOT NULL,
        plate TEXT,
        vehicle_type TEXT NOT NULL,
        phase TEXT NOT NULL,
        event_type TEXT NOT NULL,
        observed_at TEXT NOT NULL
    );

    CREATE INDEX IF NOT EXISTS idx_presence_space_time ON presence_logs(space_id, observed_at);

    CREATE TABLE IF NOT EXISTS platform_config_state (
        id INTEGER PRIMARY KEY CHECK(id = 1),
        version INTEGER NOT NULL,
        config_json TEXT NOT NULL,
        updated_at TEXT NOT NULL
    );

    CREATE TABLE IF NOT EXISTS node_system_config (
        id INTEGER PRIMARY KEY CHECK(id = 1),
        config_json TEXT NOT NULL,
        updated_at TEXT NOT NULL
    );

    CREATE TABLE IF NOT EXISTS cameras_state (
        camera_id TEXT PRIMARY KEY,
        space_id TEXT NOT NULL,
        protocol TEXT NOT NULL DEFAULT 'RTSP',
        stream_url TEXT,
        gb_device_id TEXT NOT NULL,
        roi_json TEXT,
        state TEXT NOT NULL,
        last_heartbeat TEXT
    );

    CREATE TABLE IF NOT EXISTS charger_snapshots (
        space_id TEXT PRIMARY KEY,
        charger_id TEXT NOT NULL,
        status TEXT NOT NULL,
        session_id TEXT,
        started_at TEXT,
        completed_at TEXT,
        observed_at TEXT NOT NULL,
        provider TEXT NOT NULL,
        raw_timestamp TEXT,
        staleness_json TEXT NOT NULL,
        is_simulated INTEGER NOT NULL DEFAULT 0
    );

    CREATE TABLE IF NOT EXISTS space_runtime_state (
        space_id TEXT PRIMARY KEY,
        status_json TEXT NOT NULL,
        updated_at TEXT NOT NULL
    );

    CREATE TABLE IF NOT EXISTS recognition_records (
        record_id TEXT PRIMARY KEY,
        space_id TEXT NOT NULL,
        camera_id TEXT NOT NULL,
        plate TEXT,
        plate_color TEXT NOT NULL,
        vehicle_type TEXT NOT NULL,
        confidence REAL NOT NULL,
        snapshot_url TEXT,
        direction TEXT NOT NULL,
        judgment_status TEXT NOT NULL,
        is_simulated INTEGER NOT NULL DEFAULT 0,
        recognized_at TEXT NOT NULL,
        synced INTEGER NOT NULL DEFAULT 0
    );

    CREATE INDEX IF NOT EXISTS idx_rec_synced ON recognition_records(synced, recognized_at);

    CREATE TABLE IF NOT EXISTS pending_evidence_uploads (
        file_name TEXT PRIMARY KEY,
        local_path TEXT NOT NULL,
        space_id TEXT NOT NULL,
        event_id TEXT,
        synced INTEGER NOT NULL DEFAULT 0,
        created_at TEXT NOT NULL
    );
";

/// 工业级边缘 SQLite 嵌入式存储引擎（WAL 事务日志模式，防意外断电）
pub struct SqliteStorage {
    db_path: PathBuf,
    conn: Mutex<Connection>,
}

impl SqliteStorage {
    pub fn path(&self) -> &Path {
        &self.db_path
    }

    /// 打开或创建 SQLite 数据库，并应用 WAL 模式与全量模式初始化
    pub fn open(path: impl AsRef<Path>) -> Result<Self, String> {
        let db_path = path.as_ref().to_path_buf();
        if let Some(parent) = db_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }

        let conn = Connection::open(&db_path)
            .map_err(|e| format!("无法打开 SQLite 数据库 {:?}: {}", db_path, e))?;

        conn.execute_batch(
            "
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous = NORMAL;
            PRAGMA busy_timeout = 5000;
            ",
        )
        .map_err(|e| format!("配置 SQLite PRAGMA 失败: {}", e))?;

        conn.execute_batch(SCHEMA_SQL)
            .map_err(|e| format!("初始化 SQLite 数据库表结构失败: {}", e))?;

        for migration in [
            "ALTER TABLE violation_events ADD COLUMN session_id TEXT NOT NULL DEFAULT ''",
            "ALTER TABLE violation_events ADD COLUMN status TEXT NOT NULL DEFAULT 'ALARMING'",
            "ALTER TABLE violation_events ADD COLUMN cleared_at TEXT",
            "ALTER TABLE violation_events ADD COLUMN updated_at TEXT",
        ] {
            let _ = conn.execute(migration, []);
        }

        Ok(Self {
            db_path,
            conn: Mutex::new(conn),
        })
    }

    /// 在内存中创建 SQLite 存储（用于单元测试和快速模拟）
    pub fn in_memory() -> Result<Self, String> {
        let conn = Connection::open_in_memory()
            .map_err(|e| format!("无法创建内存 SQLite 数据库: {}", e))?;

        conn.execute_batch(SCHEMA_SQL)
            .map_err(|e| format!("初始化内存 SQLite 失败: {}", e))?;

        Ok(Self {
            db_path: PathBuf::from(":memory:"),
            conn: Mutex::new(conn),
        })
    }

    /// 违规事件持久化落库（ACID 事务）
    pub fn save_event(&self, event: &CloudEvent) -> Result<(), String> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| "sqlite lock poisoned".to_string())?;

        let v_type_str = serde_json::to_string(&event.vehicle_type)
            .unwrap_or_else(|_| "\"UNKNOWN\"".into())
            .trim_matches('"')
            .to_string();
        let status = serde_json::to_string(&event.status)
            .unwrap_or_else(|_| "\"ALARMING\"".into())
            .trim_matches('"')
            .to_string();

        conn.execute(
            "
            INSERT INTO violation_events (
                event_id, session_id, space_id, plate, vehicle_type, violation_type,
                occurred_at, evidence_url, status, cleared_at, updated_at, synced, created_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 0, ?12)
            ON CONFLICT(event_id) DO UPDATE SET
                session_id = excluded.session_id,
                space_id = excluded.space_id,
                plate = excluded.plate,
                vehicle_type = excluded.vehicle_type,
                violation_type = excluded.violation_type,
                evidence_url = COALESCE(excluded.evidence_url, violation_events.evidence_url),
                status = excluded.status,
                cleared_at = excluded.cleared_at,
                updated_at = excluded.updated_at,
                synced = 0
            ",
            params![
                event.event_id.to_string(),
                event.session_id,
                event.space_id,
                event.plate,
                v_type_str,
                event.violation_type.as_deref().unwrap_or(""),
                event.occurred_at.to_rfc3339(),
                event.evidence_url,
                status,
                event.cleared_at.map(|value| value.to_rfc3339()),
                event.updated_at.to_rfc3339(),
                Utc::now().to_rfc3339()
            ],
        )
        .map_err(|e| format!("保存违规事件至 SQLite 失败: {}", e))?;

        Ok(())
    }

    pub fn update_event_evidence_url(
        &self,
        event_id: &Uuid,
        evidence_url: &str,
    ) -> Result<(), String> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| "sqlite lock poisoned".to_string())?;
        conn.execute(
            "UPDATE violation_events SET evidence_url = ?2, synced = 0, updated_at = ?3 WHERE event_id = ?1",
            params![event_id.to_string(), evidence_url, Utc::now().to_rfc3339()],
        )
        .map_err(|e| format!("更新事件证据链接失败: {}", e))?;
        Ok(())
    }

    /// 获取待上报云端的断网积压违规事件
    pub fn fetch_pending_events(&self, limit: usize) -> Result<Vec<CloudEvent>, String> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| "sqlite lock poisoned".to_string())?;

        let mut stmt = conn
            .prepare(
                "
                SELECT event_id, session_id, space_id, plate, vehicle_type, violation_type,
                       occurred_at, evidence_url, status, cleared_at, COALESCE(updated_at, occurred_at)
                FROM violation_events
                WHERE synced = 0
                ORDER BY id ASC
                LIMIT ?1
                ",
            )
            .map_err(|e| e.to_string())?;

        let event_iter = stmt
            .query_map(params![limit as i64], map_event_row)
            .map_err(|e| e.to_string())?;

        event_iter
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())
    }

    /// 标记事件已成功同步至云端
    pub fn mark_event_synced(&self, event_id: &Uuid) -> Result<(), String> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| "sqlite lock poisoned".to_string())?;

        conn.execute(
            "UPDATE violation_events SET synced = 1 WHERE event_id = ?1",
            params![event_id.to_string()],
        )
        .map_err(|e| format!("更新事件同步标记失败: {}", e))?;

        Ok(())
    }

    /// 待同步事件计数
    pub fn pending_count(&self) -> Result<usize, String> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| "sqlite lock poisoned".to_string())?;

        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM violation_events WHERE synced = 0",
                [],
                |row| row.get(0),
            )
            .unwrap_or(0);

        Ok(count as usize)
    }

    pub fn active_events(&self) -> Result<Vec<CloudEvent>, String> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| "sqlite lock poisoned".to_string())?;
        let mut stmt = conn
            .prepare(
                "SELECT event_id, session_id, space_id, plate, vehicle_type, violation_type,
                        occurred_at, evidence_url, status, cleared_at, COALESCE(updated_at, occurred_at)
                 FROM violation_events WHERE status IN ('OPEN', 'ALARMING') ORDER BY id",
            )
            .map_err(|error| error.to_string())?;
        let events = stmt
            .query_map([], map_event_row)
            .map_err(|error| error.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())?;
        Ok(events)
    }

    pub fn migrate_legacy_event_directory(
        &self,
        directory: impl AsRef<Path>,
    ) -> Result<usize, String> {
        let directory = directory.as_ref();
        if !directory.exists() {
            return Ok(0);
        }
        let mut migrated = 0;
        for entry in
            std::fs::read_dir(directory).map_err(|error| format!("读取旧事件目录失败: {error}"))?
        {
            let path = entry
                .map_err(|error| format!("读取旧事件条目失败: {error}"))?
                .path();
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            let bytes = std::fs::read(&path)
                .map_err(|error| format!("读取旧事件文件 {:?} 失败: {error}", path))?;
            let event: CloudEvent = serde_json::from_slice(&bytes)
                .map_err(|error| format!("解析旧事件文件 {:?} 失败: {error}", path))?;
            self.save_event(&event)?;
            std::fs::remove_file(&path)
                .map_err(|error| format!("删除已迁移旧事件文件 {:?} 失败: {error}", path))?;
            migrated += 1;
        }
        if std::fs::read_dir(directory)
            .map_err(|error| format!("检查旧事件目录失败: {error}"))?
            .next()
            .is_none()
        {
            std::fs::remove_dir(directory)
                .map_err(|error| format!("删除空旧事件目录失败: {error}"))?;
        }
        Ok(migrated)
    }

    /// 记录车位在位与进出场轨迹日志
    pub fn record_presence_log(
        &self,
        space_id: &str,
        plate: Option<&str>,
        v_type: &VehicleType,
        phase: &str,
        event_type: &str,
    ) -> Result<(), String> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| "sqlite lock poisoned".to_string())?;

        let v_type_str = match v_type {
            VehicleType::Fuel => "FUEL",
            VehicleType::NewEnergy => "NEW_ENERGY",
            VehicleType::NonMotor => "NON_MOTOR",
            VehicleType::Obstacle => "OBSTACLE",
            VehicleType::SpecialVehicle => "SPECIAL_VEHICLE",
            VehicleType::HeavyVehicle => "HEAVY_VEHICLE",
            VehicleType::Unknown => "UNKNOWN",
        };

        conn.execute(
            "
            INSERT INTO presence_logs (space_id, plate, vehicle_type, phase, event_type, observed_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6)
            ",
            params![
                space_id,
                plate,
                v_type_str,
                phase,
                event_type,
                Utc::now().to_rfc3339()
            ],
        )
        .map_err(|e| format!("写入在位日志失败: {}", e))?;

        Ok(())
    }

    // ====== 平台配置、系统配置、摄像头、充电桩、实时状态与取证队列全量持久化 ======

    pub fn save_platform_config(&self, cfg: &PlatformConfiguration) -> Result<(), String> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| "sqlite lock poisoned".to_string())?;
        let json = serde_json::to_string(cfg).map_err(|e| e.to_string())?;
        conn.execute(
            "INSERT INTO platform_config_state (id, version, config_json, updated_at) VALUES (1, ?1, ?2, ?3)
             ON CONFLICT(id) DO UPDATE SET version = excluded.version, config_json = excluded.config_json, updated_at = excluded.updated_at",
            params![cfg.version as i64, json, Utc::now().to_rfc3339()],
        )
        .map_err(|e| format!("保存平台配置至 SQLite 失败: {}", e))?;
        Ok(())
    }

    pub fn load_platform_config(&self) -> Result<Option<PlatformConfiguration>, String> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| "sqlite lock poisoned".to_string())?;
        let mut stmt = conn
            .prepare("SELECT config_json FROM platform_config_state WHERE id = 1")
            .map_err(|e| e.to_string())?;
        let mut rows = stmt.query([]).map_err(|e| e.to_string())?;
        if let Some(row) = rows.next().map_err(|e| e.to_string())? {
            let json: String = row.get(0).map_err(|e| e.to_string())?;
            let cfg = serde_json::from_str::<PlatformConfiguration>(&json)
                .map_err(|e| format!("解析 SQLite 平台配置失败: {}", e))?;
            Ok(Some(cfg))
        } else {
            Ok(None)
        }
    }

    pub fn save_system_config_json(&self, json: &str) -> Result<(), String> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| "sqlite lock poisoned".to_string())?;
        conn.execute(
            "INSERT INTO node_system_config (id, config_json, updated_at) VALUES (1, ?1, ?2)
             ON CONFLICT(id) DO UPDATE SET config_json = excluded.config_json, updated_at = excluded.updated_at",
            params![json, Utc::now().to_rfc3339()],
        )
        .map_err(|e| format!("保存节点系统配置至 SQLite 失败: {}", e))?;
        Ok(())
    }

    pub fn load_system_config_json(&self) -> Result<Option<String>, String> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| "sqlite lock poisoned".to_string())?;
        let mut stmt = conn
            .prepare("SELECT config_json FROM node_system_config WHERE id = 1")
            .map_err(|e| e.to_string())?;
        let mut rows = stmt.query([]).map_err(|e| e.to_string())?;
        if let Some(row) = rows.next().map_err(|e| e.to_string())? {
            let json: String = row.get(0).map_err(|e| e.to_string())?;
            Ok(Some(json))
        } else {
            Ok(None)
        }
    }

    pub fn upsert_camera(&self, cam: &CameraChannel) -> Result<(), String> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| "sqlite lock poisoned".to_string())?;
        let state_str = serde_json::to_string(&cam.state)
            .unwrap_or_else(|_| "\"REGISTERED\"".into())
            .trim_matches('"')
            .to_string();
        conn.execute(
            "INSERT INTO cameras_state (camera_id, space_id, protocol, stream_url, gb_device_id, roi_json, state, last_heartbeat)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(camera_id) DO UPDATE SET
                space_id = excluded.space_id,
                protocol = excluded.protocol,
                stream_url = excluded.stream_url,
                gb_device_id = excluded.gb_device_id,
                roi_json = excluded.roi_json,
                state = excluded.state,
                last_heartbeat = excluded.last_heartbeat",
            params![
                cam.camera_id,
                cam.space_id,
                cam.protocol.as_deref().unwrap_or("RTSP"),
                cam.stream_url,
                cam.gb_device_id,
                cam.roi_json,
                state_str,
                cam.last_heartbeat.map(|t| t.to_rfc3339())
            ],
        )
        .map_err(|e| format!("持久化摄像头通道至 SQLite 失败: {}", e))?;
        Ok(())
    }

    pub fn replace_cameras(&self, cams: &[CameraChannel]) -> Result<(), String> {
        let mut conn = self
            .conn
            .lock()
            .map_err(|_| "sqlite lock poisoned".to_string())?;
        let tx = conn.transaction().map_err(|e| e.to_string())?;
        tx.execute("DELETE FROM cameras_state", [])
            .map_err(|e| e.to_string())?;
        for cam in cams {
            let state_str = serde_json::to_string(&cam.state)
                .unwrap_or_else(|_| "\"REGISTERED\"".into())
                .trim_matches('"')
                .to_string();
            tx.execute(
                "INSERT INTO cameras_state (camera_id, space_id, protocol, stream_url, gb_device_id, roi_json, state, last_heartbeat)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    cam.camera_id,
                    cam.space_id,
                    cam.protocol.as_deref().unwrap_or("RTSP"),
                    cam.stream_url,
                    cam.gb_device_id,
                    cam.roi_json,
                    state_str,
                    cam.last_heartbeat.map(|t| t.to_rfc3339())
                ],
            )
            .map_err(|e| e.to_string())?;
        }
        tx.commit().map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn delete_camera(&self, camera_id: &str) -> Result<(), String> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| "sqlite lock poisoned".to_string())?;
        conn.execute(
            "DELETE FROM cameras_state WHERE camera_id = ?1",
            params![camera_id],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn load_cameras(&self) -> Result<Vec<CameraChannel>, String> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| "sqlite lock poisoned".to_string())?;
        let mut stmt = conn
            .prepare(
                "SELECT camera_id, space_id, protocol, stream_url, gb_device_id, roi_json, state, last_heartbeat
                 FROM cameras_state ORDER BY camera_id",
            )
            .map_err(|e| e.to_string())?;
        let iter = stmt
            .query_map([], |row| {
                let camera_id: String = row.get(0)?;
                let space_id: String = row.get(1)?;
                let protocol: String = row.get(2)?;
                let stream_url: Option<String> = row.get(3)?;
                let gb_device_id: String = row.get(4)?;
                let roi_json: Option<String> = row.get(5)?;
                let state_str: String = row.get(6)?;
                let hb_str: Option<String> = row.get(7)?;
                let state = match state_str.as_str() {
                    "ONLINE" => CameraState::Online,
                    "OFFLINE" => CameraState::Offline,
                    "ERROR" => CameraState::Error,
                    _ => CameraState::Registered,
                };
                let last_heartbeat = hb_str
                    .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
                    .map(|dt| dt.with_timezone(&Utc));
                Ok(CameraChannel {
                    channel_id: camera_id.clone(),
                    camera_id,
                    space_id,
                    gb_device_id,
                    state,
                    stream_url,
                    last_heartbeat,
                    protocol: Some(protocol),
                    roi_json,
                })
            })
            .map_err(|e| e.to_string())?;
        iter.collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())
    }

    pub fn save_charger_snapshot(&self, snap: &ChargerSnapshot) -> Result<(), String> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| "sqlite lock poisoned".to_string())?;
        let status_str = serde_json::to_string(&snap.status)
            .unwrap_or_else(|_| "\"UNKNOWN\"".into())
            .trim_matches('"')
            .to_string();
        let staleness_json =
            serde_json::to_string(&snap.staleness_status).unwrap_or_else(|_| "\"FRESH\"".into());
        conn.execute(
            "INSERT INTO charger_snapshots (space_id, charger_id, status, session_id, started_at, completed_at, observed_at, provider, raw_timestamp, staleness_json, is_simulated)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
             ON CONFLICT(space_id) DO UPDATE SET
                charger_id = excluded.charger_id,
                status = excluded.status,
                session_id = excluded.session_id,
                started_at = excluded.started_at,
                completed_at = excluded.completed_at,
                observed_at = excluded.observed_at,
                provider = excluded.provider,
                raw_timestamp = excluded.raw_timestamp,
                staleness_json = excluded.staleness_json,
                is_simulated = excluded.is_simulated",
            params![
                snap.space_id,
                snap.charger_id,
                status_str,
                snap.session_id,
                snap.started_at.map(|t| t.to_rfc3339()),
                snap.completed_at.map(|t| t.to_rfc3339()),
                snap.observed_at.to_rfc3339(),
                snap.provider,
                snap.raw_timestamp.map(|t| t.to_rfc3339()),
                staleness_json,
                if snap.is_simulated { 1 } else { 0 }
            ],
        )
        .map_err(|e| format!("保存充电桩快照至 SQLite 失败: {}", e))?;
        Ok(())
    }

    pub fn load_charger_snapshots(&self) -> Result<Vec<ChargerSnapshot>, String> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| "sqlite lock poisoned".to_string())?;
        let mut stmt = conn
            .prepare(
                "SELECT space_id, charger_id, status, session_id, started_at, completed_at, observed_at, provider, raw_timestamp, staleness_json, is_simulated
                 FROM charger_snapshots",
            )
            .map_err(|e| e.to_string())?;
        let iter = stmt
            .query_map([], |row| {
                let space_id: String = row.get(0)?;
                let charger_id: String = row.get(1)?;
                let status_str: String = row.get(2)?;
                let status = serde_json::from_str::<ChargerStatus>(&format!("\"{}\"", status_str))
                    .unwrap_or(ChargerStatus::Unknown);
                let session_id: Option<String> = row.get(3)?;
                let started_at = row
                    .get::<_, Option<String>>(4)?
                    .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
                    .map(|dt| dt.with_timezone(&Utc));
                let completed_at = row
                    .get::<_, Option<String>>(5)?
                    .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
                    .map(|dt| dt.with_timezone(&Utc));
                let observed_at = DateTime::parse_from_rfc3339(&row.get::<_, String>(6)?)
                    .map(|dt| dt.with_timezone(&Utc))
                    .unwrap_or_else(|_| Utc::now());
                let provider: String = row.get(7)?;
                let raw_timestamp = row
                    .get::<_, Option<String>>(8)?
                    .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
                    .map(|dt| dt.with_timezone(&Utc));
                let staleness_json: String = row.get(9)?;
                let staleness_status = serde_json::from_str::<StalenessStatus>(&staleness_json)
                    .unwrap_or(StalenessStatus::Fresh);
                let is_simulated: i64 = row.get(10)?;
                Ok(ChargerSnapshot {
                    charger_id,
                    space_id,
                    status,
                    session_id,
                    started_at,
                    completed_at,
                    observed_at,
                    provider,
                    raw_timestamp,
                    staleness_status,
                    is_simulated: is_simulated != 0,
                })
            })
            .map_err(|e| e.to_string())?;
        iter.collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())
    }

    pub fn save_space_status(&self, status: &SpaceRuntimeStatus) -> Result<(), String> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| "sqlite lock poisoned".to_string())?;
        let json = serde_json::to_string(status).map_err(|e| e.to_string())?;
        conn.execute(
            "INSERT INTO space_runtime_state (space_id, status_json, updated_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(space_id) DO UPDATE SET status_json = excluded.status_json, updated_at = excluded.updated_at",
            params![status.space_id, json, Utc::now().to_rfc3339()],
        )
        .map_err(|e| format!("保存车位实时状态至 SQLite 失败: {}", e))?;
        Ok(())
    }

    pub fn delete_space_status(&self, space_id: &str) -> Result<(), String> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| "sqlite lock poisoned".to_string())?;
        conn.execute(
            "DELETE FROM space_runtime_state WHERE space_id = ?1",
            params![space_id],
        )
        .map_err(|e| format!("从 SQLite 删除车位状态失败: {}", e))?;
        Ok(())
    }

    pub fn prune_space_statuses(&self, active_space_ids: &[String]) -> Result<usize, String> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| "sqlite lock poisoned".to_string())?;
        if active_space_ids.is_empty() {
            let count = conn
                .execute("DELETE FROM space_runtime_state", [])
                .map_err(|e| format!("清空车位状态表失败: {}", e))?;
            return Ok(count);
        }
        let placeholders = active_space_ids
            .iter()
            .map(|_| "?")
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "DELETE FROM space_runtime_state WHERE space_id NOT IN ({})",
            placeholders
        );
        let params_vec: Vec<&dyn rusqlite::ToSql> = active_space_ids
            .iter()
            .map(|s| s as &dyn rusqlite::ToSql)
            .collect();
        let count = conn
            .execute(&sql, rusqlite::params_from_iter(params_vec))
            .map_err(|e| format!("清理未绑定车位历史状态失败: {}", e))?;
        Ok(count)
    }

    pub fn load_space_statuses(&self) -> Result<Vec<SpaceRuntimeStatus>, String> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| "sqlite lock poisoned".to_string())?;
        let mut stmt = conn
            .prepare("SELECT status_json FROM space_runtime_state ORDER BY space_id")
            .map_err(|e| e.to_string())?;
        let iter = stmt
            .query_map([], |row| {
                let json: String = row.get(0)?;
                Ok(serde_json::from_str::<SpaceRuntimeStatus>(&json).ok())
            })
            .map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        for item in iter {
            if let Ok(Some(st)) = item {
                out.push(st);
            }
        }
        Ok(out)
    }

    pub fn save_recognition_record(&self, rec: &RecognitionRecord) -> Result<(), String> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| "sqlite lock poisoned".to_string())?;
        let v_type_str = match rec.vehicle_type {
            VehicleType::Fuel => "FUEL",
            VehicleType::NewEnergy => "NEW_ENERGY",
            VehicleType::NonMotor => "NON_MOTOR",
            VehicleType::Obstacle => "OBSTACLE",
            VehicleType::SpecialVehicle => "SPECIAL_VEHICLE",
            VehicleType::HeavyVehicle => "HEAVY_VEHICLE",
            VehicleType::Unknown => "UNKNOWN",
        };
        conn.execute(
            "INSERT INTO recognition_records (record_id, space_id, camera_id, plate, plate_color, vehicle_type, confidence, snapshot_url, direction, judgment_status, is_simulated, recognized_at, synced)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, 0)
             ON CONFLICT(record_id) DO NOTHING",
            params![
                rec.record_id,
                rec.space_id,
                rec.camera_id,
                rec.plate,
                rec.plate_color,
                v_type_str,
                rec.confidence,
                rec.snapshot_url,
                rec.direction,
                rec.judgment_status,
                if rec.is_simulated { 1 } else { 0 },
                rec.recognized_at.to_rfc3339()
            ],
        )
        .map_err(|e| format!("保存识别流水至 SQLite 失败: {}", e))?;
        Ok(())
    }

    pub fn fetch_pending_recognition_records(
        &self,
        limit: usize,
    ) -> Result<Vec<RecognitionRecord>, String> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| "sqlite lock poisoned".to_string())?;
        let mut stmt = conn
            .prepare(
                "SELECT record_id, space_id, camera_id, plate, plate_color, vehicle_type, confidence, snapshot_url, direction, judgment_status, is_simulated, recognized_at
                 FROM recognition_records WHERE synced = 0 ORDER BY recognized_at ASC LIMIT ?1",
            )
            .map_err(|e| e.to_string())?;
        let iter = stmt
            .query_map(params![limit as i64], |row| {
                let v_str: String = row.get(5)?;
                let vehicle_type = match v_str.as_str() {
                    "FUEL" => VehicleType::Fuel,
                    "NEW_ENERGY" => VehicleType::NewEnergy,
                    "NON_MOTOR" => VehicleType::NonMotor,
                    "OBSTACLE" => VehicleType::Obstacle,
                    "SPECIAL_VEHICLE" => VehicleType::SpecialVehicle,
                    "HEAVY_VEHICLE" => VehicleType::HeavyVehicle,
                    _ => VehicleType::Unknown,
                };
                let recognized_at = DateTime::parse_from_rfc3339(&row.get::<_, String>(11)?)
                    .map(|dt| dt.with_timezone(&Utc))
                    .unwrap_or_else(|_| Utc::now());
                Ok(RecognitionRecord {
                    record_id: row.get(0)?,
                    space_id: row.get(1)?,
                    camera_id: row.get(2)?,
                    plate: row.get(3)?,
                    plate_color: row.get(4)?,
                    vehicle_type,
                    confidence: row.get(6)?,
                    snapshot_url: row.get(7)?,
                    direction: row.get(8)?,
                    judgment_status: row.get(9)?,
                    is_simulated: row.get::<_, i64>(10)? != 0,
                    recognized_at,
                })
            })
            .map_err(|e| e.to_string())?;
        iter.collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())
    }

    pub fn mark_recognition_synced(&self, record_ids: &[String]) -> Result<(), String> {
        if record_ids.is_empty() {
            return Ok(());
        }
        let mut conn = self
            .conn
            .lock()
            .map_err(|_| "sqlite lock poisoned".to_string())?;
        let tx = conn.transaction().map_err(|e| e.to_string())?;
        for id in record_ids {
            tx.execute(
                "UPDATE recognition_records SET synced = 1 WHERE record_id = ?1",
                params![id],
            )
            .map_err(|e| e.to_string())?;
        }
        tx.commit().map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn queue_evidence_upload(&self, item: &PendingEvidenceUpload) -> Result<(), String> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| "sqlite lock poisoned".to_string())?;
        conn.execute(
            "INSERT INTO pending_evidence_uploads (file_name, local_path, space_id, event_id, synced, created_at)
             VALUES (?1, ?2, ?3, ?4, 0, ?5)
             ON CONFLICT(file_name) DO NOTHING",
            params![
                item.file_name,
                item.local_path,
                item.space_id,
                item.event_id,
                Utc::now().to_rfc3339()
            ],
        )
        .map_err(|e| format!("保存待上传取证队列失败: {}", e))?;
        Ok(())
    }

    pub fn fetch_pending_evidence_uploads(
        &self,
        limit: usize,
    ) -> Result<Vec<PendingEvidenceUpload>, String> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| "sqlite lock poisoned".to_string())?;
        let mut stmt = conn
            .prepare(
                "SELECT file_name, local_path, space_id, event_id
                 FROM pending_evidence_uploads WHERE synced = 0 ORDER BY created_at ASC LIMIT ?1",
            )
            .map_err(|e| e.to_string())?;
        let iter = stmt
            .query_map(params![limit as i64], |row| {
                Ok(PendingEvidenceUpload {
                    file_name: row.get(0)?,
                    local_path: row.get(1)?,
                    space_id: row.get(2)?,
                    event_id: row.get(3)?,
                })
            })
            .map_err(|e| e.to_string())?;
        iter.collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())
    }

    pub fn mark_evidence_uploaded(&self, file_name: &str) -> Result<(), String> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| "sqlite lock poisoned".to_string())?;
        conn.execute(
            "UPDATE pending_evidence_uploads SET synced = 1 WHERE file_name = ?1",
            params![file_name],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }
}

fn map_event_row(row: &Row<'_>) -> rusqlite::Result<CloudEvent> {
    let event_id_str: String = row.get(0)?;
    let event_id = Uuid::parse_str(&event_id_str).unwrap_or_else(|_| Uuid::nil());
    let session_id: String = row.get(1)?;
    let space_id: String = row.get(2)?;
    let plate: Option<String> = row.get(3)?;
    let vehicle_type = match row.get::<_, String>(4)?.as_str() {
        "FUEL" => VehicleType::Fuel,
        "NEW_ENERGY" => VehicleType::NewEnergy,
        "NON_MOTOR" => VehicleType::NonMotor,
        "OBSTACLE" => VehicleType::Obstacle,
        "SPECIAL_VEHICLE" => VehicleType::SpecialVehicle,
        "HEAVY_VEHICLE" => VehicleType::HeavyVehicle,
        _ => VehicleType::Unknown,
    };
    let violation_type: Option<String> = row.get(5)?;
    let occurred_at = DateTime::parse_from_rfc3339(&row.get::<_, String>(6)?)
        .map(|value| value.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now());
    let evidence_url: Option<String> = row.get(7)?;
    let status = match row.get::<_, String>(8)?.as_str() {
        "OPEN" => EventStatus::Open,
        "CLEARED" => EventStatus::Cleared,
        "ACKNOWLEDGED" => EventStatus::Acknowledged,
        _ => EventStatus::Alarming,
    };
    let cleared_at = row
        .get::<_, Option<String>>(9)?
        .and_then(|value| DateTime::parse_from_rfc3339(&value).ok())
        .map(|value| value.with_timezone(&Utc));
    let updated_at = DateTime::parse_from_rfc3339(&row.get::<_, String>(10)?)
        .map(|value| value.with_timezone(&Utc))
        .unwrap_or(occurred_at);
    Ok(CloudEvent {
        event_id,
        session_id,
        space_id,
        plate,
        vehicle_type,
        violation_type,
        occurred_at,
        evidence_url,
        status,
        cleared_at,
        updated_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DeviceBinding, PresencePhase, RuleConfig};

    #[test]
    fn test_sqlite_event_lifecycle() {
        let storage = SqliteStorage::in_memory().unwrap();
        let event_id = Uuid::new_v4();
        let event = CloudEvent {
            event_id,
            session_id: "A-01:1".into(),
            space_id: "A-01".into(),
            plate: Some("粤A88888".into()),
            vehicle_type: VehicleType::Fuel,
            violation_type: Some("FUEL_VEHICLE".into()),
            occurred_at: Utc::now(),
            evidence_url: Some("https://oss.my-cloud.com/evidences/A01.jpg".into()),
            status: EventStatus::Alarming,
            cleared_at: None,
            updated_at: Utc::now(),
        };

        storage.save_event(&event).unwrap();
        assert_eq!(storage.pending_count().unwrap(), 1);

        let pending = storage.fetch_pending_events(10).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].event_id, event_id);
        assert_eq!(pending[0].space_id, "A-01");
        assert_eq!(pending[0].plate.as_deref(), Some("粤A88888"));

        storage.mark_event_synced(&event_id).unwrap();
        assert_eq!(storage.pending_count().unwrap(), 0);

        let mut cleared = event.clone();
        cleared.status = EventStatus::Cleared;
        cleared.cleared_at = Some(Utc::now());
        cleared.updated_at = Utc::now();
        storage.save_event(&cleared).unwrap();
        assert_eq!(storage.pending_count().unwrap(), 1);
        assert_eq!(
            storage.fetch_pending_events(10).unwrap()[0].status,
            EventStatus::Cleared
        );
        assert!(storage.active_events().unwrap().is_empty());

        storage.mark_event_synced(&event_id).unwrap();
        assert!(storage.fetch_pending_events(10).unwrap().is_empty());
    }

    #[test]
    fn test_sqlite_presence_log() {
        let storage = SqliteStorage::in_memory().unwrap();
        storage
            .record_presence_log(
                "A-02",
                Some("粤B66666"),
                &VehicleType::NewEnergy,
                "OCCUPIED",
                "VEHICLE_ENTERED",
            )
            .unwrap();
    }

    #[test]
    fn test_legacy_file_spool_migrates_to_sqlite_once() {
        let storage = SqliteStorage::in_memory().unwrap();
        let directory = std::env::temp_dir().join(format!("legacy-event-spool-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let event = CloudEvent {
            event_id: Uuid::new_v4(),
            session_id: "A-01:legacy".into(),
            space_id: "A-01".into(),
            plate: Some("粤A12345".into()),
            vehicle_type: VehicleType::Fuel,
            violation_type: Some("FUEL_VEHICLE".into()),
            occurred_at: Utc::now(),
            evidence_url: None,
            status: EventStatus::Alarming,
            cleared_at: None,
            updated_at: Utc::now(),
        };
        std::fs::write(
            directory.join("event.json"),
            serde_json::to_vec(&event).unwrap(),
        )
        .unwrap();
        assert_eq!(
            storage.migrate_legacy_event_directory(&directory).unwrap(),
            1
        );
        assert_eq!(storage.pending_count().unwrap(), 1);
        assert!(!directory.exists());
    }

    #[test]
    fn test_sqlite_full_runtime_state_persists_across_reopen() {
        let tmp_dir = std::env::temp_dir().join(format!("edge-db-full-{}", Uuid::new_v4()));
        let db_path = tmp_dir.join("edge.db");
        let now = Utc::now();

        {
            let db = SqliteStorage::open(&db_path).unwrap();
            let cfg = PlatformConfiguration {
                version: 3,
                updated_at: now,
                rules: RuleConfig::default(),
                bindings: vec![DeviceBinding {
                    site_id: "site-i3".into(),
                    space_id: "A-01".into(),
                    camera_id: "CAM-01".into(),
                    charger_id: "CHG-01".into(),
                    relay_channel: 0,
                    speaker_channel: None,
                    enabled: true,
                    ..Default::default()
                }],
                whitelist: Vec::new(),
                cameras: Vec::new(),
            };
            db.save_platform_config(&cfg).unwrap();
            db.save_system_config_json("{\"nodeId\":\"edge-i3-01\"}")
                .unwrap();

            let cam = CameraChannel {
                camera_id: "CAM-01".into(),
                channel_id: "CAM-01".into(),
                space_id: "A-01".into(),
                gb_device_id: "CAM-01".into(),
                state: CameraState::Online,
                stream_url: Some("rtsp://user:pass@192.168.1.64:554/stream1".into()),
                last_heartbeat: Some(now),
                protocol: Some("RTSP".into()),
                roi_json: Some("[0.1,0.1,0.9,0.9]".into()),
            };
            db.upsert_camera(&cam).unwrap();

            let charger = ChargerSnapshot {
                charger_id: "CHG-01".into(),
                space_id: "A-01".into(),
                status: ChargerStatus::Charging,
                session_id: Some("sess-1".into()),
                started_at: Some(now),
                completed_at: None,
                observed_at: now,
                provider: "modbus".into(),
                raw_timestamp: Some(now),
                staleness_status: StalenessStatus::Fresh,
                is_simulated: false,
            };
            db.save_charger_snapshot(&charger).unwrap();

            let status = SpaceRuntimeStatus {
                node_id: "edge-i3-01".into(),
                site_id: "site-i3".into(),
                space_id: "A-01".into(),
                phase: PresencePhase::Occupied,
                charger_status: ChargerStatus::Charging,
                plate: Some("粤BD12345".into()),
                vehicle_type: VehicleType::NewEnergy,
                alarming: false,
                active_event_id: None,
                observed_at: now,
                is_simulated: false,
                stream_type: Some("RTSP_VISION_AI".into()),
                snapshot_url: None,
                confidence: 96.5,
            };
            db.save_space_status(&status).unwrap();

            db.save_recognition_record(&RecognitionRecord {
                record_id: "rec-1".into(),
                space_id: "A-01".into(),
                camera_id: "CAM-01".into(),
                plate: Some("粤BD12345".into()),
                plate_color: "GREEN".into(),
                vehicle_type: VehicleType::NewEnergy,
                confidence: 96.5,
                snapshot_url: None,
                direction: "ENTER".into(),
                judgment_status: "NORMAL_CHARGING".into(),
                is_simulated: false,
                recognized_at: now,
            })
            .unwrap();

            db.queue_evidence_upload(&PendingEvidenceUpload {
                file_name: "ev_1.jpg".into(),
                local_path: "/tmp/ev_1.jpg".into(),
                space_id: "A-01".into(),
                event_id: None,
            })
            .unwrap();
        }

        {
            let reopened = SqliteStorage::open(&db_path).unwrap();
            let cfg = reopened.load_platform_config().unwrap().unwrap();
            assert_eq!(cfg.version, 3);
            assert_eq!(cfg.bindings[0].space_id, "A-01");
            assert!(reopened
                .load_system_config_json()
                .unwrap()
                .unwrap()
                .contains("edge-i3-01"));

            let cams = reopened.load_cameras().unwrap();
            assert_eq!(cams.len(), 1);
            assert_eq!(cams[0].roi_json.as_deref(), Some("[0.1,0.1,0.9,0.9]"));

            let chargers = reopened.load_charger_snapshots().unwrap();
            assert_eq!(chargers.len(), 1);
            assert_eq!(chargers[0].status, ChargerStatus::Charging);

            let statuses = reopened.load_space_statuses().unwrap();
            assert_eq!(statuses.len(), 1);
            assert_eq!(statuses[0].plate.as_deref(), Some("粤BD12345"));

            let recs = reopened.fetch_pending_recognition_records(10).unwrap();
            assert_eq!(recs.len(), 1);
            reopened
                .mark_recognition_synced(&[recs[0].record_id.clone()])
                .unwrap();
            assert!(reopened
                .fetch_pending_recognition_records(10)
                .unwrap()
                .is_empty());

            let evs = reopened.fetch_pending_evidence_uploads(10).unwrap();
            assert_eq!(evs.len(), 1);
            reopened.mark_evidence_uploaded(&evs[0].file_name).unwrap();
            assert!(reopened
                .fetch_pending_evidence_uploads(10)
                .unwrap()
                .is_empty());
        }

        let _ = std::fs::remove_dir_all(tmp_dir);
    }
}
