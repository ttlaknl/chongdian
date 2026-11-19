use crate::{ChargerStatus, EvaluationInput, RuleConfig, VehicleType};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Mutex;

/// 车位在位状态机阶段
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PresencePhase {
    /// 车位空闲
    Empty,
    /// 车辆驶入中（防抖确认中）
    Entering,
    /// 车辆已确认在位
    Occupied,
    /// 车辆可能驶离（离场防抖确认中）
    Leaving,
}

fn default_data_source() -> String {
    "REAL_STREAM".to_string()
}

/// 相机或视频分析算法上报的原始识别帧数据
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RawDetection {
    pub space_id: String,
    pub has_vehicle: bool,
    pub plate: Option<String>,
    pub vehicle_type: VehicleType,
    pub captured_at: DateTime<Utc>,
    /// 抓拍图片相对路径或 URL (若有)
    pub evidence_image_path: Option<String>,
    #[serde(default)]
    pub is_simulated: bool,
    #[serde(default = "default_data_source")]
    pub data_source: String,
    #[serde(default)]
    pub confidence: Option<f64>,
    #[serde(default)]
    pub plate_color: Option<String>,
    #[serde(default)]
    pub camera_id: Option<String>,
    #[serde(default)]
    pub image_base64: Option<String>,
}

impl Default for RawDetection {
    fn default() -> Self {
        Self {
            space_id: String::new(),
            has_vehicle: false,
            plate: None,
            vehicle_type: VehicleType::Unknown,
            captured_at: Utc::now(),
            evidence_image_path: None,
            is_simulated: false,
            data_source: "REAL_STREAM".to_string(),
            confidence: None,
            plate_color: None,
            camera_id: None,
            image_base64: None,
        }
    }
}

/// 状态机转换事件输出
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransitionEvent {
    /// 无状态跳变
    None,
    /// 车辆正式入场确认（进场防抖通过）
    VehicleEntered {
        occupied_since: DateTime<Utc>,
        plate: Option<String>,
        vehicle_type: VehicleType,
    },
    /// 车辆正式驶离确认（离场防抖通过）
    VehicleLeft {
        occupied_duration_seconds: i64,
        plate: Option<String>,
        vehicle_type: VehicleType,
    },
}

/// 单个车位的在位与防抖状态跟踪器
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpacePresenceTracker {
    pub space_id: String,
    pub phase: PresencePhase,
    pub candidate_plate: Option<String>,
    pub candidate_vehicle_type: VehicleType,
    pub candidate_enter_at: Option<DateTime<Utc>>,
    pub confirmed_enter_at: Option<DateTime<Utc>>,
    pub candidate_leave_at: Option<DateTime<Utc>>,
    pub last_evidence_path: Option<String>,
    #[serde(default)]
    pub last_confidence: f64,
    #[serde(default)]
    pub last_plate_color: Option<String>,
}

impl SpacePresenceTracker {
    pub fn new(space_id: String) -> Self {
        Self {
            space_id,
            phase: PresencePhase::Empty,
            candidate_plate: None,
            candidate_vehicle_type: VehicleType::Unknown,
            candidate_enter_at: None,
            confirmed_enter_at: None,
            candidate_leave_at: None,
            last_evidence_path: None,
            last_confidence: 0.0,
            last_plate_color: None,
        }
    }

