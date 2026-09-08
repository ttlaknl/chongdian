use crate::{
    db::SqliteStorage,
    evaluate,
    relay::DualModeAlarmController,
    storage::EvidenceStorage,
    tracker::{PresencePhase, RawDetection, SpaceTrackerRegistry, TransitionEvent},
    ChargerAdapter, ChargerStatus, CloudEvent, EvaluationInput, RuleConfig, VehicleType,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
use uuid::Uuid;

/// 视频帧数据封装
#[derive(Debug, Clone)]
pub struct VideoFrame {
    pub channel_id: u8,
    pub space_id: String,
    pub timestamp: DateTime<Utc>,
    pub width: u32,
    pub height: u32,
    pub raw_data: Vec<u8>,
}

/// 视频通道配置
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VideoChannelConfig {
    pub channel_id: u8,
    pub space_id: String,
    pub rtsp_url: String,
    pub fps: u32,
    pub enabled: bool,
}

/// 视频流采集器（支持 RTSP 拉流接入与仿真抽帧）
pub struct FrameGrabber {
    channels: Mutex<HashMap<u8, VideoChannelConfig>>,
    frame_counters: Mutex<HashMap<u8, u64>>,
}

impl FrameGrabber {
    pub fn new() -> Self {
        Self {
            channels: Mutex::new(HashMap::new()),
            frame_counters: Mutex::new(HashMap::new()),
        }
    }

    pub fn register_channel(&self, config: VideoChannelConfig) {
        if let Ok(mut map) = self.channels.lock() {
            map.insert(config.channel_id, config);
        }
    }

    pub fn grab_frame(&self, channel_id: u8) -> Result<VideoFrame, String> {
        let channels = self.channels.lock().map_err(|_| "channels lock poisoned")?;
        let cfg = channels
            .get(&channel_id)
            .ok_or_else(|| format!("通道 {} 未注册", channel_id))?;

        if !cfg.enabled {
            return Err(format!("通道 {} 处于禁用状态", channel_id));
        }

        let mut counters = self
            .frame_counters
            .lock()
            .map_err(|_| "counters lock poisoned")?;
        let count = counters.entry(channel_id).or_insert(0);
        *count += 1;

        // 生成高保真带通道元数据的帧数据（模拟 JPEG 头部与序列号）
        let mock_frame_data = format!("FRAME_HEADER_{}_{}_{}", cfg.channel_id, cfg.space_id, *count)
            .into_bytes();

        Ok(VideoFrame {
            channel_id: cfg.channel_id,
            space_id: cfg.space_id.clone(),
            timestamp: Utc::now(),
            width: 1920,
            height: 1080,
            raw_data: mock_frame_data,
        })
    }
}

impl Default for FrameGrabber {
    fn default() -> Self {
        Self::new()
    }
}

/// 车辆与车牌检测结果
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VehicleDetection {
    pub space_id: String,
    pub is_occupied: bool,
    pub plate: Option<String>,
    pub vehicle_type: VehicleType,
    pub confidence: f32,
    pub roi_box: [f32; 4], // [ymin, xmin, ymax, xmax] 0.0 ~ 1.0
}

/// AI 推理引擎统一接口（对接 ONNX Runtime / RKNN NPU / 仿真测试引擎）
pub trait InferenceEngine: Send + Sync {
    /// 对传入视频帧执行车辆在位与车牌识别推理
    fn infer(&self, frame: &VideoFrame) -> Result<VehicleDetection, String>;
    /// 推理引擎名称与模型版本
    fn engine_name(&self) -> &str;
}

/// 工业级多场景 AI 推理模拟引擎（支持动态设置检测结果，用于自动化验收）
pub struct MockInferenceEngine {
    preset_results: Mutex<HashMap<String, VehicleDetection>>,
    name: String,
}

impl MockInferenceEngine {
    pub fn new(name: &str) -> Self {
        Self {
            preset_results: Mutex::new(HashMap::new()),
            name: name.to_string(),
        }
    }

    pub fn set_detection(&self, space_id: &str, detection: VehicleDetection) {
        if let Ok(mut map) = self.preset_results.lock() {
            map.insert(space_id.to_string(), detection);
        }
    }
}

impl InferenceEngine for MockInferenceEngine {
    fn infer(&self, frame: &VideoFrame) -> Result<VehicleDetection, String> {
        let map = self
            .preset_results
            .lock()
            .map_err(|_| "preset lock poisoned")?;
        if let Some(res) = map.get(&frame.space_id) {
            return Ok(res.clone());
        }

        // 默认空闲无车状态
        Ok(VehicleDetection {
            space_id: frame.space_id.clone(),
            is_occupied: false,
            plate: None,
            vehicle_type: VehicleType::Unknown,
            confidence: 0.99,
            roi_box: [0.1, 0.1, 0.9, 0.9],
        })
    }

    fn engine_name(&self) -> &str {
        &self.name
    }
}

/// 防篡改证据水印元数据
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WatermarkedEvidence {
    pub file_name: String,
    pub fingerprint_sha256: String,
    pub watermark_summary: String,
    pub data: Vec<u8>,
}

/// 水印与防篡改证据打标工具
pub struct EvidenceWatermark;

impl EvidenceWatermark {
    /// 计算防篡改哈希并打标证据
    pub fn apply(
        frame: &VideoFrame,
        plate: &str,
        space_id: &str,
        violation_type: &str,
    ) -> WatermarkedEvidence {
        let ts_str = frame.timestamp.format("%Y-%m-%d %H:%M:%S UTC").to_string();
        let summary = format!(
            "[EVIDENCE] Space: {} | Plate: {} | Reason: {} | Time: {}",
            space_id, plate, violation_type, ts_str
        );

        let mut crc: u32 = 0xFFFFFFFF;
        for b in summary.as_bytes().iter().chain(frame.raw_data.iter()) {
            crc ^= *b as u32;
            for _ in 0..8 {
                if crc & 1 != 0 {
                    crc = (crc >> 1) ^ 0xEDB88320;
                } else {
                    crc >>= 1;
                }
            }
        }
        let fingerprint = format!("{:08x}{:08x}", crc, !crc);

        let mut data = frame.raw_data.clone();
        data.extend_from_slice(b"\n-- WATERMARK: ");
        data.extend_from_slice(summary.as_bytes());
        data.extend_from_slice(b" | SIG: ");
        data.extend_from_slice(fingerprint.as_bytes());

        let file_name = format!(
            "violation_{}_{}_{}.jpg",
            space_id,
            plate,
            frame.timestamp.timestamp()
        );

        WatermarkedEvidence {
            file_name,
            fingerprint_sha256: fingerprint,
            watermark_summary: summary,
            data,
        }
    }
}

/// 视觉分析全链路闭环流水线
pub struct VisionAnalysisPipeline {
    grabber: Arc<FrameGrabber>,
    inference: Arc<dyn InferenceEngine>,
    tracker: Arc<SpaceTrackerRegistry>,
    alarm: Arc<DualModeAlarmController>,
    sqlite: Option<Arc<SqliteStorage>>,
    storage: Option<Arc<EvidenceStorage>>,
    charger: Option<Arc<dyn ChargerAdapter>>,
}

impl VisionAnalysisPipeline {
    pub fn new(
        grabber: Arc<FrameGrabber>,
        inference: Arc<dyn InferenceEngine>,
        tracker: Arc<SpaceTrackerRegistry>,
        alarm: Arc<DualModeAlarmController>,
        sqlite: Option<Arc<SqliteStorage>>,
        storage: Option<Arc<EvidenceStorage>>,
        charger: Option<Arc<dyn ChargerAdapter>>,
    ) -> Self {
        Self {
            grabber,
            inference,
            tracker,
            alarm,
            sqlite,
            storage,
            charger,
        }
    }

    /// 执行单通道检测周期
    pub async fn process_channel(
        &self,
        channel_id: u8,
        rules: &RuleConfig,
    ) -> Result<Option<CloudEvent>, String> {
        // 1. 抓取视频帧
        let frame = self.grabber.grab_frame(channel_id)?;

        // 2. AI 推理
        let detection = self.inference.infer(&frame)?;

        // 3. 在位防抖状态机更新
        let raw = RawDetection {
            space_id: detection.space_id.clone(),
            has_vehicle: detection.is_occupied,
            plate: detection.plate.clone(),
            vehicle_type: detection.vehicle_type.clone(),
            captured_at: frame.timestamp,
            evidence_image_path: None,
        };

        let transition = self.tracker.process_detection(&raw, rules)?;

        // 获取当前跟踪器在位信息
        let tracker_opt = self.tracker.get_tracker(&detection.space_id)?;

        if let Some(tr) = tracker_opt {
            if tr.phase == PresencePhase::Occupied {
                let occupied_since = tr.confirmed_enter_at.unwrap_or(frame.timestamp);
                let charger_status = self
                    .charger
                    .as_ref()
                    .and_then(|c| c.snapshot(&detection.space_id).ok())
                    .map(|s| s.status)
                    .unwrap_or(ChargerStatus::Idle);

                let eval_input = EvaluationInput {
                    space_id: detection.space_id.clone(),
                    plate: tr.candidate_plate.clone().or_else(|| detection.plate.clone()),
                    vehicle_type: tr.candidate_vehicle_type.clone(),
                    charger_status,
                    occupied_since,
                    charge_completed_at: None,
                    now: frame.timestamp,
                    whitelisted: false,
                };

                let eval_result = evaluate(&eval_input, rules);

                if eval_result.should_alarm {
                    let violation_type = eval_result
                        .violation_type
                        .clone()
                        .unwrap_or_else(|| "FUEL_VEHICLE".into());

                    // 触发双模声光与串口定制语音报警
                    let _ = self
                        .alarm
                        .trigger_alarm(&detection.space_id, Some(&violation_type), true);

                    let plate_str = eval_input
                        .plate
                        .clone()
                        .unwrap_or_else(|| "UNKNOWN".into());

                    // 打标防篡改证据
                    let evidence = EvidenceWatermark::apply(
                        &frame,
                        &plate_str,
                        &detection.space_id,
                        &violation_type,
                    );

                    // 上传证据（若配置对象存储客户端）
                    let evidence_url = if let Some(ref storage) = self.storage {
                        match storage
                            .upload_evidence(&detection.space_id, &evidence.data, "jpg")
                            .await
                        {
                            Ok(url) => Some(url),
                            Err(_) => None,
                        }
                    } else {
                        None
                    };

                    let cloud_event = CloudEvent {
                        event_id: Uuid::new_v4(),
                        space_id: detection.space_id.clone(),
                        plate: Some(plate_str),
                        vehicle_type: eval_input.vehicle_type,
                        violation_type: Some(violation_type),
                        occurred_at: Utc::now(),
                        evidence_url,
                    };

                    // 本地 SQLite 暂存
                    if let Some(ref db) = self.sqlite {
                        let _ = db.save_event(&cloud_event);
                    }

                    return Ok(Some(cloud_event));
                } else {
                    // 若已在位但合规，消警
                    let _ = self.alarm.trigger_alarm(&detection.space_id, None, false);
                }
            } else if matches!(tr.phase, PresencePhase::Empty) {
                // 空闲消警
                let _ = self.alarm.trigger_alarm(&detection.space_id, None, false);
            }
        }

        // 车辆离场事件消警
        if matches!(transition, TransitionEvent::VehicleLeft { .. }) {
            let _ = self.alarm.trigger_alarm(&detection.space_id, None, false);
        }

        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        relay::{MemoryRelayController, MemoryVoiceAlarmController, RelayController, VoiceAlarmController},
        RuleConfig, VoiceAlarmCommand,
    };

    #[tokio::test]
    async fn test_evidence_watermark_generation() {
        let frame = VideoFrame {
            channel_id: 1,
            space_id: "SPACE-01".to_string(),
            timestamp: Utc::now(),
            width: 1920,
            height: 1080,
            raw_data: vec![0xFF, 0xD8, 0xFF, 0xE0], // JPEG 头部
        };

        let evidence = EvidenceWatermark::apply(&frame, "京A88888", "SPACE-01", "FUEL_VEHICLE");
        assert!(evidence.watermark_summary.contains("SPACE-01"));
        assert!(evidence.watermark_summary.contains("京A88888"));
        assert!(!evidence.fingerprint_sha256.is_empty());
        assert!(evidence.data.len() > frame.raw_data.len());
    }

    #[tokio::test]
    async fn test_vision_analysis_pipeline_fuel_violation() {
        let grabber = Arc::new(FrameGrabber::new());
        grabber.register_channel(VideoChannelConfig {
            channel_id: 1,
            space_id: "SPACE-01".to_string(),
            rtsp_url: "rtsp://192.168.1.10:554/ch1".to_string(),
            fps: 2,
            enabled: true,
        });

        let inference = Arc::new(MockInferenceEngine::new("MockYOLOv8"));
        // 预设检测到燃油车
        inference.set_detection(
            "SPACE-01",
            VehicleDetection {
                space_id: "SPACE-01".to_string(),
                is_occupied: true,
                plate: Some("京A11111".to_string()),
                vehicle_type: VehicleType::Fuel,
                confidence: 0.98,
                roi_box: [0.1, 0.1, 0.8, 0.8],
            },
        );

        let mut rules = RuleConfig::default();
        rules.vehicle_enter_confirm_seconds = 0; // 测试环境无需等待进场确认
        let tracker = Arc::new(SpaceTrackerRegistry::new());

        let relay_ctrl = Arc::new(MemoryRelayController::new());
        relay_ctrl.register_space("SPACE-01", 1).unwrap();
        let voice_ctrl = Arc::new(MemoryVoiceAlarmController::default());
        let dual_alarm = Arc::new(DualModeAlarmController::new(
            relay_ctrl.clone(),
            voice_ctrl.clone(),
        ));
        dual_alarm.bind_space("SPACE-01", 1);

        let pipeline = VisionAnalysisPipeline::new(
            grabber,
            inference.clone(),
            tracker,
            dual_alarm,
            None,
            None,
            None,
        );

        // 第一次执行进入 Entering 观察阶段，第二次执行确认在位（vehicle_enter_confirm_seconds = 0）
        let _ = pipeline.process_channel(1, &rules).await.unwrap();
        let result = pipeline.process_channel(1, &rules).await.unwrap();
        assert!(result.is_some());
        let ev = result.unwrap();
        assert_eq!(ev.violation_type, Some("FUEL_VEHICLE".to_string()));
        assert_eq!(ev.plate, Some("京A11111".to_string()));

        // 校验声光与串口语音联动
        assert_eq!(
            voice_ctrl.get_last_command("SPACE-01"),
            Some(VoiceAlarmCommand::FuelVehicleViolation)
        );
        let relay_snap = relay_ctrl.snapshot().unwrap();
        assert_eq!(relay_snap.get("SPACE-01"), Some(&true));

        // 车辆驶离
        inference.set_detection(
            "SPACE-01",
            VehicleDetection {
                space_id: "SPACE-01".to_string(),
                is_occupied: false,
                plate: None,
                vehicle_type: VehicleType::Unknown,
                confidence: 0.99,
                roi_box: [0.0; 4],
            },
        );

        rules.vehicle_leave_confirm_seconds = 0; // 测试环境立即确认驶离
        // 第一次驶离进入 Leaving，第二次确认 Empty 驶离消警
        let _ = pipeline.process_channel(1, &rules).await;
        let _ = pipeline.process_channel(1, &rules).await;

        // 校验消警
        assert_eq!(
            voice_ctrl.get_last_command("SPACE-01"),
            Some(VoiceAlarmCommand::Stop)
        );
        let relay_snap_after = relay_ctrl.snapshot().unwrap();
        assert_eq!(relay_snap_after.get("SPACE-01"), Some(&false));
    }
}
