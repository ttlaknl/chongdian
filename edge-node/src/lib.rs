use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Mutex,
};
use uuid::Uuid;

pub mod relay;
pub use relay::*;

pub mod tracker;
pub use tracker::*;

pub mod charger;
pub use charger::*;

pub mod db;
pub use db::*;

pub mod storage;
pub use storage::*;

pub mod operator;
pub use operator::*;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum VehicleType {
    Fuel,
    NewEnergy,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ChargerStatus {
    Idle,
    Preparing,
    Charging,
    Completed,
    Fault,
    Offline,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChargerSnapshot {
    pub charger_id: String,
    pub space_id: String,
    pub status: ChargerStatus,
    pub session_id: Option<String>,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    pub observed_at: DateTime<Utc>,
    pub provider: String,
}

pub trait ChargerAdapter: Send + Sync {
    fn snapshot(&self, space_id: &str) -> Result<ChargerSnapshot, String>;
}

#[derive(Clone, Default)]
pub struct MemoryChargerAdapter {
    snapshots: std::sync::Arc<Mutex<HashMap<String, ChargerSnapshot>>>,
}

impl MemoryChargerAdapter {
    pub fn apply_bindings(&self, bindings: &[DeviceBinding]) -> Result<(), String> {
        let mut snapshots = self.snapshots.lock().map_err(|_| "charger lock poisoned".to_string())?;
        snapshots.retain(|space_id, _| bindings.iter().any(|binding| binding.enabled && binding.space_id == *space_id));
        for binding in bindings.iter().filter(|binding| binding.enabled) {
            snapshots.entry(binding.space_id.clone()).or_insert_with(|| ChargerSnapshot {
                charger_id: binding.charger_id.clone(),
                space_id: binding.space_id.clone(),
                status: ChargerStatus::Unknown,
                session_id: None,
                started_at: None,
                completed_at: None,
                observed_at: Utc::now(),
                provider: "memory".into(),
            });
        }
        Ok(())
    }

    pub fn update(&self, snapshot: ChargerSnapshot) -> Result<(), String> {
        self.snapshots.lock().map_err(|_| "charger lock poisoned".to_string())?.insert(snapshot.space_id.clone(), snapshot);
        Ok(())
    }

    pub fn snapshots(&self) -> Result<Vec<ChargerSnapshot>, String> {
        self.snapshots.lock().map(|snapshots| snapshots.values().cloned().collect()).map_err(|_| "charger lock poisoned".to_string())
    }
}

impl ChargerAdapter for MemoryChargerAdapter {
    fn snapshot(&self, space_id: &str) -> Result<ChargerSnapshot, String> {
        self.snapshots.lock().map_err(|_| "charger lock poisoned".to_string())?.get(space_id).cloned().ok_or_else(|| format!("charger for space {space_id} is not configured"))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuleConfig {
    pub uncharged_grace_seconds: i64,
    pub completed_grace_seconds: i64,
    pub vehicle_enter_confirm_seconds: i64,
    pub vehicle_leave_confirm_seconds: i64,
    pub alarm_repeat_seconds: i64,
    pub alarm_auto_stop_seconds: i64,
    pub unknown_vehicle_action: String,
    pub unknown_status_action: String,
    pub fault_status_action: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceBinding {
    pub site_id: String,
    pub space_id: String,
    pub camera_id: String,
    pub charger_id: String,
    pub relay_channel: u16,
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CameraState {
    Registered,
    Online,
    Offline,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CameraChannel {
    pub camera_id: String,
    pub channel_id: String,
    pub space_id: String,
    pub gb_device_id: String,
    pub state: CameraState,
    pub stream_url: Option<String>,
    pub last_heartbeat: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone)]
pub struct VideoChannelManager {
    max_channels: usize,
    channels: std::sync::Arc<Mutex<HashMap<String, CameraChannel>>>,
}

impl VideoChannelManager {
    pub fn new(max_channels: usize) -> Self {
        Self {
            max_channels,
            channels: std::sync::Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn apply_bindings(&self, bindings: &[DeviceBinding]) -> Result<(), String> {
        let enabled: Vec<&DeviceBinding> =
            bindings.iter().filter(|binding| binding.enabled).collect();
        if enabled.len() > self.max_channels {
            return Err(format!(
                "enabled camera channels {} exceed node capacity {}",
                enabled.len(),
                self.max_channels
            ));
        }
        let mut channels = self
            .channels
            .lock()
            .map_err(|_| "camera channel lock poisoned".to_string())?;
        channels.retain(|camera_id, _| {
            enabled
                .iter()
                .any(|binding| binding.camera_id == *camera_id)
        });
        for binding in enabled {
            let entry = channels
                .entry(binding.camera_id.clone())
                .or_insert_with(|| CameraChannel {
                    camera_id: binding.camera_id.clone(),
                    channel_id: binding.camera_id.clone(),
                    space_id: binding.space_id.clone(),
                    gb_device_id: binding.camera_id.clone(),
                    state: CameraState::Registered,
                    stream_url: None,
                    last_heartbeat: None,
                });
            entry.space_id = binding.space_id.clone();
        }
        Ok(())
    }

    pub fn register(&self, mut channel: CameraChannel) -> Result<(), String> {
        let mut channels = self
            .channels
            .lock()
            .map_err(|_| "camera channel lock poisoned".to_string())?;
        if !channels.contains_key(&channel.camera_id) && channels.len() >= self.max_channels {
            return Err(format!(
                "camera channel capacity {} reached",
                self.max_channels
            ));
        }
        channel.state = CameraState::Registered;
        channels.insert(channel.camera_id.clone(), channel);
        Ok(())
    }

    pub fn heartbeat(&self, camera_id: &str, now: DateTime<Utc>) -> Result<CameraChannel, String> {
        let mut channels = self
            .channels
            .lock()
            .map_err(|_| "camera channel lock poisoned".to_string())?;
        let channel = channels
            .get_mut(camera_id)
            .ok_or_else(|| format!("camera {camera_id} is not registered"))?;
        channel.state = CameraState::Online;
        channel.last_heartbeat = Some(now);
        Ok(channel.clone())
    }

    pub fn snapshot(&self) -> Result<Vec<CameraChannel>, String> {
        self.channels
            .lock()
            .map(|channels| channels.values().cloned().collect())
            .map_err(|_| "camera channel lock poisoned".to_string())
    }

    pub fn count(&self) -> Result<usize, String> {
        self.channels
            .lock()
            .map(|channels| channels.len())
            .map_err(|_| "camera channel lock poisoned".to_string())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlatformConfiguration {
    pub version: u64,
    pub updated_at: DateTime<Utc>,
    pub rules: RuleConfig,
    pub bindings: Vec<DeviceBinding>,
}

impl Default for PlatformConfiguration {
    fn default() -> Self {
        let bindings = (1..=6)
            .map(|i| DeviceBinding {
                site_id: "site-demo".into(),
                space_id: format!("A-{i:02}"),
                camera_id: format!("camera-sim-{i:02}"),
                charger_id: format!("charger-sim-{i:02}"),
                relay_channel: (i - 1) as u16,
                enabled: true,
            })
            .collect();
        Self {
            version: 0,
            updated_at: Utc::now(),
            rules: RuleConfig::default(),
            bindings,
        }
    }
}

impl PlatformConfiguration {
    pub fn validate(&self) -> Result<(), String> {
        self.rules.validate()?;
        if self.bindings.is_empty() || self.bindings.len() > 16 {
            return Err(format!(
                "车位绑定数量 {} 超出范围，系统支持 1 至 16 路车位（现场实施推荐 6~16 位）",
                self.bindings.len()
            ));
        }
        let mut spaces = std::collections::HashSet::new();
        let mut channels = std::collections::HashSet::new();
        for binding in &self.bindings {
            for (name, value) in [
                ("siteId", binding.site_id.as_str()),
                ("spaceId", binding.space_id.as_str()),
                ("cameraId", binding.camera_id.as_str()),
                ("chargerId", binding.charger_id.as_str()),
            ] {
                if value.trim().is_empty() {
                    return Err(format!("{name} must not be blank"));
                }
            }
            if binding.relay_channel >= 16 {
                return Err(format!(
                    "继电器通道编号 {} 超出最大 16 路范围 (0..15)",
                    binding.relay_channel
                ));
            }
            if !spaces.insert(binding.space_id.clone()) {
                return Err(format!(
                    "duplicate binding for spaceId {}",
                    binding.space_id
                ));
            }
            if binding.enabled && !channels.insert(binding.relay_channel) {
                return Err(format!(
                    "继电器物理通道 {} 被多个启用的车位重复占用",
                    binding.relay_channel
                ));
            }
        }
        Ok(())
    }
}

impl Default for RuleConfig {
    fn default() -> Self {
        Self {
            uncharged_grace_seconds: 600,
            completed_grace_seconds: 900,
            vehicle_enter_confirm_seconds: 3,
            vehicle_leave_confirm_seconds: 5,
            alarm_repeat_seconds: 60,
            alarm_auto_stop_seconds: 300,
            unknown_vehicle_action: "WAIT".into(),
            unknown_status_action: "WAIT".into(),
            fault_status_action: "ALLOW".into(),
        }
    }
}

impl RuleConfig {
    pub fn validate(&self) -> Result<(), String> {
        let values = [
            ("unchargedGraceSeconds", self.uncharged_grace_seconds),
            ("completedGraceSeconds", self.completed_grace_seconds),
            (
                "vehicleEnterConfirmSeconds",
                self.vehicle_enter_confirm_seconds,
            ),
            (
                "vehicleLeaveConfirmSeconds",
                self.vehicle_leave_confirm_seconds,
            ),
            ("alarmRepeatSeconds", self.alarm_repeat_seconds),
            ("alarmAutoStopSeconds", self.alarm_auto_stop_seconds),
        ];
        for (name, value) in values {
            if !(0..=604_800).contains(&value) {
                return Err(format!("{name} must be between 0 and 604800"));
            }
        }
        for (name, value) in [
            ("unknownVehicleAction", self.unknown_vehicle_action.as_str()),
            ("unknownStatusAction", self.unknown_status_action.as_str()),
            ("faultStatusAction", self.fault_status_action.as_str()),
        ] {
            if !matches!(value, "ALLOW" | "WAIT" | "VIOLATE") {
                return Err(format!("{name} must be ALLOW, WAIT, or VIOLATE"));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EvaluationInput {
    pub space_id: String,
    pub plate: Option<String>,
    pub vehicle_type: VehicleType,
    pub charger_status: ChargerStatus,
    pub occupied_since: DateTime<Utc>,
    pub charge_completed_at: Option<DateTime<Utc>>,
    pub now: DateTime<Utc>,
    pub whitelisted: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Decision {
    Compliant,
    Waiting,
    Violation,
    AllowedFault,
    Whitelisted,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EvaluationResult {
    pub event_id: Uuid,
    pub space_id: String,
    pub decision: Decision,
    pub violation_type: Option<String>,
    pub should_alarm: bool,
    pub reason: String,
}

pub fn evaluate(input: &EvaluationInput, rules: &RuleConfig) -> EvaluationResult {
    let base = |decision: Decision, violation_type: Option<String>, alarm: bool, reason: &str| {
        EvaluationResult {
            event_id: Uuid::new_v4(),
            space_id: input.space_id.clone(),
            decision,
            violation_type,
            should_alarm: alarm,
            reason: reason.into(),
        }
    };
    if input.whitelisted {
        return base(Decision::Whitelisted, None, false, "车牌在白名单中");
    }
    if input.vehicle_type == VehicleType::Fuel {
        return base(
            Decision::Violation,
            Some("FUEL_VEHICLE".into()),
            true,
            "燃油车占用充电车位",
        );
    }
    if input.vehicle_type == VehicleType::Unknown {
        return match rules.unknown_vehicle_action.as_str() {
            "ALLOW" => base(Decision::Compliant, None, false, "车型未知，按配置允许停留"),
            "VIOLATE" => base(
                Decision::Violation,
                Some("UNKNOWN_VEHICLE".into()),
                true,
                "车型未知，按配置报警",
            ),
            _ => base(Decision::Waiting, None, false, "车型未知，等待再次识别"),
        };
    }
    if input.charger_status == ChargerStatus::Fault {
        return match rules.fault_status_action.as_str() {
            "VIOLATE" => base(
                Decision::Violation,
                Some("CHARGER_FAULT".into()),
                true,
                "充电桩故障，按配置报警",
            ),
            "WAIT" => base(Decision::Waiting, None, false, "充电桩故障，等待状态恢复"),
            _ => base(
                Decision::AllowedFault,
                None,
                false,
                "充电桩故障，按配置允许停留",
            ),
        };
    }
    if input.charger_status == ChargerStatus::Charging
        || input.charger_status == ChargerStatus::Preparing
    {
        return base(Decision::Compliant, None, false, "车辆处于充电流程");
    }
    if input.charger_status == ChargerStatus::Completed {
        let completed_at = input.charge_completed_at.unwrap_or(input.occupied_since);
        if (input.now - completed_at).num_seconds() > rules.completed_grace_seconds {
            return base(
                Decision::Violation,
                Some("CHARGE_COMPLETED_OVERTIME".into()),
                true,
                "充电完成后超时未驶离",
            );
        }
        return base(
            Decision::Waiting,
            None,
            false,
            "充电完成，仍在允许滞留时间内",
        );
    }
    if matches!(
        input.charger_status,
        ChargerStatus::Unknown | ChargerStatus::Offline
    ) {
        return match rules.unknown_status_action.as_str() {
            "ALLOW" => base(
                Decision::Compliant,
                None,
                false,
                "充电状态不可用，按配置允许停留",
            ),
            "VIOLATE" => base(
                Decision::Violation,
                Some("CHARGER_STATUS_UNAVAILABLE".into()),
                true,
                "充电状态不可用，按配置报警",
            ),
            _ => base(Decision::Waiting, None, false, "充电状态未知，等待数据恢复"),
        };
    }
    if (input.now - input.occupied_since).num_seconds() > rules.uncharged_grace_seconds {
        return base(
            Decision::Violation,
            Some("NOT_CHARGING_OVERTIME".into()),
            true,
            "新能源车进入后超时未充电",
        );
    }
    base(Decision::Waiting, None, false, "新能源车等待开始充电")
}


#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CloudEvent {
    pub event_id: Uuid,
    pub space_id: String,
    pub plate: Option<String>,
    pub vehicle_type: VehicleType,
    pub violation_type: Option<String>,
    pub occurred_at: DateTime<Utc>,
    pub evidence_url: Option<String>,
}

#[derive(Debug, Clone)]
pub struct EventSpool {
    directory: PathBuf,
    max_events: usize,
}

#[derive(Debug, Clone)]
pub struct RuleConfigStore {
    path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct ConfigurationStore {
    path: PathBuf,
}

impl ConfigurationStore {
    pub async fn open(path: impl AsRef<Path>) -> std::io::Result<(Self, PlatformConfiguration)> {
        let store = Self {
            path: path.as_ref().to_path_buf(),
        };
        if let Some(parent) = store.path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        match tokio::fs::read(&store.path).await {
            Ok(bytes) => {
                let configuration: PlatformConfiguration =
                    serde_json::from_slice(&bytes).map_err(std::io::Error::other)?;
                configuration
                    .validate()
                    .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
                Ok((store, configuration))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let configuration = PlatformConfiguration::default();
                store.save(&configuration).await?;
                Ok((store, configuration))
            }
            Err(error) => Err(error),
        }
    }

    pub async fn save(&self, configuration: &PlatformConfiguration) -> std::io::Result<()> {
        configuration
            .validate()
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?;
        let bytes = serde_json::to_vec_pretty(configuration).map_err(std::io::Error::other)?;
        let temporary_path = self.path.with_extension("json.tmp");
        tokio::fs::write(&temporary_path, bytes).await?;
        tokio::fs::rename(temporary_path, &self.path).await
    }
}

impl RuleConfigStore {
    pub async fn open(path: impl AsRef<Path>) -> std::io::Result<(Self, RuleConfig)> {
        let store = Self {
            path: path.as_ref().to_path_buf(),
        };
        if let Some(parent) = store.path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        match tokio::fs::read(&store.path).await {
            Ok(bytes) => {
                let rules: RuleConfig =
                    serde_json::from_slice(&bytes).map_err(std::io::Error::other)?;
                rules
                    .validate()
                    .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
                Ok((store, rules))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let rules = RuleConfig::default();
                store.save(&rules).await?;
                Ok((store, rules))
            }
            Err(error) => Err(error),
        }
    }

    pub async fn save(&self, rules: &RuleConfig) -> std::io::Result<()> {
        rules
            .validate()
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?;
        let bytes = serde_json::to_vec_pretty(rules).map_err(std::io::Error::other)?;
        let temporary_path = self.path.with_extension("json.tmp");
        tokio::fs::write(&temporary_path, bytes).await?;
        tokio::fs::rename(temporary_path, &self.path).await
    }
}

impl EventSpool {
    pub async fn open(directory: impl AsRef<Path>, max_events: usize) -> std::io::Result<Self> {
        let directory = directory.as_ref().to_path_buf();
        tokio::fs::create_dir_all(&directory).await?;
        Ok(Self {
            directory,
            max_events,
        })
    }

    pub async fn enqueue(&self, event: &CloudEvent) -> std::io::Result<()> {
        let mut paths = self.event_paths().await?;
        while paths.len() >= self.max_events {
            if let Some(oldest) = paths.first().cloned() {
                tokio::fs::remove_file(oldest).await?;
                paths.remove(0);
            }
        }

        let name = format!(
            "{:013}-{}.json",
            event.occurred_at.timestamp_millis(),
            event.event_id
        );
        let final_path = self.directory.join(&name);
        let temporary_path = self.directory.join(format!("{}.tmp", name));
        let bytes = serde_json::to_vec(event).map_err(std::io::Error::other)?;
        tokio::fs::write(&temporary_path, bytes).await?;
        tokio::fs::rename(temporary_path, final_path).await
    }

    pub async fn next(&self) -> std::io::Result<Option<(PathBuf, CloudEvent)>> {
        let Some(path) = self.event_paths().await?.into_iter().next() else {
            return Ok(None);
        };
        let bytes = tokio::fs::read(&path).await?;
        let event = serde_json::from_slice(&bytes).map_err(std::io::Error::other)?;
        Ok(Some((path, event)))
    }

    pub async fn acknowledge(&self, path: &Path) -> std::io::Result<()> {
        if path.parent() != Some(self.directory.as_path()) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "event path is outside spool",
            ));
        }
        tokio::fs::remove_file(path).await
    }

    pub async fn pending_count(&self) -> std::io::Result<usize> {
        Ok(self.event_paths().await?.len())
    }

    async fn event_paths(&self) -> std::io::Result<Vec<PathBuf>> {
        let mut directory = tokio::fs::read_dir(&self.directory).await?;
        let mut paths = Vec::new();
        while let Some(entry) = directory.next_entry().await? {
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) == Some("json") {
                paths.push(path);
            }
        }
        paths.sort();
        Ok(paths)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn input(v: VehicleType, s: ChargerStatus, age: i64) -> EvaluationInput {
        let now = Utc::now();
        EvaluationInput {
            space_id: "A-01".into(),
            plate: Some("粤A12345".into()),
            vehicle_type: v,
            charger_status: s,
            occupied_since: now - chrono::Duration::seconds(age),
            charge_completed_at: None,
            now,
            whitelisted: false,
        }
    }
    #[test]
    fn fuel_is_violation() {
        assert_eq!(
            evaluate(
                &input(VehicleType::Fuel, ChargerStatus::Idle, 1),
                &RuleConfig::default()
            )
            .decision,
            Decision::Violation
        );
    }
    #[test]
    fn charging_is_compliant() {
        assert_eq!(
            evaluate(
                &input(VehicleType::NewEnergy, ChargerStatus::Charging, 9999),
                &RuleConfig::default()
            )
            .decision,
            Decision::Compliant
        );
    }
    #[test]
    fn uncharged_timeout_alarms() {
        let r = evaluate(
            &input(VehicleType::NewEnergy, ChargerStatus::Idle, 601),
            &RuleConfig::default(),
        );
        assert!(r.should_alarm);
    }
    #[test]
    fn completed_timeout_alarms() {
        let now = Utc::now();
        let mut i = input(VehicleType::NewEnergy, ChargerStatus::Completed, 1);
        i.charge_completed_at = Some(now - chrono::Duration::seconds(901));
        i.now = now;
        let r = evaluate(&i, &RuleConfig::default());
        assert_eq!(
            r.violation_type.as_deref(),
            Some("CHARGE_COMPLETED_OVERTIME")
        );
        assert!(r.should_alarm);
    }
    #[test]
    fn whitelist_suppresses_alarm() {
        let mut i = input(VehicleType::Fuel, ChargerStatus::Idle, 9999);
        i.whitelisted = true;
        assert!(!evaluate(&i, &RuleConfig::default()).should_alarm);
    }
    #[test]
    fn fault_is_allowed_by_default() {
        assert_eq!(
            evaluate(
                &input(VehicleType::NewEnergy, ChargerStatus::Fault, 9999),
                &RuleConfig::default()
            )
            .decision,
            Decision::AllowedFault
        );
    }

    #[test]
    fn relay_state_is_independent_per_space() {
        let relay = MemoryRelayController::default();
        relay.set_alarm("A-01", true).unwrap();
        relay.set_alarm("A-02", false).unwrap();
        let states = relay.snapshot().unwrap();
        assert_eq!(states.get("A-01"), Some(&true));
        assert_eq!(states.get("A-02"), Some(&false));
    }

    #[test]
    fn rejects_invalid_rule_configuration() {
        let mut rules = RuleConfig::default();
        rules.completed_grace_seconds = -1;
        assert!(rules.validate().is_err());
        rules.completed_grace_seconds = 10;
        rules.fault_status_action = "INVALID".into();
        assert!(rules.validate().is_err());
    }

    #[tokio::test]
    async fn disk_spool_survives_reopen_and_acknowledges() {
        let directory =
            std::env::temp_dir().join(format!("charging-edge-spool-{}", Uuid::new_v4()));
        let event = CloudEvent {
            event_id: Uuid::new_v4(),
            space_id: "A-01".into(),
            plate: Some("粤A12345".into()),
            vehicle_type: VehicleType::Fuel,
            violation_type: Some("FUEL_VEHICLE".into()),
            occurred_at: Utc::now(),
            evidence_url: None,
        };
        let spool = EventSpool::open(&directory, 10).await.unwrap();
        spool.enqueue(&event).await.unwrap();
        assert_eq!(
            EventSpool::open(&directory, 10)
                .await
                .unwrap()
                .pending_count()
                .await
                .unwrap(),
            1
        );
        let (path, restored) = spool.next().await.unwrap().unwrap();
        assert_eq!(restored.event_id, event.event_id);
        spool.acknowledge(&path).await.unwrap();
        assert_eq!(spool.pending_count().await.unwrap(), 0);
        tokio::fs::remove_dir(&directory).await.unwrap();
    }

    #[tokio::test]
    async fn rule_configuration_survives_reopen() {
        let directory =
            std::env::temp_dir().join(format!("charging-edge-rules-{}", Uuid::new_v4()));
        let path = directory.join("rules.json");
        let (store, mut rules) = RuleConfigStore::open(&path).await.unwrap();
        rules.uncharged_grace_seconds = 42;
        store.save(&rules).await.unwrap();
        let (_, restored) = RuleConfigStore::open(&path).await.unwrap();
        assert_eq!(restored.uncharged_grace_seconds, 42);
        tokio::fs::remove_file(path).await.unwrap();
        tokio::fs::remove_dir(directory).await.unwrap();
    }

    #[tokio::test]
    async fn platform_configuration_validates_bindings_and_survives_reopen() {
        let directory =
            std::env::temp_dir().join(format!("charging-edge-config-{}", Uuid::new_v4()));
        let path = directory.join("configuration.json");
        let (store, mut configuration) = ConfigurationStore::open(&path).await.unwrap();
        configuration.version = 4;
        configuration.bindings[0].space_id = "B-01".into();
        store.save(&configuration).await.unwrap();
        let (_, restored) = ConfigurationStore::open(&path).await.unwrap();
        assert_eq!(restored.version, 4);
        assert_eq!(restored.bindings[0].space_id, "B-01");
        restored.validate().unwrap();
        tokio::fs::remove_file(path).await.unwrap();
        tokio::fs::remove_dir(directory).await.unwrap();
    }

    #[test]
    fn video_channel_manager_enforces_capacity_and_tracks_heartbeat() {
        let manager = VideoChannelManager::new(1);
        let first = DeviceBinding {
            site_id: "site".into(),
            space_id: "A-01".into(),
            camera_id: "cam-01".into(),
            charger_id: "charger-01".into(),
            relay_channel: 0,
            enabled: true,
        };
        manager.apply_bindings(&[first.clone()]).unwrap();
        manager.heartbeat("cam-01", Utc::now()).unwrap();
        assert_eq!(manager.snapshot().unwrap()[0].state, CameraState::Online);
        let second = DeviceBinding {
            camera_id: "cam-02".into(),
            space_id: "A-02".into(),
            ..first
        };
        assert!(manager.apply_bindings(&[second]).is_ok());
        assert_eq!(manager.count().unwrap(), 1);
    }

    #[test]
    fn video_channel_manager_rejects_more_than_capacity() {
        let manager = VideoChannelManager::new(1);
        let bindings = (0..2)
            .map(|index| DeviceBinding {
                site_id: "site".into(),
                space_id: format!("A-{index:02}"),
                camera_id: format!("cam-{index:02}"),
                charger_id: format!("charger-{index:02}"),
                relay_channel: index,
                enabled: true,
            })
            .collect::<Vec<_>>();
        assert!(manager.apply_bindings(&bindings).is_err());
    }

    #[test]
    fn memory_charger_adapter_maps_binding_and_updates_status() {
        let adapter = MemoryChargerAdapter::default();
        let binding = DeviceBinding { site_id: "site".into(), space_id: "A-01".into(), camera_id: "cam-01".into(), charger_id: "charger-01".into(), relay_channel: 0, enabled: true };
        adapter.apply_bindings(&[binding]).unwrap();
        let mut snapshot = adapter.snapshot("A-01").unwrap();
        assert_eq!(snapshot.status, ChargerStatus::Unknown);
        snapshot.status = ChargerStatus::Charging;
        snapshot.observed_at = Utc::now();
        adapter.update(snapshot).unwrap();
        assert_eq!(adapter.snapshot("A-01").unwrap().status, ChargerStatus::Charging);
    }
}
