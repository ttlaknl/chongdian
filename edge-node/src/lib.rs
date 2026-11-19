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

pub mod hardware;
pub use hardware::*;

pub mod pipeline;
pub use pipeline::*;

pub mod watchdog;
pub use watchdog::*;

pub mod gb28181;
pub use gb28181::*;

pub mod rtsp;
pub use rtsp::*;

pub mod ai;
pub use ai::*;

pub mod decoder;
pub use decoder::*;

pub mod upgrade;
pub use upgrade::*;

pub mod metrics;
pub use metrics::*;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum VehicleType {
    Fuel,
    NewEnergy,
    NonMotor,
    Obstacle,
    SpecialVehicle,
    HeavyVehicle,
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum StalenessStatus {
    #[default]
    Fresh,
    Stale(u64),
    Disconnected,
    Fault,
    Completed,
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
    #[serde(default)]
    pub raw_timestamp: Option<DateTime<Utc>>,
    #[serde(default)]
    pub staleness_status: StalenessStatus,
    #[serde(default)]
    pub is_simulated: bool,
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
        let mut snapshots = self
            .snapshots
            .lock()
            .map_err(|_| "charger lock poisoned".to_string())?;
        snapshots.retain(|space_id, _| {
            bindings
                .iter()
                .any(|binding| binding.enabled && binding.space_id == *space_id)
        });
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
                .unwrap_or_else(|| "memory".into());

            snapshots
                .entry(binding.space_id.clone())
                .and_modify(|s| {
                    s.provider = brand_or_proto.clone();
                    if s.status == ChargerStatus::Unknown {
                        s.status = ChargerStatus::Idle;
                    }
                })
                .or_insert_with(|| ChargerSnapshot {
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
        }
        Ok(())
    }

    pub fn update(&self, snapshot: ChargerSnapshot) -> Result<(), String> {
        self.snapshots
            .lock()
            .map_err(|_| "charger lock poisoned".to_string())?
            .insert(snapshot.space_id.clone(), snapshot);
        Ok(())
    }

    pub fn snapshots(&self) -> Result<Vec<ChargerSnapshot>, String> {
        self.snapshots
            .lock()
            .map(|snapshots| snapshots.values().cloned().collect())
            .map_err(|_| "charger lock poisoned".to_string())
    }
}

impl ChargerAdapter for MemoryChargerAdapter {
    fn snapshot(&self, space_id: &str) -> Result<ChargerSnapshot, String> {
        self.snapshots
            .lock()
            .map_err(|_| "charger lock poisoned".to_string())?
            .get(space_id)
            .cloned()
            .ok_or_else(|| format!("charger for space {space_id} is not configured"))
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
    #[serde(default)]
    pub speaker_channel: Option<u16>,
    pub enabled: bool,
    #[serde(default)]
    pub charger_brand: Option<String>,
    #[serde(default)]
    pub protocol_id: Option<String>,
    #[serde(default)]
    pub connector_id: Option<String>,
    #[serde(default)]
    pub charger_params: Option<String>,
}

impl Default for DeviceBinding {
    fn default() -> Self {
        Self {
            site_id: "default".into(),
            space_id: "".into(),
            camera_id: "".into(),
            charger_id: "".into(),
            relay_channel: 0,
            speaker_channel: None,
            enabled: true,
            charger_brand: Some("GENERIC".into()),
            protocol_id: Some("proto-modbus-tcp".into()),
            connector_id: Some("1".into()),
            charger_params: Some("{}".into()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WhitelistEntry {
    pub plate: String,
    pub vehicle_type: Option<String>,
    pub reason: Option<String>,
    pub added_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
    pub enabled: bool,
}

#[derive(Debug, Default)]
pub struct WhitelistRegistry {
    entries: Mutex<HashMap<String, WhitelistEntry>>,
}

impl WhitelistRegistry {
    pub fn apply(&self, entries: &[WhitelistEntry]) -> Result<(), String> {
        let normalized = entries
            .iter()
            .cloned()
            .map(|mut entry| {
                entry.plate = normalize_plate(&entry.plate);
                (entry.plate.clone(), entry)
            })
            .collect();
        *self
            .entries
            .lock()
            .map_err(|_| "whitelist lock poisoned".to_string())? = normalized;
        Ok(())
    }

    pub fn contains(&self, plate: Option<&str>, now: DateTime<Utc>) -> Result<bool, String> {
        let Some(plate) = plate else {
            return Ok(false);
        };
        let entries = self
            .entries
            .lock()
            .map_err(|_| "whitelist lock poisoned".to_string())?;
        Ok(entries.get(&normalize_plate(plate)).is_some_and(|entry| {
            entry.enabled && entry.expires_at.is_none_or(|expires_at| expires_at > now)
        }))
    }

    pub fn snapshot(&self) -> Result<Vec<WhitelistEntry>, String> {
        self.entries
            .lock()
            .map(|entries| entries.values().cloned().collect())
            .map_err(|_| "whitelist lock poisoned".to_string())
    }
}

fn normalize_plate(plate: &str) -> String {
    plate.trim().to_uppercase()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CameraState {
    Registered,
    Online,
    Offline,
    Error,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct CameraConfigSync {
    pub camera_id: String,
    #[serde(default)]
    pub camera_name: String,
    #[serde(default)]
    pub site_id: String,
    #[serde(default)]
    pub space_id: String,
    #[serde(default)]
    pub node_id: Option<String>,
    #[serde(default)]
    pub protocol: String,
    #[serde(default)]
    pub stream_url: String,
    #[serde(default)]
    pub sip_device_id: String,
    #[serde(default)]
    pub resolution: String,
    #[serde(default)]
    pub codec: String,
    #[serde(default)]
    pub roi_json: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
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
    #[serde(default)]
    pub protocol: Option<String>,
    #[serde(default)]
    pub roi_json: Option<String>,
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
                    protocol: Some("RTSP".into()),
                    roi_json: None,
                });
            entry.space_id = binding.space_id.clone();
        }
        Ok(())
    }

    pub fn apply_configuration_cameras(&self, cameras: &[CameraConfigSync]) -> Result<(), String> {
        let mut channels = self
            .channels
            .lock()
            .map_err(|_| "camera channel lock poisoned".to_string())?;
        for cam in cameras.iter().filter(|c| c.enabled && !c.camera_id.trim().is_empty()) {
            let gb_id = if cam.sip_device_id.trim().is_empty() {
                cam.camera_id.clone()
            } else {
                cam.sip_device_id.clone()
            };
            let stream_url = if cam.stream_url.trim().is_empty() {
                None
            } else {
                Some(cam.stream_url.clone())
            };
            let roi = if cam.roi_json.trim().is_empty() {
                None
            } else {
                Some(cam.roi_json.clone())
            };
            let protocol = if cam.protocol.trim().is_empty() {
                "RTSP".to_string()
            } else {
                cam.protocol.clone()
            };

            channels
                .entry(cam.camera_id.clone())
                .and_modify(|entry| {
                    if !cam.space_id.trim().is_empty() {
                        entry.space_id = cam.space_id.clone();
                    }
                    entry.gb_device_id = gb_id.clone();
                    entry.stream_url = stream_url.clone();
                    entry.protocol = Some(protocol.clone());
                    entry.roi_json = roi.clone();
                })
                .or_insert_with(|| CameraChannel {
                    camera_id: cam.camera_id.clone(),
                    channel_id: cam.camera_id.clone(),
                    space_id: cam.space_id.clone(),
                    gb_device_id: gb_id,
                    state: CameraState::Registered,
                    stream_url,
                    last_heartbeat: None,
                    protocol: Some(protocol),
                    roi_json: roi,
                });
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
        if channel.protocol.is_none() {
            channel.protocol = Some("RTSP".into());
        }
        channels.insert(channel.camera_id.clone(), channel);
        Ok(())
    }

    pub fn remove(&self, camera_id: &str) -> Result<bool, String> {
        let mut channels = self
            .channels
            .lock()
            .map_err(|_| "camera channel lock poisoned".to_string())?;
        Ok(channels.remove(camera_id).is_some())
    }

    pub fn restore(&self, list: Vec<CameraChannel>) -> Result<(), String> {
        let mut channels = self
            .channels
            .lock()
            .map_err(|_| "camera channel lock poisoned".to_string())?;
        for cam in list {
            channels.insert(cam.camera_id.clone(), cam);
        }
        Ok(())
    }

    pub fn mark_stale_offline(
        &self,
        now: DateTime<Utc>,
        timeout_seconds: i64,
    ) -> Result<Vec<CameraChannel>, String> {
        let mut channels = self
            .channels
            .lock()
            .map_err(|_| "camera channel lock poisoned".to_string())?;
        let mut changed = Vec::new();
        for ch in channels.values_mut() {
            if ch.state == CameraState::Online {
                if let Some(last) = ch.last_heartbeat {
                    if (now - last).num_seconds() > timeout_seconds {
                        ch.state = CameraState::Offline;
                        changed.push(ch.clone());
                    }
                }
            }
        }
        Ok(changed)
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
    #[serde(default)]
    pub whitelist: Vec<WhitelistEntry>,
    #[serde(default)]
    pub cameras: Vec<CameraConfigSync>,
}

impl Default for PlatformConfiguration {
    fn default() -> Self {
        Self {
            version: 0,
            updated_at: Utc::now(),
            rules: RuleConfig::default(),
            bindings: Vec::new(),
            whitelist: Vec::new(),
            cameras: Vec::new(),
        }
    }
}

impl PlatformConfiguration {
    pub fn validate(&self) -> Result<(), String> {
        self.rules.validate()?;
        if self.bindings.len() > 64 {
            return Err(format!(
                "车位绑定数量 {} 超出范围，系统单机最多支持 64 路车位",
                self.bindings.len()
            ));
        }
        let mut spaces = std::collections::HashSet::new();
        let mut cameras = std::collections::HashSet::new();
        let mut chargers = std::collections::HashSet::new();
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
            if binding.enabled && !cameras.insert(binding.camera_id.clone()) {
                return Err(format!(
                    "cameraId {} is assigned to multiple enabled spaces",
                    binding.camera_id
                ));
            }
            if binding.enabled && !chargers.insert(binding.charger_id.clone()) {
                return Err(format!(
                    "chargerId {} is assigned to multiple enabled spaces",
                    binding.charger_id
                ));
            }
            if binding.enabled && !channels.insert(binding.relay_channel) {
                return Err(format!(
                    "继电器物理通道 {} 被多个启用的车位重复占用",
                    binding.relay_channel
                ));
            }
        }
        let mut plates = std::collections::HashSet::new();
        for entry in &self.whitelist {
            let plate = normalize_plate(&entry.plate);
            if plate.is_empty() {
                return Err("whitelist plate must not be blank".into());
            }
            if !plates.insert(plate.clone()) {
                return Err(format!("duplicate whitelist plate {plate}"));
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
    if input.vehicle_type == VehicleType::SpecialVehicle {
        return base(
            Decision::Compliant,
            None,
            false,
            "特种车辆合规停放(军/警车牌)",
        );
    }
    if input.vehicle_type == VehicleType::NonMotor {
        return base(
            Decision::Violation,
            Some("NON_MOTOR_VEHICLE".into()),
            true,
            "两轮车/非机动车占用充电车位",
        );
    }
    if input.vehicle_type == VehicleType::Obstacle {
        return base(
            Decision::Violation,
            Some("OBSTACLE_OCCUPATION".into()),
            true,
            "桌椅杂物非法占用充电车位",
        );
    }
    if input.vehicle_type == VehicleType::HeavyVehicle {
        return base(
            Decision::Violation,
            Some("HEAVY_VEHICLE".into()),
            true,
            "大客货车违规占用充电车位",
        );
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
    #[serde(default)]
    pub session_id: String,
    pub space_id: String,
    pub plate: Option<String>,
    pub vehicle_type: VehicleType,
    pub violation_type: Option<String>,
    pub occurred_at: DateTime<Utc>,
    pub evidence_url: Option<String>,
    #[serde(default)]
    pub status: EventStatus,
    #[serde(default)]
    pub cleared_at: Option<DateTime<Utc>>,
    #[serde(default = "Utc::now")]
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum EventStatus {
    Open,
    #[default]
    Alarming,
    Cleared,
    Acknowledged,
}

#[derive(Debug, Default)]
pub struct EventLifecycleRegistry {
    active_by_space: Mutex<HashMap<String, CloudEvent>>,
}

impl EventLifecycleRegistry {
    pub fn restore(&self, events: Vec<CloudEvent>) -> Result<(), String> {
        let restored = events
            .into_iter()
            .filter(|event| matches!(event.status, EventStatus::Open | EventStatus::Alarming))
            .map(|event| (event.space_id.clone(), event))
            .collect();
        *self
            .active_by_space
            .lock()
            .map_err(|_| "event lifecycle lock poisoned".to_string())? = restored;
        Ok(())
    }

    pub fn process_evaluation(
        &self,
        input: &EvaluationInput,
        result: &mut EvaluationResult,
        evidence_url: Option<String>,
    ) -> Result<Vec<CloudEvent>, String> {
        if result.decision != Decision::Violation {
            return self.clear_space(&input.space_id, input.now);
        }

        let session_id = format!(
            "{}:{}",
            input.space_id,
            input.occupied_since.timestamp_millis()
        );
        let mut active = self
            .active_by_space
            .lock()
            .map_err(|_| "event lifecycle lock poisoned".to_string())?;
        let mut changes = Vec::new();

        if let Some(existing) = active.get_mut(&input.space_id) {
            let same_occupancy = existing.session_id == session_id
                || (existing.plate == input.plate
                    && existing.violation_type == result.violation_type);
            if same_occupancy {
                result.event_id = existing.event_id;
                if existing.status == EventStatus::Acknowledged {
                    result.should_alarm = false;
                    return Ok(changes);
                }
                let new_evidence = existing.evidence_url.is_none() && evidence_url.is_some();
                let changed = existing.session_id != session_id
                    || existing.violation_type != result.violation_type
                    || existing.plate != input.plate
                    || new_evidence;
                if changed {
                    existing.session_id = session_id;
                    existing.violation_type = result.violation_type.clone();
                    existing.plate = input.plate.clone();
                    existing.vehicle_type = input.vehicle_type.clone();
                    if new_evidence {
                        existing.evidence_url = evidence_url;
                    }
                    existing.status = EventStatus::Alarming;
                    existing.updated_at = input.now;
                    changes.push(existing.clone());
                }
                return Ok(changes);
            }

            existing.status = EventStatus::Cleared;
            existing.cleared_at = Some(input.now);
            existing.updated_at = input.now;
            changes.push(existing.clone());
        }

        let event = CloudEvent {
            event_id: result.event_id,
            session_id,
            space_id: input.space_id.clone(),
            plate: input.plate.clone(),
            vehicle_type: input.vehicle_type.clone(),
            violation_type: result.violation_type.clone(),
            occurred_at: input.now,
            evidence_url,
            status: EventStatus::Alarming,
            cleared_at: None,
            updated_at: input.now,
        };
        active.insert(input.space_id.clone(), event.clone());
        changes.push(event);
        Ok(changes)
    }

    pub fn acknowledge_space(
        &self,
        space_id: &str,
        now: DateTime<Utc>,
    ) -> Result<Option<CloudEvent>, String> {
        let mut active = self
            .active_by_space
            .lock()
            .map_err(|_| "event lifecycle lock poisoned".to_string())?;
        if let Some(event) = active.get_mut(space_id) {
            event.status = EventStatus::Acknowledged;
            event.updated_at = now;
            Ok(Some(event.clone()))
        } else {
            Ok(None)
        }
    }

    pub fn acknowledge_event_by_id(
        &self,
        event_id: Uuid,
        now: DateTime<Utc>,
    ) -> Result<Option<CloudEvent>, String> {
        let mut active = self
            .active_by_space
            .lock()
            .map_err(|_| "event lifecycle lock poisoned".to_string())?;
        for event in active.values_mut() {
            if event.event_id == event_id {
                event.status = EventStatus::Acknowledged;
                event.updated_at = now;
                return Ok(Some(event.clone()));
            }
        }
        Ok(None)
    }

    pub fn clear_space(
        &self,
        space_id: &str,
        now: DateTime<Utc>,
    ) -> Result<Vec<CloudEvent>, String> {
        let mut active = self
            .active_by_space
            .lock()
            .map_err(|_| "event lifecycle lock poisoned".to_string())?;
        let Some(mut event) = active.remove(space_id) else {
            return Ok(Vec::new());
        };
        event.status = EventStatus::Cleared;
        event.cleared_at = Some(now);
        event.updated_at = now;
        Ok(vec![event])
    }

    pub fn prune_unbound_events(
        &self,
        active_space_ids: &[String],
        now: DateTime<Utc>,
    ) -> Result<Vec<CloudEvent>, String> {
        let mut active = self
            .active_by_space
            .lock()
            .map_err(|_| "event lifecycle lock poisoned".to_string())?;
        let active_set: std::collections::HashSet<&str> =
            active_space_ids.iter().map(|s| s.as_str()).collect();
        let mut cleared = Vec::new();
        active.retain(|space_id, event| {
            if !active_set.contains(space_id.as_str()) {
                let mut ev = event.clone();
                ev.status = EventStatus::Cleared;
                ev.cleared_at = Some(now);
                ev.updated_at = now;
                cleared.push(ev);
                false
            } else {
                true
            }
        });
        Ok(cleared)
    }

    pub fn active_event(&self, space_id: &str) -> Result<Option<CloudEvent>, String> {
        self.active_by_space
            .lock()
            .map(|events| events.get(space_id).cloned())
            .map_err(|_| "event lifecycle lock poisoned".to_string())
    }

    pub fn active_events(&self) -> Result<Vec<CloudEvent>, String> {
        self.active_by_space
            .lock()
            .map(|events| events.values().cloned().collect())
            .map_err(|_| "event lifecycle lock poisoned".to_string())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpaceRuntimeStatus {
    pub node_id: String,
    pub site_id: String,
    pub space_id: String,
    pub phase: PresencePhase,
    pub charger_status: ChargerStatus,
    pub plate: Option<String>,
    pub vehicle_type: VehicleType,
    pub alarming: bool,
    pub active_event_id: Option<Uuid>,
    pub observed_at: DateTime<Utc>,
    #[serde(default)]
    pub is_simulated: bool,
    #[serde(default)]
    pub stream_type: Option<String>,
    #[serde(default)]
    pub snapshot_url: Option<String>,
    #[serde(default)]
    pub confidence: f64,
}

#[derive(Debug, Default)]
pub struct SpaceStatusRegistry {
    statuses: Mutex<HashMap<String, SpaceRuntimeStatus>>,
}

impl SpaceStatusRegistry {
    pub fn apply_bindings(&self, node_id: &str, bindings: &[DeviceBinding]) -> Result<(), String> {
        let mut statuses = self
            .statuses
            .lock()
            .map_err(|_| "space status lock poisoned".to_string())?;
        let mut replacement = HashMap::new();
        for binding in bindings.iter().filter(|binding| binding.enabled) {
            let status =
                statuses
                    .get(&binding.space_id)
                    .cloned()
                    .unwrap_or_else(|| SpaceRuntimeStatus {
                        node_id: node_id.to_string(),
                        site_id: binding.site_id.clone(),
                        space_id: binding.space_id.clone(),
                        phase: PresencePhase::Empty,
                        charger_status: ChargerStatus::Idle,
                        plate: None,
                        vehicle_type: VehicleType::Unknown,
                        alarming: false,
                        active_event_id: None,
                        observed_at: Utc::now(),
                        is_simulated: false,
                        stream_type: None,
                        snapshot_url: None,
                        confidence: 0.0,
                    });
            replacement.insert(
                binding.space_id.clone(),
                SpaceRuntimeStatus {
                    node_id: node_id.to_string(),
                    site_id: binding.site_id.clone(),
                    ..status
                },
            );
        }
        *statuses = replacement;
        Ok(())
    }

    pub fn restore(&self, list: Vec<SpaceRuntimeStatus>) -> Result<(), String> {
        let mut statuses = self
            .statuses
            .lock()
            .map_err(|_| "space status lock poisoned".to_string())?;
        for st in list {
            statuses.insert(st.space_id.clone(), st);
        }
        Ok(())
    }

    pub fn update(&self, status: SpaceRuntimeStatus) -> Result<(), String> {
        self.statuses
            .lock()
            .map_err(|_| "space status lock poisoned".to_string())?
            .insert(status.space_id.clone(), status);
        Ok(())
    }

    pub fn snapshot(&self) -> Result<Vec<SpaceRuntimeStatus>, String> {
        self.statuses
            .lock()
            .map(|statuses| statuses.values().cloned().collect())
            .map_err(|_| "space status lock poisoned".to_string())
    }
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
    fn whitelist_registry_normalizes_plate_and_honors_expiry() {
        let now = Utc::now();
        let registry = WhitelistRegistry::default();
        registry
            .apply(&[
                WhitelistEntry {
                    plate: " 粤a12345 ".into(),
                    vehicle_type: None,
                    reason: None,
                    added_at: now,
                    expires_at: Some(now + chrono::Duration::minutes(1)),
                    enabled: true,
                },
                WhitelistEntry {
                    plate: "粤B00001".into(),
                    vehicle_type: None,
                    reason: None,
                    added_at: now,
                    expires_at: Some(now - chrono::Duration::seconds(1)),
                    enabled: true,
                },
            ])
            .unwrap();
        assert!(registry.contains(Some("粤A12345"), now).unwrap());
        assert!(!registry.contains(Some("粤B00001"), now).unwrap());
        assert!(!registry.contains(None, now).unwrap());
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
        relay.register_space("A-01", 0).unwrap();
        relay.register_space("A-02", 1).unwrap();
        relay.set_alarm("A-01", true).unwrap();
        relay.set_alarm("A-02", false).unwrap();
        let states = relay.snapshot().unwrap();
        assert_eq!(states.get("A-01"), Some(&true));
        assert_eq!(states.get("A-02"), Some(&false));
    }

    #[test]
    fn relay_bindings_are_replaced_on_configuration_update() {
        let relay = MemoryRelayController::default();
        let binding = |space: &str, channel| DeviceBinding {
            site_id: "site".into(),
            space_id: space.into(),
            camera_id: format!("camera-{space}"),
            charger_id: format!("charger-{space}"),
            relay_channel: channel,
            speaker_channel: None,
            enabled: true,
            ..Default::default()
        };
        relay.apply_bindings(&[binding("A-01", 0)]).unwrap();
        relay.set_alarm("A-01", true).unwrap();
        relay.apply_bindings(&[binding("B-01", 7)]).unwrap();
        assert!(relay.set_alarm("A-01", true).is_err());
        assert_eq!(relay.detailed_snapshot().unwrap()["B-01"].channel, 7);
    }

    #[test]
    fn lifecycle_deduplicates_and_clears_a_single_occupancy_session() {
        let lifecycle = EventLifecycleRegistry::default();
        let mut first_input = input(VehicleType::Fuel, ChargerStatus::Idle, 5);
        let mut first_result = evaluate(&first_input, &RuleConfig::default());
        let opened = lifecycle
            .process_evaluation(&first_input, &mut first_result, None)
            .unwrap();
        assert_eq!(opened.len(), 1);
        assert_eq!(opened[0].status, EventStatus::Alarming);

        first_input.now += chrono::Duration::seconds(2);
        let mut repeated_result = evaluate(&first_input, &RuleConfig::default());
        let repeated = lifecycle
            .process_evaluation(&first_input, &mut repeated_result, None)
            .unwrap();
        assert!(repeated.is_empty());
        assert_eq!(repeated_result.event_id, first_result.event_id);

        let cleared = lifecycle
            .clear_space(&first_input.space_id, first_input.now)
            .unwrap();
        assert_eq!(cleared.len(), 1);
        assert_eq!(cleared[0].event_id, first_result.event_id);
        assert_eq!(cleared[0].status, EventStatus::Cleared);
        assert!(lifecycle
            .active_event(&first_input.space_id)
            .unwrap()
            .is_none());
    }

    #[test]
    fn lifecycle_restores_active_event_without_creating_duplicate() {
        let lifecycle = EventLifecycleRegistry::default();
        let now = Utc::now();
        let existing = CloudEvent {
            event_id: Uuid::new_v4(),
            session_id: "A-01:old".into(),
            space_id: "A-01".into(),
            plate: Some("粤A12345".into()),
            vehicle_type: VehicleType::Fuel,
            violation_type: Some("FUEL_VEHICLE".into()),
            occurred_at: now - chrono::Duration::minutes(2),
            evidence_url: None,
            status: EventStatus::Alarming,
            cleared_at: None,
            updated_at: now - chrono::Duration::minutes(1),
        };
        lifecycle.restore(vec![existing.clone()]).unwrap();
        let input = input(VehicleType::Fuel, ChargerStatus::Idle, 10);
        let mut result = evaluate(&input, &RuleConfig::default());
        let changes = lifecycle
            .process_evaluation(&input, &mut result, None)
            .unwrap();
        assert_eq!(result.event_id, existing.event_id);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].event_id, existing.event_id);
    }

    #[test]
    fn space_status_registry_rebinds_without_stale_spaces() {
        let registry = SpaceStatusRegistry::default();
        let binding = |space: &str| DeviceBinding {
            site_id: "site".into(),
            space_id: space.into(),
            camera_id: format!("camera-{space}"),
            charger_id: format!("charger-{space}"),
            relay_channel: 0,
            speaker_channel: None,
            enabled: true,
            ..Default::default()
        };
        registry
            .apply_bindings("edge-01", &[binding("A-01")])
            .unwrap();
        registry
            .apply_bindings("edge-01", &[binding("B-01")])
            .unwrap();
        let statuses = registry.snapshot().unwrap();
        assert_eq!(statuses.len(), 1);
        assert_eq!(statuses[0].space_id, "B-01");
    }

    #[test]
    fn rejects_invalid_rule_configuration() {
        let rules = RuleConfig {
            completed_grace_seconds: -1,
            ..RuleConfig::default()
        };
        assert!(rules.validate().is_err());
        let rules = RuleConfig {
            completed_grace_seconds: 10,
            fault_status_action: "INVALID".into(),
            ..RuleConfig::default()
        };
        assert!(rules.validate().is_err());
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
        assert!(configuration.bindings.is_empty());
        configuration.version = 4;
        configuration.bindings.push(DeviceBinding {
            site_id: "site-1".into(),
            space_id: "B-01".into(),
            camera_id: "cam-B-01".into(),
            charger_id: "chg-B-01".into(),
            relay_channel: 0,
            speaker_channel: None,
            enabled: true,
            ..Default::default()
        });
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
            speaker_channel: None,
            enabled: true,
            ..Default::default()
        };
        manager
            .apply_bindings(std::slice::from_ref(&first))
            .unwrap();
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
                speaker_channel: None,
                enabled: true,
                ..Default::default()
            })
            .collect::<Vec<_>>();
        assert!(manager.apply_bindings(&bindings).is_err());
    }

    #[test]
    fn memory_charger_adapter_maps_binding_and_updates_status() {
        let adapter = MemoryChargerAdapter::default();
        let binding = DeviceBinding {
            site_id: "site".into(),
            space_id: "A-01".into(),
            camera_id: "cam-01".into(),
            charger_id: "charger-01".into(),
            relay_channel: 0,
            speaker_channel: None,
            enabled: true,
            ..Default::default()
        };
        adapter.apply_bindings(&[binding]).unwrap();
        let mut snapshot = adapter.snapshot("A-01").unwrap();
        assert_eq!(snapshot.status, ChargerStatus::Idle);
        snapshot.status = ChargerStatus::Charging;
        snapshot.observed_at = Utc::now();
        adapter.update(snapshot).unwrap();
        assert_eq!(
            adapter.snapshot("A-01").unwrap().status,
            ChargerStatus::Charging
        );
    }
}
