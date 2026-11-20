"""Configuration loader for the RTSP vision sidecar."""

from __future__ import annotations

import json
import os
from dataclasses import dataclass, field
from typing import Any


def _env_float(name: str, default: float, min_val: float = 0.01) -> float:
    raw = os.getenv(name, "").strip()
    if not raw:
        return default
    try:
        return max(min_val, float(raw))
    except ValueError:
        return default


def _env_int(name: str, default: int, min_val: int = 0) -> int:
    raw = os.getenv(name, "").strip()
    if not raw:
        return default
    try:
        return max(min_val, int(raw))
    except ValueError:
        return default


def _env_bool(name: str, default: bool) -> bool:
    raw = os.getenv(name, "").strip().lower()
    if not raw:
        return default
    return raw in {"1", "true", "yes", "on"}


def _env_str(name: str, default: str) -> str:
    raw = os.getenv(name, "").strip()
    return raw if raw else default


@dataclass(frozen=True)
class VisionConfig:
    """Runtime settings sourced purely from environment variables."""

    edge_url: str = "http://127.0.0.1:8081"
    edge_local_api_key: str | None = None
    target_poll_seconds: float = 10.0
    infer_fps: float = 1.0
    snapshot_seconds: float = 5.0
    heartbeat_seconds: float = 15.0
    rtsp_transport: str = "tcp"
    health_port: int = 8090
    static_cameras: list[dict[str, Any]] = field(default_factory=list)

    # === 里程碑 1B: OpenVINO 运行时配置 ===
    ov_device: str = "AUTO:GPU,CPU"
    """OpenVINO target device (e.g. 'AUTO:GPU,CPU', 'CPU', 'GPU')."""
    ov_num_streams: str = "AUTO"
    """OpenVINO concurrent inference throughput streams ('AUTO' or integer string)."""
    inference_backend: str = "openvino"
    """Inference backend mode: 'openvino' (production) or 'mock' (lightweight CI)."""

    # === 里程碑 1B: 主/子码流协同调度 ===
    sub_stream_fps: float = 1.0
    """子码流巡检态抽帧频率 (FPS)."""
    main_burst_fps: float = 4.0
    """主码流突发识牌态抓帧频率 (FPS)."""
    main_burst_max_sec: float = 12.0
    """主码流单次突发最长持续时间 (秒)，超时强制回落巡检态."""
    main_stream_mode: str = "grab_idle"
    """主码流空闲维持模式: 'grab_idle' (长连接仅grab丢缓冲) 或 'on_demand' (触发时重开)."""

    # === 里程碑 1A: 车辆检测硬门控 ===
    vehicle_conf_threshold: float = 0.45
    """YOLO 车辆目标置信度最低门槛 (COCO car/bus/truck)."""
    vehicle_min_size_px: int = 60
    """车辆检测框宽/高最小像素值，低于此视为噪点。"""
    vehicle_min_box_px: int = 60
    """兼容旧名称别名。"""
    vehicle_bbox_pad_ratio: float = 0.06
    """车牌检测时对车辆框外扩比例 (默认 6%)，裁剪后在画面内。"""

    # === 里程碑 1A: 车牌检测 & OCR 置信度门槛 ===
    plate_det_conf_threshold: float = 0.55
    """YOLO-Pose 车牌检测置信度最低门槛。"""
    plate_ocr_conf_threshold: float = 0.75
    """CRNN OCR 有效字符平均概率最低门槛。"""
    plate_ocr_min_char_conf: float = 0.45
    """CRNN OCR 单字符最低允许概率门槛。"""
    plate_min_width_px: int = 40
    """车牌裁剪框最小像素宽度。"""
    plate_min_height_px: int = 14
    """车牌裁剪框最小像素高度。"""
    plate_single_ar_min: float = 1.8
    """单层车牌最小宽高比。"""
    plate_single_ar_max: float = 5.5
    """单层车牌最大宽高比。"""
    plate_double_ar_min: float = 1.1
    """双层车牌最小宽高比。"""
    plate_double_ar_max: float = 3.5
    """双层车牌最大宽高比。"""
    plate_ioa_threshold: float = 0.70
    """车牌框需落入车辆框的最小交集面积比 (IoA)。"""

    # === 里程碑 2A: 4 角点透视拉直与逆光自适应增强 ===
    plate_det_has_keypoints: str = "auto"
    """车牌检测角点模式: 'auto' (自动根据张量维度探测), 'true' (强制4角点), 'false' (回退4框水平模式)."""
    enable_adaptive_enhance: bool = True
    """是否开启室外逆光/暗光自适应 CLAHE 双路择优增强."""
    plate_low_luma_threshold: float = 75.0
    """车牌暗光/欠曝平均亮度阈值 (低于此值触发增强)."""
    plate_low_contrast_threshold: float = 28.0
    """车牌低对比度标准差阈值 (低于此值触发增强)."""
    plate_clahe_clip_limit: float = 2.5
    """自适应 CLAHE 对比度受限阈值 (默认 2.5)."""
    plate_enhance_gamma: float = 0.75
    """自适应 Gamma 校正系数 (暗图默认 0.75)."""

    # === 里程碑 2B: ByteTrack 轨迹跟踪与车位 FSM 状态机 ===
    track_high_thresh: float = 0.45
    """ByteTrack 第一阶段高置信度跟踪门槛."""
    track_low_thresh: float = 0.20
    """ByteTrack 第二阶段低置信度挽救跟踪门槛."""
    track_max_lost_sec: float = 3.0
    """ByteTrack 轨迹最长丢失保持时间 (秒)，超时则回收."""
    plate_lock_score_threshold: float = 1.50
    """TrackPlateVoter 累计质量分锁定阈值 (默认 1.50)."""
    enter_confirm_sec: float = 3.0
    """车位接地带初次入位至 ENTERING 状态确认时间 (秒)."""
    occupied_confirm_sec: float = 8.0
    """车位持续在位至正式进入 OCCUPIED 状态并上报占位确认时间 (秒)."""
    leave_confirm_sec: float = 5.0
    """车位连续无车至正式退出至 FREE 状态并上报离场确认时间 (秒)."""

    # === 里程碑 1A: 1 拖 2 接地带 ROI 判定参数 ===
    ground_band_ratio: float = 0.22
    """车辆框底部轮胎接地带高度比例 (默认 22%)。"""
    ground_min_ioa: float = 0.35
    """轮胎接地带与车位多边形的最小交集比阈值 (默认 0.35)。"""

    # === 里程碑 1A: OSD 遮罩与防抖 ===
    osd_mask_top_ratio: float = 0.08
    """画面顶部排除比例，遮蔽摄像头 OSD 时间戳/通道名。"""
    osd_mask_bottom_ratio: float = 0.0
    """画面底部排除比例。"""
    plate_vote_window: int = 3
    """多帧滑动投票窗口长度。"""
    plate_vote_min_hits: int = 2
    """窗口内同一车牌至少命中次数才确认。"""
    vote_empty_clear_frames: int = 4
    """连续空帧数后清除投票缓存。"""

    # Legacy aliases kept for backward compatibility
    min_confidence: float = 0.72
    vote_window: int = 5
    vote_min_hits: int = 2
    empty_clear_frames: int = 4

    # === 调试开关 ===
    debug_draw_boxes: bool = False
    """开启后在证据帧上绘制车位多边形(黄)、车辆框与接地带(蓝)、车牌框(绿)及置信度与车牌标注。"""

    @classmethod
    def from_env(cls) -> "VisionConfig":
        raw_cams = os.getenv("VISION_CAMERAS_JSON", "").strip()
        static_cameras: list[dict[str, Any]] = []
        if raw_cams:
            parsed = json.loads(raw_cams)
            if isinstance(parsed, list):
                static_cameras = [item for item in parsed if isinstance(item, dict)]

        local_key = os.getenv("EDGE_LOCAL_API_KEY", "").strip() or None
        transport = os.getenv("VISION_RTSP_TRANSPORT", "tcp").strip().lower()
        if transport not in {"tcp", "udp"}:
            transport = "tcp"

        veh_min_sz = _env_int("VEHICLE_MIN_SIZE_PX", _env_int("VEHICLE_MIN_BOX_PX", 60, 10), 10)

        return cls(
            edge_url=os.getenv("EDGE_URL", "http://127.0.0.1:8081").strip().rstrip("/"),
            edge_local_api_key=local_key,
            target_poll_seconds=_env_float("VISION_TARGET_POLL_SECONDS", 10.0, 1.0),
            infer_fps=_env_float("VISION_INFER_FPS", 1.0, 0.1),
            snapshot_seconds=_env_float("VISION_SNAPSHOT_SECONDS", 5.0, 1.0),
            heartbeat_seconds=_env_float("VISION_HEARTBEAT_SECONDS", 15.0, 3.0),
            rtsp_transport=transport,
            health_port=_env_int("VISION_HEALTH_PORT", 8090, 1),
            static_cameras=static_cameras,
            # 里程碑 1B: OpenVINO
            ov_device=_env_str("OV_DEVICE", "AUTO:GPU,CPU"),
            ov_num_streams=_env_str("OV_NUM_STREAMS", "AUTO"),
            inference_backend=_env_str("INFERENCE_BACKEND", "openvino"),
            # 里程碑 1B: 主/子码流协同调度
            sub_stream_fps=_env_float("SUB_STREAM_FPS", 1.0, 0.1),
            main_burst_fps=_env_float("MAIN_BURST_FPS", 4.0, 0.5),
            main_burst_max_sec=_env_float("MAIN_BURST_MAX_SEC", 12.0, 1.0),
            main_stream_mode=_env_str("MAIN_STREAM_MODE", "grab_idle"),
            # 车辆检测门控
            vehicle_conf_threshold=_env_float("VEHICLE_CONF_THRESHOLD", 0.45, 0.1),
            vehicle_min_size_px=veh_min_sz,
            vehicle_min_box_px=veh_min_sz,
            vehicle_bbox_pad_ratio=_env_float("VEHICLE_BBOX_PAD_RATIO", 0.06, 0.0),
            # 车牌检测 & OCR 双门槛
            plate_det_conf_threshold=_env_float("PLATE_DET_CONF_THRESHOLD", 0.55, 0.1),
            plate_ocr_conf_threshold=_env_float("PLATE_OCR_CONF_THRESHOLD", 0.75, 0.1),
            plate_ocr_min_char_conf=_env_float("PLATE_OCR_MIN_CHAR_CONF", 0.45, 0.05),
            plate_min_width_px=_env_int("PLATE_MIN_WIDTH_PX", 40, 5),
            plate_min_height_px=_env_int("PLATE_MIN_HEIGHT_PX", 14, 3),
            plate_single_ar_min=_env_float("PLATE_SINGLE_AR_MIN", 1.8, 0.5),
            plate_single_ar_max=_env_float("PLATE_SINGLE_AR_MAX", 5.5, 2.0),
            plate_double_ar_min=_env_float("PLATE_DOUBLE_AR_MIN", 1.1, 0.3),
            plate_double_ar_max=_env_float("PLATE_DOUBLE_AR_MAX", 3.5, 1.0),
            plate_ioa_threshold=_env_float("PLATE_IOA_THRESHOLD", 0.70, 0.1),
            # 里程碑 2A: 4 角点 & 自适应增强
            plate_det_has_keypoints=_env_str("PLATE_DET_HAS_KEYPOINTS", "auto").lower(),
            enable_adaptive_enhance=_env_bool("ENABLE_ADAPTIVE_ENHANCE", True),
            plate_low_luma_threshold=_env_float("PLATE_LOW_LUMA_THRESHOLD", 75.0, 10.0),
            plate_low_contrast_threshold=_env_float("PLATE_LOW_CONTRAST_THRESHOLD", 28.0, 5.0),
            plate_clahe_clip_limit=_env_float("PLATE_CLAHE_CLIP_LIMIT", 2.5, 0.5),
            plate_enhance_gamma=_env_float("PLATE_ENHANCE_GAMMA", 0.75, 0.1),
            # 里程碑 2B: ByteTrack & 车位 FSM
            track_high_thresh=_env_float("TRACK_HIGH_THRESH", 0.45, 0.1),
            track_low_thresh=_env_float("TRACK_LOW_THRESH", 0.20, 0.05),
            track_max_lost_sec=_env_float("TRACK_MAX_LOST_SEC", 3.0, 0.5),
            plate_lock_score_threshold=_env_float("PLATE_LOCK_SCORE_THRESHOLD", 1.50, 0.5),
            enter_confirm_sec=_env_float("ENTER_CONFIRM_SEC", 3.0, 0.5),
            occupied_confirm_sec=_env_float("OCCUPIED_CONFIRM_SEC", 8.0, 1.0),
            leave_confirm_sec=_env_float("LEAVE_CONFIRM_SEC", 5.0, 1.0),
            # 1 拖 2 接地带参数
            ground_band_ratio=_env_float("GROUND_BAND_RATIO", 0.22, 0.05),
            ground_min_ioa=_env_float("GROUND_MIN_IOA", 0.35, 0.05),
            # OSD 遮罩 & 多帧投票
            osd_mask_top_ratio=_env_float("OSD_MASK_TOP_RATIO", 0.08, 0.0),
            osd_mask_bottom_ratio=_env_float("OSD_MASK_BOTTOM_RATIO", 0.0, 0.0),
            plate_vote_window=_env_int("PLATE_VOTE_WINDOW", 3, 1),
            plate_vote_min_hits=_env_int("PLATE_VOTE_MIN_HITS", 2, 1),
            vote_empty_clear_frames=_env_int("VOTE_EMPTY_CLEAR_FRAMES", 4, 1),
            # Legacy
            min_confidence=_env_float("VISION_MIN_CONFIDENCE", 0.72, 0.1),
            vote_window=_env_int("VISION_VOTE_WINDOW", 5, 1),
            vote_min_hits=_env_int("VISION_VOTE_MIN_HITS", 2, 1),
            empty_clear_frames=_env_int("VISION_EMPTY_CLEAR_FRAMES", 4, 1),
            # 调试开关
            debug_draw_boxes=_env_bool("DEBUG_DRAW_BOXES", False),
        )
