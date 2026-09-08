use crate::{CloudEvent, VehicleType};
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use uuid::Uuid;

/// 工业级边缘 SQLite 嵌入式存储引擎（WAL 事务日志模式，防意外断电）
pub struct SqliteStorage {
    db_path: PathBuf,
    conn: Mutex<Connection>,
}

impl SqliteStorage {
    pub fn path(&self) -> &Path {
        &self.db_path
    }
    /// 打开或创建 SQLite 数据库，并应用 WAL 模式与模式初始化
    pub fn open(path: impl AsRef<Path>) -> Result<Self, String> {
        let db_path = path.as_ref().to_path_buf();
        if let Some(parent) = db_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }

        let conn = Connection::open(&db_path)
            .map_err(|e| format!("无法打开 SQLite 数据库 {:?}: {}", db_path, e))?;

        // 开启 WAL 模式和快速同步，抗断电且高并发写入
        conn.execute_batch(
            "
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous = NORMAL;
            PRAGMA busy_timeout = 5000;
            ",
        )
        .map_err(|e| format!("配置 SQLite PRAGMA 失败: {}", e))?;

        // 初始化核心表结构
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS violation_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                event_id TEXT UNIQUE NOT NULL,
                space_id TEXT NOT NULL,
                plate TEXT,
                vehicle_type TEXT NOT NULL,
                violation_type TEXT NOT NULL,
                occurred_at TEXT NOT NULL,
                evidence_url TEXT,
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
            ",
        )
        .map_err(|e| format!("初始化 SQLite 数据库表结构失败: {}", e))?;

        Ok(Self {
            db_path,
            conn: Mutex::new(conn),
        })
    }

    /// 在内存中创建 SQLite 存储（用于单元测试和快速模拟）
    pub fn in_memory() -> Result<Self, String> {
        let conn = Connection::open_in_memory()
            .map_err(|e| format!("无法创建内存 SQLite 数据库: {}", e))?;

        conn.execute_batch(
            "
            CREATE TABLE violation_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                event_id TEXT UNIQUE NOT NULL,
                space_id TEXT NOT NULL,
                plate TEXT,
                vehicle_type TEXT NOT NULL,
                violation_type TEXT NOT NULL,
                occurred_at TEXT NOT NULL,
                evidence_url TEXT,
                synced INTEGER DEFAULT 0,
                retry_count INTEGER DEFAULT 0,
                created_at TEXT NOT NULL
            );

            CREATE TABLE presence_logs (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                space_id TEXT NOT NULL,
                plate TEXT,
                vehicle_type TEXT NOT NULL,
                phase TEXT NOT NULL,
                event_type TEXT NOT NULL,
                observed_at TEXT NOT NULL
            );
            ",
        )
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

        conn.execute(
            "
            INSERT OR REPLACE INTO violation_events (
                event_id, space_id, plate, vehicle_type, violation_type, occurred_at, evidence_url, synced, created_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0, ?8)
            ",
            params![
                event.event_id.to_string(),
                event.space_id,
                event.plate,
                v_type_str,
                event.violation_type.as_deref().unwrap_or(""),
                event.occurred_at.to_rfc3339(),
                event.evidence_url,
                Utc::now().to_rfc3339()
            ],
        )
        .map_err(|e| format!("保存违规事件至 SQLite 失败: {}", e))?;

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
                SELECT event_id, space_id, plate, vehicle_type, violation_type, occurred_at, evidence_url
                FROM violation_events
                WHERE synced = 0
                ORDER BY id ASC
                LIMIT ?1
                ",
            )
            .map_err(|e| e.to_string())?;

        let event_iter = stmt
            .query_map(params![limit as i64], |row| {
                let event_id_str: String = row.get(0)?;
                let event_id = Uuid::parse_str(&event_id_str)
                    .unwrap_or_else(|_| Uuid::nil());
                let space_id: String = row.get(1)?;
                let plate: Option<String> = row.get(2)?;
                let v_type_str: String = row.get(3)?;
                let v_type = match v_type_str.as_str() {
                    "FUEL" => VehicleType::Fuel,
                    "NEW_ENERGY" => VehicleType::NewEnergy,
                    _ => VehicleType::Unknown,
                };
                let violation_type: Option<String> = row.get(4)?;
                let occurred_at_str: String = row.get(5)?;
                let occurred_at = DateTime::parse_from_rfc3339(&occurred_at_str)
                    .map(|dt| dt.with_timezone(&Utc))
                    .unwrap_or_else(|_| Utc::now());
                let evidence_url: Option<String> = row.get(6)?;

                Ok(CloudEvent {
                    event_id,
                    space_id,
                    plate,
                    vehicle_type: v_type,
                    violation_type,
                    occurred_at,
                    evidence_url,
                })
            })
            .map_err(|e| e.to_string())?;

        let mut list = Vec::new();
        for ev in event_iter {
            if let Ok(item) = ev {
                list.push(item);
            }
        }
        Ok(list)
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sqlite_event_lifecycle() {
        let storage = SqliteStorage::in_memory().unwrap();
        let event_id = Uuid::new_v4();
        let event = CloudEvent {
            event_id,
            space_id: "A-01".into(),
            plate: Some("粤A88888".into()),
            vehicle_type: VehicleType::Fuel,
            violation_type: Some("FUEL_VEHICLE".into()),
            occurred_at: Utc::now(),
            evidence_url: Some("https://oss.my-cloud.com/evidences/A01.jpg".into()),
        };

        // 1. 写入事件
        storage.save_event(&event).unwrap();
        assert_eq!(storage.pending_count().unwrap(), 1);

        // 2. 获取待同步列表
        let pending = storage.fetch_pending_events(10).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].event_id, event_id);
        assert_eq!(pending[0].space_id, "A-01");
        assert_eq!(pending[0].plate.as_deref(), Some("粤A88888"));

        // 3. 标记同步完成
        storage.mark_event_synced(&event_id).unwrap();
        assert_eq!(storage.pending_count().unwrap(), 0);

        // 4. 再次获取应为空
        let after_sync = storage.fetch_pending_events(10).unwrap();
        assert!(after_sync.is_empty());
    }

    #[test]
    fn test_sqlite_presence_log() {
        let storage = SqliteStorage::in_memory().unwrap();
        storage
            .record_presence_log("A-02", Some("粤B66666"), &VehicleType::NewEnergy, "OCCUPIED", "VEHICLE_ENTERED")
            .unwrap();
    }
}
