//! H.265 / HEVC 专用解码器适配器
//!
//! 在 RK3588 边缘端，通过硬件 VPU MPP 进行 4K/8K 60fps 极速硬解；
//! 在非 RK3588 环境下，优雅返回错误并由外层流水线执行裸帧透传降级。

use super::{DecoderStatistics, MppHardwareDecoder, VideoDecoder};
use crate::gb28181::rtp_ps::DecodedVideoFrame;

pub struct H265Decoder {
    mpp: Option<MppHardwareDecoder>,
    stats: DecoderStatistics,
}

impl Default for H265Decoder {
    fn default() -> Self {
        Self::new()
    }
}

impl H265Decoder {
    pub fn new() -> Self {
        let mpp = MppHardwareDecoder::new(true).ok();
        Self {
            mpp,
            stats: DecoderStatistics::default(),
        }
    }
}

impl VideoDecoder for H265Decoder {
    fn decode_frame(&mut self, data: &[u8]) -> Result<DecodedVideoFrame, String> {
        if let Some(ref mut mpp) = self.mpp {
            let res = mpp.decode_frame(data);
            if res.is_ok() {
                self.stats.frames_decoded += 1;
            } else {
                self.stats.decode_errors += 1;
            }
            res
        } else {
            self.stats.decode_errors += 1;
            Err("H.265 hardware decoding requires RK3588 MPP (librockchip_mpp.so)".to_string())
        }
    }

    fn statistics(&self) -> DecoderStatistics {
        if let Some(ref mpp) = self.mpp {
            mpp.statistics()
        } else {
            self.stats.clone()
        }
    }

    fn codec_name(&self) -> &str {
        if self.mpp.is_some() {
            "RK3588-MPP-HEVC"
        } else {
            "H265-HardwareUnavailable"
        }
    }
}
