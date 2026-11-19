use crate::{ChargerAdapter, ChargerSnapshot, ChargerStatus, DeviceBinding, StalenessStatus};
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
    #[serde(default)]
    pub raw_timestamp: Option<DateTime<Utc>>,
    #[serde(default)]
    pub staleness_status: Option<StalenessStatus>,
    #[serde(default)]
    pub is_simulated: bool,
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
        let mut heartbeats = self
            .last_heartbeat
            .lock()
            .map_err(|_| "charger heartbeat lock poisoned".to_string())?;
        let mut replacement = HashMap::new();
        let now = Utc::now();
        for binding in bindings.iter().filter(|binding| binding.enabled) {
            let brand_or_proto = binding
                .charger_brand
                .as_ref()
                .filter(|s| !s.trim().is_empty())
                .cloned()
                .or_else(|| {
                    binding
                        .protocol_id
                        .as_ref()
                        .filter(|s| !s.trim().is_empty())
                        .cloned()
                })
                .unwrap_or_else(|| "external-api".into());

            let snapshot = snapshots
                .get(&binding.space_id)
                .filter(|snapshot| snapshot.charger_id == binding.charger_id)
                .cloned()
                .map(|mut s| {
                    s.provider = brand_or_proto.clone();
                    if s.status == ChargerStatus::Unknown {
                        s.status = ChargerStatus::Idle;
                    }
                    s
                })
                .unwrap_or_else(|| ChargerSnapshot {
                    charger_id: binding.charger_id.clone(),
                    space_id: binding.space_id.clone(),
                    status: ChargerStatus::Idle,
                    session_id: None,
                    started_at: None,
                    completed_at: None,
                    observed_at: now,
                    provider: brand_or_proto,
                    raw_timestamp: None,
                    staleness_status: StalenessStatus::Fresh,
                    is_simulated: false,
                });
            replacement.insert(binding.space_id.clone(), snapshot);
        }
        let active_chargers = replacement
            .values()
            .map(|snapshot| snapshot.charger_id.clone())
            .collect::<std::collections::HashSet<_>>();
        heartbeats.retain(|charger_id, _| active_chargers.contains(charger_id));
        *snapshots = replacement;
        Ok(())
    }

    /// 刷新处于待充或正常状态的充电桩观测时间
    pub fn touch_heartbeats(&self) {
        if let Ok(mut snapshots) = self.snapshots.lock() {
            let now = Utc::now();
            for snapshot in snapshots.values_mut() {
                if snapshot.status == ChargerStatus::Idle || snapshot.status == ChargerStatus::Preparing {
                    snapshot.observed_at = now;
                }
            }
        }
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
            status: update.status.clone(),
            session_id: update.session_id,
            started_at: update.started_at,
            completed_at: update.completed_at,
            observed_at: now,
            provider: update.provider.unwrap_or_else(|| "external-api".into()),
            raw_timestamp: update.raw_timestamp,
            staleness_status: update.staleness_status.unwrap_or(match update.status {
                ChargerStatus::Offline => StalenessStatus::Disconnected,
                ChargerStatus::Fault => StalenessStatus::Fault,
                ChargerStatus::Completed => StalenessStatus::Completed,
                _ => StalenessStatus::Fresh,
            }),
            is_simulated: update.is_simulated,
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
                    snapshot.staleness_status = StalenessStatus::Disconnected;
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

    pub fn restore_snapshots(&self, list: Vec<ChargerSnapshot>) -> Result<(), String> {
        let mut snapshots = self
            .snapshots
            .lock()
            .map_err(|_| "charger lock poisoned".to_string())?;
        for snap in list {
            snapshots.insert(snap.space_id.clone(), snap);
        }
        Ok(())
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
            speaker_channel: None,
            enabled: true,
            ..Default::default()
        };
        adapter.apply_bindings(&[binding]).unwrap();

        // 初始状态应为 Idle
        assert_eq!(
            adapter.snapshot("A-01").unwrap().status,
            ChargerStatus::Idle
        );

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
                raw_timestamp: None,
                staleness_status: Some(StalenessStatus::Fresh),
                is_simulated: false,
            })
            .unwrap();

        assert_eq!(
            adapter.snapshot("A-01").unwrap().status,
            ChargerStatus::Charging
        );

        // 人工将最后心跳篡改到 15 秒前，测试离线自动软降级
        {
            let mut hb = adapter.last_heartbeat.lock().unwrap();
            hb.insert("chg-01".into(), Utc::now() - Duration::seconds(15));
        }

        let degraded = adapter.tick_offline_detection().unwrap();
        assert_eq!(degraded, vec!["A-01"]);
        assert_eq!(
            adapter.snapshot("A-01").unwrap().status,
            ChargerStatus::Offline
        );

        let replacement = DeviceBinding {
            site_id: "site-1".into(),
            space_id: "B-01".into(),
            camera_id: "cam-02".into(),
            charger_id: "chg-02".into(),
            relay_channel: 1,
            speaker_channel: None,
            enabled: true,
            ..Default::default()
        };
        adapter.apply_bindings(&[replacement]).unwrap();
        assert!(adapter.snapshot("A-01").is_err());
        assert_eq!(adapter.snapshot("B-01").unwrap().charger_id, "chg-02");
    }
}
