//! 视频解码器抽象与工厂模式 (H.264 / H.265)
//!
//! 支持两种解码后端：
//! 1. `OpenH264Decoder`: 基于 Cisco OpenH264 的跨平台 CPU 纯软解 (H.264)
//! 2. `MppHardwareDecoder`: 基于瑞芯微 RK3588 MPP (Media Process Platform) 的硬件 VPU 解码器 (H.264 / H.265)
//!
//! 在非 RK3588 环境下，自动平滑降级至 OpenH264 软解；当解码出现异常时，
//! 采用工业级容错策略，允许透传原始帧供下游保底处理。

pub mod gpu_memory;
pub mod h265;
pub mod hardware_mpp;
pub mod mpp_ffi;
pub mod software;
#[cfg(test)]
mod tests;

pub use gpu_memory::nv12_to_rgb24;
pub use h265::H265Decoder;
pub use hardware_mpp::MppHardwareDecoder;
pub use software::OpenH264Decoder;

use crate::gb28181::rtp_ps::DecodedVideoFrame;

/// 解码器运行统计指标
#[derive(Debug, Clone, Default)]
pub struct DecoderStatistics {
    pub frames_decoded: u64,
    pub decode_errors: u64,
    pub frames_dropped: u64,
}

/// 统一视频解码器抽象接口
pub trait VideoDecoder: Send {
    /// 输入 Annex-B 裸流 (NALU)，解码为 RGB 像素帧
    fn decode_frame(&mut self, data: &[u8]) -> Result<DecodedVideoFrame, String>;

    /// 获取解码器运行统计数据
    fn statistics(&self) -> DecoderStatistics;

    /// 获取当前解码器名称
    fn codec_name(&self) -> &str;
}

/// 默认解码器工厂函数：优先探测 RK3588 硬件 VPU，不可用时平滑降级为 OpenH264
pub fn create_default_decoder() -> Box<dyn VideoDecoder> {
    // 检查 RK3588 MPP 设备节点 /dev/mpp_service 是否存在
    if std::path::Path::new("/dev/mpp_service").exists() {
        if let Ok(mpp) = MppHardwareDecoder::new(false) {
            return Box::new(mpp);
        }
    }

    Box::new(OpenH264Decoder::new())
}

/// H.265 专用解码器工厂函数
pub fn create_h265_decoder() -> Box<dyn VideoDecoder> {
    if std::path::Path::new("/dev/mpp_service").exists() {
        if let Ok(mpp) = MppHardwareDecoder::new(true) {
            return Box::new(mpp);
        }
    }

    Box::new(H265Decoder::new())
}
