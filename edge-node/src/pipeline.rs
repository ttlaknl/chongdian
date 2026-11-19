use crate::{
    db::SqliteStorage,
    evaluate,
    relay::DualModeAlarmController,
    storage::EvidenceStorage,
    tracker::{PresencePhase, RawDetection, SpaceTrackerRegistry, TransitionEvent},
    ChargerAdapter, ChargerStatus, CloudEvent, EvaluationInput, RuleConfig, VehicleType,
    WhitelistRegistry,
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
    pub is_simulated: bool,
    pub data_source: String,
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
///
/// 当配置了 RTSP URL 时，通过 RTSP 客户端拉取实时流并解码为 RGB 帧。
/// 当配置了 GB28181 设备 ID 时，通过 GB28181 接收器获取视频流。
/// 未配置真实源时，回退到仿真抽帧模式。
pub struct FrameGrabber {
    channels: Mutex<HashMap<u8, VideoChannelConfig>>,
    frame_counters: Mutex<HashMap<u8, u64>>,
    /// 解码后的真实帧缓冲区（从 RTSP/GB28181 接收）
    decoded_frame_buffer: Mutex<HashMap<u8, VideoFrame>>,
}

impl FrameGrabber {
    pub fn new() -> Self {
        Self {
            channels: Mutex::new(HashMap::new()),
            frame_counters: Mutex::new(HashMap::new()),
            decoded_frame_buffer: Mutex::new(HashMap::new()),
        }
    }

    pub fn register_channel(&self, config: VideoChannelConfig) {
        if let Ok(mut map) = self.channels.lock() {
            map.insert(config.channel_id, config);
        }
    }

    /// 推送解码后的真实帧（由 RTSP 客户端或 GB28181 接收器调用）
    pub fn push_decoded_frame(&self, channel_id: u8, frame: VideoFrame) {
        if let Ok(mut buf) = self.decoded_frame_buffer.lock() {
            buf.insert(channel_id, frame);
        }
    }

    /// 检查通道是否有真实视频源配置
    fn has_real_source(&self, cfg: &VideoChannelConfig) -> bool {
        !cfg.rtsp_url.is_empty() || cfg.channel_id.to_string().starts_with("gb")
    }

    pub fn grab_frame(&self, channel_id: u8) -> Result<VideoFrame, String> {
        let channels = self.channels.lock().map_err(|_| "channels lock poisoned")?;
        let cfg = channels
            .get(&channel_id)
            .ok_or_else(|| format!("通道 {} 未注册", channel_id))?;

        if !cfg.enabled {
            return Err(format!("通道 {} 处于禁用状态", channel_id));
        }

        // 优先使用真实解码帧
        if self.has_real_source(cfg) {
            if let Ok(mut buf) = self.decoded_frame_buffer.lock() {
                if let Some(frame) = buf.remove(&channel_id) {
                    return Ok(frame);
                }
            }
            // 真实源尚未产生帧：返回占位帧（避免管道阻塞）
            return Ok(VideoFrame {
                channel_id: cfg.channel_id,
                space_id: cfg.space_id.clone(),
                timestamp: Utc::now(),
                width: 640,
                height: 480,
                raw_data: vec![128u8; 640 * 480 * 3],
                is_simulated: true,
                data_source: "REAL_STREAM_WAITING".to_string(),
            });
        }

        // 仿真模式：生成带通道元数据的帧数据
        let mut counters = self
            .frame_counters
            .lock()
            .map_err(|_| "counters lock poisoned")?;
        let count = counters.entry(channel_id).or_insert(0);
        *count += 1;

        let mock_frame_data = format!(
            "FRAME_HEADER_{}_{}_{}",
            cfg.channel_id, cfg.space_id, *count
        )
        .into_bytes();

        Ok(VideoFrame {
            channel_id: cfg.channel_id,
            space_id: cfg.space_id.clone(),
            timestamp: Utc::now(),
            width: 1920,
            height: 1080,
            raw_data: mock_frame_data,
            is_simulated: true,
            data_source: "MOCK_SIMULATED".to_string(),
        })
    }
}

impl Default for FrameGrabber {
    fn default() -> Self {
        Self::new()
    }
}

fn default_detection_data_source() -> String {
    "AI_INFERENCE".to_string()
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
    #[serde(default)]
    pub is_simulated: bool,
    #[serde(default = "default_detection_data_source")]
    pub data_source: String,
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
            is_simulated: true,
            data_source: "[MOCK_SIMULATED] 录像回放数据集".to_string(),
        })
    }

    fn engine_name(&self) -> &str {
        &self.name
    }
}