    /// 接收检测帧，执行进出场防抖计算
    pub fn update(&mut self, detection: &RawDetection, rules: &RuleConfig) -> TransitionEvent {
        let now = detection.captured_at;
        if let Some(path) = &detection.evidence_image_path {
            self.last_evidence_path = Some(path.clone());
        }

        if detection.has_vehicle {
            if let Some(conf) = detection.confidence {
                self.last_confidence = conf;
            }
            if detection.plate_color.is_some() {
                self.last_plate_color = detection.plate_color.clone();
            }
            // 当前帧检测到有车
            match self.phase {
                PresencePhase::Empty => {
                    // 从空闲转为进场防抖观察中
                    self.phase = PresencePhase::Entering;
                    self.candidate_enter_at = Some(now);
                    self.candidate_plate = detection.plate.clone();
                    self.candidate_vehicle_type = detection.vehicle_type.clone();
                    self.candidate_leave_at = None;
                    TransitionEvent::None
                }
                PresencePhase::Entering => {
                    // 进场观察期内持续更新更高置信度的车牌/车型识别结果
                    if detection.plate.is_some() {
                        self.candidate_plate = detection.plate.clone();
                    }
                    if detection.vehicle_type != VehicleType::Unknown {
                        self.candidate_vehicle_type = detection.vehicle_type.clone();
                    }
                    // 检查是否已经满足进场防抖时长
                    let enter_start = self.candidate_enter_at.unwrap_or(now);
                    if (now - enter_start).num_seconds() >= rules.vehicle_enter_confirm_seconds {
                        self.phase = PresencePhase::Occupied;
                        self.confirmed_enter_at = Some(enter_start);
                        TransitionEvent::VehicleEntered {
                            occupied_since: enter_start,
                            plate: self.candidate_plate.clone(),
                            vehicle_type: self.candidate_vehicle_type.clone(),
                        }
                    } else {
                        TransitionEvent::None
                    }
                }
                PresencePhase::Occupied => {
                    // 持续在位，若之前有离场候选，消除抖动重置
                    self.candidate_leave_at = None;
                    if let Some(ref p) = detection.plate {
                        if self.candidate_plate.as_ref() != Some(p) {
                            self.last_evidence_path = None;
                        }
                        self.candidate_plate = Some(p.clone());
                    }
                    if detection.vehicle_type != VehicleType::Unknown {
                        self.candidate_vehicle_type = detection.vehicle_type.clone();
                    }
                    TransitionEvent::None
                }
                PresencePhase::Leaving => {
                    // 离场候选期内又检测到车辆，判定为短暂遮挡闪断，恢复为已在位
                    self.phase = PresencePhase::Occupied;
                    self.candidate_leave_at = None;
                    TransitionEvent::None
                }
            }
        } else {
            // 当前帧未检测到车
            match self.phase {
                PresencePhase::Empty => TransitionEvent::None,
                PresencePhase::Entering => {
                    // 若此前已识别到车牌，视为测试或短暂停留驶离；否则判定为虚警重置
                    let had_plate = self.candidate_plate.clone();
                    let had_vtype = self.candidate_vehicle_type.clone();
                    self.phase = PresencePhase::Empty;
                    self.candidate_enter_at = None;
                    self.candidate_plate = None;
                    self.candidate_vehicle_type = VehicleType::Unknown;
                    self.last_confidence = 0.0;
                    self.last_plate_color = None;
                    self.last_evidence_path = None;
                    if had_plate.is_some() {
                        TransitionEvent::VehicleLeft {
                            occupied_duration_seconds: 0,
                            plate: had_plate,
                            vehicle_type: had_vtype,
                        }
                    } else {
                        TransitionEvent::None
                    }
                }
                PresencePhase::Occupied => {
                    // 已在位车辆首次丢失视野，转入离场防抖观察中
                    self.phase = PresencePhase::Leaving;
                    self.candidate_leave_at = Some(now);
                    TransitionEvent::None
                }
                PresencePhase::Leaving => {
                    // 检查是否已经满足离场防抖时长
                    let leave_start = self.candidate_leave_at.unwrap_or(now);
                    if (now - leave_start).num_seconds() >= rules.vehicle_leave_confirm_seconds {
                        let duration = self
                            .confirmed_enter_at
                            .map(|t| (now - t).num_seconds())
                            .unwrap_or(0);
                        let prev_plate = self.candidate_plate.clone();
                        let prev_vtype = self.candidate_vehicle_type.clone();
                        self.phase = PresencePhase::Empty;
                        self.candidate_enter_at = None;
                        self.confirmed_enter_at = None;
                        self.candidate_leave_at = None;
                        self.candidate_plate = None;
                        self.candidate_vehicle_type = VehicleType::Unknown;
                        self.last_evidence_path = None;
                        self.last_confidence = 0.0;
                        self.last_plate_color = None;
                        TransitionEvent::VehicleLeft {
                            occupied_duration_seconds: duration,
                            plate: prev_plate,
                            vehicle_type: prev_vtype,
                        }
                    } else {
                        TransitionEvent::None
                    }
                }
            }
        }
    }

    /// 构造规则判定输入（仅当处于 Occupied 或 Leaving 阶段时有效）
    pub fn build_evaluation_input(
        &self,
        now: DateTime<Utc>,
        charger_status: ChargerStatus,
        completed_at: Option<DateTime<Utc>>,
        whitelisted: bool,
    ) -> Option<EvaluationInput> {
        let occupied_since = self.confirmed_enter_at?;
        Some(EvaluationInput {
            space_id: self.space_id.clone(),
            plate: self.candidate_plate.clone(),
            vehicle_type: self.candidate_vehicle_type.clone(),
            charger_status,
            occupied_since,
            charge_completed_at: completed_at,
            now,
            whitelisted,
        })
    }
}

/// 多车位在位跟踪管理器
pub struct SpaceTrackerRegistry {
    trackers: Mutex<HashMap<String, SpacePresenceTracker>>,
}

impl Default for SpaceTrackerRegistry {
    fn default() -> Self {
        Self {
            trackers: Mutex::new(HashMap::new()),
        }
    }
}

