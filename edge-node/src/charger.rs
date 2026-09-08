use crate::{ChargerAdapter, ChargerSnapshot, ChargerStatus, DeviceBinding};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

/// 充电桩平台主动推送或拉取更新的数据载荷
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExternalChargerUpdate {
    pub charger_id: String,
    pub space_id: String,
    pub status: ChargerStatus,
    pub session_id: Option<String>,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    pub provider: Option<String>,
}

/// 统一智能充电桩适配器（支持多桩状态汇聚、心跳超时离线检测与软降级）
#[derive(Clone)]
pub struct DynamicChargerAdapter {
    snapshots: Arc<Mutex<HashMap<String, ChargerSnapshot>>>,
    last_heartbeat: Arc<Mutex<HashMap<String, DateTime<Utc>>>>,
    offline_timeout_seconds: i64,
}

impl Default for DynamicChargerAdapter {
    fn default() -> Self {
        Self::new(60) // 默认 60 秒未收到充电桩心跳视为离线
    }
}

impl DynamicChargerAdapter {
    pub fn new(offline_timeout_seconds: i64) -> Self {
        Self {
            snapshots: Arc::new(Mutex::new(HashMap::new())),
            last_heartbeat: Arc::new(Mutex::new(HashMap::new())),
            offline_timeout_seconds,
        }
    }

    /// 应用车位-设备绑定配置
    pub fn apply_bindings(&self, bindings: &[DeviceBinding]) -> Result<(), String> {
        let mut snapshots = self
            .snapshots
            .lock()
            .map_err(|_| "charger lock poisoned".to_string())?;
        snapshots.retain(|space_id, _| {
            bindings
                .iter()
                .any(|binding| binding.enabled && binding.space_id == *space_id)
        });
        for binding in bindings.iter().filter(|binding| binding.enabled) {
            snapshots
                .entry(binding.space_id.clone())
                .or_insert_with(|| ChargerSnapshot {
                    charger_id: binding.charger_id.clone(),
                    space_id: binding.space_id.clone(),
                    status: ChargerStatus::Unknown,
                    session_id: None,
                    started_at: None,
                    completed_at: None,
                    observed_at: Utc::now(),
                    provider: "dynamic-hub".into(),
                });
        }
        Ok(())
    }

    /// 更新充电桩实时快照
    pub fn update(&self, update: ExternalChargerUpdate) -> Result<(), String> {
        let now = Utc::now();
        let mut snapshots = self
            .snapshots
            .lock()
            .map_err(|_| "charger lock poisoned".to_string())?;
        let mut heartbeats = self
            .last_heartbeat
            .lock()
            .map_err(|_| "charger heartbeat lock poisoned".to_string())?;

        let snapshot = ChargerSnapshot {
            charger_id: update.charger_id.clone(),
            space_id: update.space_id.clone(),
            status: update.status,
            session_id: update.session_id,
            started_at: update.started_at,
            completed_at: update.completed_at,
            observed_at: now,
            provider: update.provider.unwrap_or_else(|| "external-api".into()),
        };

        snapshots.insert(update.space_id.clone(), snapshot);
        heartbeats.insert(update.charger_id, now);
        Ok(())
    }

    /// 执行充电桩离线检测与软降级扫描
    pub fn tick_offline_detection(&self) -> Result<Vec<String>, String> {
        let now = Utc::now();
        let mut degraded_spaces = Vec::new();
        let mut snapshots = self
            .snapshots
            .lock()
            .map_err(|_| "charger lock poisoned".to_string())?;
        let heartbeats = self
            .last_heartbeat
            .lock()
            .map_err(|_| "charger heartbeat lock poisoned".to_string())?;

        for (space_id, snapshot) in snapshots.iter_mut() {
            if let Some(last_hb) = heartbeats.get(&snapshot.charger_id) {
                if (now - *last_hb).num_seconds() > self.offline_timeout_seconds
                    && snapshot.status != ChargerStatus::Offline
                {
                    snapshot.status = ChargerStatus::Offline;
                    snapshot.observed_at = now;
                    degraded_spaces.push(space_id.clone());
                }
            }
        }
        Ok(degraded_spaces)
    }

    pub fn list_snapshots(&self) -> Result<Vec<ChargerSnapshot>, String> {
        self.snapshots
            .lock()
            .map(|s| s.values().cloned().collect())
            .map_err(|_| "charger lock poisoned".to_string())
    }
}

impl ChargerAdapter for DynamicChargerAdapter {
    fn snapshot(&self, space_id: &str) -> Result<ChargerSnapshot, String> {
        self.snapshots
            .lock()
            .map_err(|_| "charger lock poisoned".to_string())?
            .get(space_id)
            .cloned()
            .ok_or_else(|| format!("未找到车位 {} 关联的充电桩配置", space_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    #[test]
    fn test_dynamic_charger_update_and_offline_degradation() {
        let adapter = DynamicChargerAdapter::new(10); // 10 秒超时
        let binding = DeviceBinding {
            site_id: "site-1".into(),
            space_id: "A-01".into(),
            camera_id: "cam-01".into(),
            charger_id: "chg-01".into(),
            relay_channel: 0,
            enabled: true,
        };
        adapter.apply_bindings(&[binding]).unwrap();

        // 初始状态应为 Unknown
        assert_eq!(adapter.snapshot("A-01").unwrap().status, ChargerStatus::Unknown);

        // 收到开始充电更新
        adapter
            .update(ExternalChargerUpdate {
                charger_id: "chg-01".into(),
                space_id: "A-01".into(),
                status: ChargerStatus::Charging,
                session_id: Some("session-1001".into()),
                started_at: Some(Utc::now()),
                completed_at: None,
                provider: Some("cloud-fast-charge".into()),
            })
            .unwrap();

        assert_eq!(adapter.snapshot("A-01").unwrap().status, ChargerStatus::Charging);

        // 人工将最后心跳篡改到 15 秒前，测试离线自动软降级
        {
            let mut hb = adapter.last_heartbeat.lock().unwrap();
            hb.insert("chg-01".into(), Utc::now() - Duration::seconds(15));
        }

        let degraded = adapter.tick_offline_detection().unwrap();
        assert_eq!(degraded, vec!["A-01"]);
        assert_eq!(adapter.snapshot("A-01").unwrap().status, ChargerStatus::Offline);
    }
}