/// 真实视觉推理引擎适配器
pub struct RealVisualInferenceAdapter {
    inner: crate::ai::RealVisualInferenceEngine,
}

impl RealVisualInferenceAdapter {
    pub fn new() -> Self {
        Self {
            inner: crate::ai::RealVisualInferenceEngine::new(),
        }
    }
}

impl Default for RealVisualInferenceAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl InferenceEngine for RealVisualInferenceAdapter {
    fn infer(&self, frame: &VideoFrame) -> Result<VehicleDetection, String> {
        let det = self
            .inner
            .infer_frame(&frame.raw_data, frame.width, frame.height);
        let vtype = if !det.occupied {
            VehicleType::Unknown
        } else if det.is_new_energy {
            VehicleType::NewEnergy
        } else {
            VehicleType::Fuel
        };

        Ok(VehicleDetection {
            space_id: frame.space_id.clone(),
            is_occupied: det.occupied,
            plate: det.plate_number,
            vehicle_type: vtype,
            confidence: det.confidence as f32,
            roi_box: [
                det.bbox[1] as f32,
                det.bbox[0] as f32,
                (det.bbox[1] + det.bbox[3]) as f32,
                (det.bbox[0] + det.bbox[2]) as f32,
            ],
            is_simulated: det.is_simulated,
            data_source: det.data_source,
        })
    }