impl SpaceTrackerRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn process_detection(
        &self,
        detection: &RawDetection,
        rules: &RuleConfig,
    ) -> Result<TransitionEvent, String> {
        let mut trackers = self
            .trackers
            .lock()
            .map_err(|_| "tracker lock poisoned".to_string())?;
        let tracker = trackers
            .entry(detection.space_id.clone())
            .or_insert_with(|| SpacePresenceTracker::new(detection.space_id.clone()));
        Ok(tracker.update(detection, rules))
    }

    pub fn get_tracker(&self, space_id: &str) -> Result<Option<SpacePresenceTracker>, String> {
        let trackers = self
            .trackers
            .lock()
            .map_err(|_| "tracker lock poisoned".to_string())?;
        Ok(trackers.get(space_id).cloned())
    }

    pub fn update_evidence_path(&self, space_id: &str, path: String) -> Result<(), String> {
        let mut trackers = self
            .trackers
            .lock()
            .map_err(|_| "tracker lock poisoned".to_string())?;
        if let Some(tr) = trackers.get_mut(space_id) {
            tr.last_evidence_path = Some(path);
        }
        Ok(())
    }

    pub fn prune_unbound_trackers(&self, active_space_ids: &[String]) -> Result<(), String> {
        let mut trackers = self
            .trackers
            .lock()
            .map_err(|_| "tracker lock poisoned".to_string())?;
        let active_set: std::collections::HashSet<&str> =
            active_space_ids.iter().map(|s| s.as_str()).collect();
        trackers.retain(|space_id, _| active_set.contains(space_id.as_str()));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    #[test]
    fn test_enter_debounce_logic() {
        let mut tracker = SpacePresenceTracker::new("A-01".into());
        let rules = RuleConfig::default(); // vehicleEnterConfirmSeconds = 3
        let t0 = Utc::now();

        // 0s: 车辆首次出现
        let d0 = RawDetection {
            space_id: "A-01".into(),
            has_vehicle: true,
            plate: Some("粤A12345".into()),
            vehicle_type: VehicleType::Fuel,
            captured_at: t0,
            evidence_image_path: None,
            is_simulated: false,
            data_source: "REAL_STREAM".to_string(),
            confidence: Some(0.95),
            plate_color: Some("BLUE".into()),
            camera_id: None,
            image_base64: None,
        };
        assert_eq!(tracker.update(&d0, &rules), TransitionEvent::None);
        assert_eq!(tracker.phase, PresencePhase::Entering);

        // 2s: 车辆依然在，但未满 3 秒防抖，不产生进入事件
        let d1 = RawDetection {
            captured_at: t0 + Duration::seconds(2),
            ..d0.clone()
        };
        assert_eq!(tracker.update(&d1, &rules), TransitionEvent::None);
        assert_eq!(tracker.phase, PresencePhase::Entering);

        // 3s: 满 3 秒，正式触发 VehicleEntered
        let d2 = RawDetection {
            captured_at: t0 + Duration::seconds(3),
            ..d0
        };
        let event = tracker.update(&d2, &rules);
        assert_eq!(tracker.phase, PresencePhase::Occupied);
        assert!(matches!(event, TransitionEvent::VehicleEntered { .. }));
    }

    #[test]
    fn test_leave_debounce_and_recovery() {
        let mut tracker = SpacePresenceTracker::new("A-01".into());
        let rules = RuleConfig::default(); // vehicleLeaveConfirmSeconds = 5
        let t0 = Utc::now();

        // 快速确认在位
        let d_enter = RawDetection {
            space_id: "A-01".into(),
            has_vehicle: true,
            plate: Some("粤B66666".into()),
            vehicle_type: VehicleType::NewEnergy,
            captured_at: t0,
            evidence_image_path: None,
            is_simulated: false,
            data_source: "REAL_STREAM".to_string(),
            confidence: Some(0.96),
            plate_color: Some("GREEN".into()),
            camera_id: None,
            image_base64: None,
        };
        tracker.update(&d_enter, &rules);
        tracker.update(
            &RawDetection {
                captured_at: t0 + Duration::seconds(3),
                ..d_enter.clone()
            },
            &rules,
        );
        assert_eq!(tracker.phase, PresencePhase::Occupied);

        // t0+10s: 突然未检测到（视野闪断）
        let d_miss = RawDetection {
            space_id: "A-01".into(),
            has_vehicle: false,
            plate: None,
            vehicle_type: VehicleType::Unknown,
            captured_at: t0 + Duration::seconds(10),
            evidence_image_path: None,
            is_simulated: false,
            data_source: "REAL_STREAM".to_string(),
            confidence: None,
            plate_color: None,
            camera_id: None,
            image_base64: None,
        };
        assert_eq!(tracker.update(&d_miss, &rules), TransitionEvent::None);
        assert_eq!(tracker.phase, PresencePhase::Leaving);

        // t0+12s: 2 秒后车辆再次被看到（恢复在位，消除离场假警）
        let d_recover = RawDetection {
            captured_at: t0 + Duration::seconds(12),
            ..d_enter.clone()
        };
        assert_eq!(tracker.update(&d_recover, &rules), TransitionEvent::None);
        assert_eq!(tracker.phase, PresencePhase::Occupied);

        // t0+20s: 车辆真正驶离，5 秒后确认 VehicleLeft
        tracker.update(
            &RawDetection {
                captured_at: t0 + Duration::seconds(20),
                ..d_miss.clone()
            },
            &rules,
        );
        let left_event = tracker.update(
            &RawDetection {
                captured_at: t0 + Duration::seconds(25),
                ..d_miss
            },
            &rules,
        );
        assert_eq!(tracker.phase, PresencePhase::Empty);
        assert!(matches!(left_event, TransitionEvent::VehicleLeft { .. }));
    }
}