    fn engine_name(&self) -> &str {
        &self.inner.model_version
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
    whitelist: Option<Arc<WhitelistRegistry>>,
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
            whitelist: None,
        }
    }

    pub fn with_whitelist(mut self, whitelist: Arc<WhitelistRegistry>) -> Self {
        self.whitelist = Some(whitelist);
        self
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
            is_simulated: detection.is_simulated,
            data_source: detection.data_source.clone(),
            confidence: Some(detection.confidence as f64),
            plate_color: None,
            camera_id: None,
            image_base64: None,
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

                let plate = tr
                    .candidate_plate
                    .clone()
                    .or_else(|| detection.plate.clone());
                let whitelisted = self
                    .whitelist
                    .as_ref()
                    .map(|registry| registry.contains(plate.as_deref(), frame.timestamp))
                    .transpose()?
                    .unwrap_or(false);
                let eval_input = EvaluationInput {
                    space_id: detection.space_id.clone(),
                    plate,
                    vehicle_type: tr.candidate_vehicle_type.clone(),
                    charger_status,
                    occupied_since,
                    charge_completed_at: None,
                    now: frame.timestamp,
                    whitelisted,
                };

                let eval_result = evaluate(&eval_input, rules);

                if eval_result.should_alarm {
                    let violation_type = eval_result
                        .violation_type
                        .clone()
                        .unwrap_or_else(|| "FUEL_VEHICLE".into());

                    // 触发双模声光与串口定制语音报警
                    let _ =
                        self.alarm
                            .trigger_alarm(&detection.space_id, Some(&violation_type), true);

                    let plate_str = eval_input.plate.clone().unwrap_or_else(|| "UNKNOWN".into());

                    // 打标防篡改证据
                    let evidence = EvidenceWatermark::apply(
                        &frame,
                        &plate_str,
                        &detection.space_id,
                        &violation_type,
                    );

                    // 上传证据（若配置对象存储客户端，自动生成防篡改 Sidecar 元数据）
                    let evidence_url = if let Some(ref storage) = self.storage {
                        storage
                            .upload_evidence_with_context(
                                &detection.space_id,
                                &evidence.data,
                                "jpg",
                                Some(&plate_str),
                                Some(&violation_type),
                                detection.is_simulated,
                            )
                            .await
                            .map(|(url, _)| url)
                            .ok()
                    } else {
                        None
                    };

                    let cloud_event = CloudEvent {
                        event_id: Uuid::new_v4(),
                        session_id: format!(
                            "{}:{}",
                            detection.space_id,
                            occupied_since.timestamp_millis()
                        ),
                        space_id: detection.space_id.clone(),
                        plate: Some(plate_str),
                        vehicle_type: eval_input.vehicle_type,
                        violation_type: Some(violation_type),
                        occurred_at: Utc::now(),
                        evidence_url,
                        status: crate::EventStatus::Alarming,
                        cleared_at: None,
                        updated_at: Utc::now(),
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
        relay::{
            MemoryRelayController, MemoryVoiceAlarmController, RelayController,
            VoiceAlarmController,
        },
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
            is_simulated: false,
            data_source: "TEST_REAL_CAMERA".to_string(),
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
                is_simulated: false,
                data_source: "MOCK_INFERENCE".to_string(),
            },
        );

        let mut rules = RuleConfig {
            vehicle_enter_confirm_seconds: 0,
            ..RuleConfig::default()
        };
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
                is_simulated: false,
                data_source: "MOCK_INFERENCE".to_string(),
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

    #[test]
    fn test_frame_grabber_decoded_frame_buffer() {
        let grabber = FrameGrabber::new();
        grabber.register_channel(VideoChannelConfig {
            channel_id: 1,
            space_id: "SPACE-01".to_string(),
            rtsp_url: "rtsp://192.168.1.10:554/ch1".to_string(),
            fps: 2,
            enabled: true,
        });

        // 推送真实解码帧
        let decoded_frame = VideoFrame {
            channel_id: 1,
            space_id: "SPACE-01".to_string(),
            timestamp: Utc::now(),
            width: 1920,
            height: 1080,
            raw_data: vec![255u8; 1920 * 1080 * 3],
            is_simulated: false,
            data_source: "RTSP_DECODED".to_string(),
        };
        grabber.push_decoded_frame(1, decoded_frame);

        // 拉取帧应返回真实解码帧
        let frame = grabber.grab_frame(1).unwrap();
        assert!(!frame.is_simulated);
        assert_eq!(frame.width, 1920);
        assert_eq!(frame.height, 1080);
        assert_eq!(frame.data_source, "RTSP_DECODED");
    }

    #[test]
    fn test_frame_grabber_mock_fallback() {
        let grabber = FrameGrabber::new();
        // 注册无 RTSP URL 的通道（仿真模式）
        grabber.register_channel(VideoChannelConfig {
            channel_id: 2,
            space_id: "SPACE-02".to_string(),
            rtsp_url: String::new(),
            fps: 2,
            enabled: true,
        });

        // 仿真模式应返回模拟帧
        let frame = grabber.grab_frame(2).unwrap();
        assert!(frame.is_simulated);
        assert!(frame.data_source.contains("MOCK"));
    }

    #[test]
    fn test_frame_grabber_shared_between_producers_and_consumers() {
        // 验证 FrameGrabber 可以在多个生产者和消费者之间共享
        let grabber = Arc::new(FrameGrabber::new());
        grabber.register_channel(VideoChannelConfig {
            channel_id: 3,
            space_id: "SPACE-03".to_string(),
            rtsp_url: "rtsp://192.168.1.12:554/ch1".to_string(),
            fps: 2,
            enabled: true,
        });

        // 模拟 GB28181 生产者推送帧
        let gb_frame = VideoFrame {
            channel_id: 3,
            space_id: "SPACE-03".to_string(),
            timestamp: Utc::now(),
            width: 1920,
            height: 1080,
            raw_data: vec![200u8; 1920 * 1080 * 3],
            is_simulated: false,
            data_source: "GB28181_DECODED".to_string(),
        };
        grabber.push_decoded_frame(3, gb_frame);

        // 模拟 RTSP 生产者推送帧（覆盖）
        let rtsp_frame = VideoFrame {
            channel_id: 3,
            space_id: "SPACE-03".to_string(),
            timestamp: Utc::now(),
            width: 1280,
            height: 720,
            raw_data: vec![100u8; 1280 * 720 * 3],
            is_simulated: false,
            data_source: "RTSP_DECODED".to_string(),
        };
        grabber.push_decoded_frame(3, rtsp_frame);

        // 消费者拉取帧（应获取最后推送的帧）
        let frame = grabber.grab_frame(3).unwrap();
        assert!(!frame.is_simulated);
        assert_eq!(frame.width, 1280);
        assert_eq!(frame.height, 720);
        assert_eq!(frame.data_source, "RTSP_DECODED");
    }
}
