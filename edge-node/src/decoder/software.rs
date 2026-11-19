//! 基于 Cisco OpenH264 的跨平台 CPU 软解码器

use super::{DecoderStatistics, VideoDecoder};
use crate::gb28181::rtp_ps::DecodedVideoFrame;
use openh264::formats::YUVSource;

pub struct OpenH264Decoder {
    inner: Option<openh264::decoder::Decoder>,
    stats: DecoderStatistics,
    last_width: u32,
    last_height: u32,
}

impl Default for OpenH264Decoder {
    fn default() -> Self {
        Self::new()
    }
}

impl OpenH264Decoder {
    pub fn new() -> Self {
        let inner = openh264::decoder::Decoder::new().ok();
        Self {
            inner,
            stats: DecoderStatistics::default(),
            last_width: 1920,
            last_height: 1080,
        }
    }

    pub fn statistics(&self) -> DecoderStatistics {
        self.stats.clone()
    }

    pub fn decode_frame(&mut self, data: &[u8]) -> Result<DecodedVideoFrame, String> {
        let decoder = match self.inner.as_mut() {
            Some(d) => d,
            None => {
                self.stats.decode_errors += 1;
                return Err("OpenH264 decoder failed to initialize".to_string());
            }
        };

        if data.is_empty() {
            self.stats.frames_dropped += 1;
            return Err("Empty frame data passed to OpenH264Decoder".to_string());
        }

        match decoder.decode(data) {
            Ok(Some(yuv)) => {
                let (width, height) = yuv.dimensions();
                let w = width as u32;
                let h = height as u32;
                self.last_width = w;
                self.last_height = h;

                let mut rgb_buf = vec![0u8; width * height * 3];
                yuv.write_rgb8(&mut rgb_buf);

                self.stats.frames_decoded += 1;
                let now_ms = chrono::Utc::now().timestamp_millis() as u64;

                Ok(DecodedVideoFrame {
                    width: w,
                    height: h,
                    is_keyframe: true,
                    timestamp_ms: now_ms,
                    frame_data: rgb_buf,
                })
            }
            Ok(None) => {
                // SPS/PPS 或分片前导，编解码器正在缓冲
                self.stats.frames_dropped += 1;
                Err("Frame buffered by OpenH264 (waiting for complete GOP)".to_string())
            }
            Err(e) => {
                self.stats.decode_errors += 1;
                Err(format!("OpenH264 decode error: {:?}", e))
            }
        }
    }
}

impl VideoDecoder for OpenH264Decoder {
    fn decode_frame(&mut self, data: &[u8]) -> Result<DecodedVideoFrame, String> {
        self.decode_frame(data)
    }

    fn statistics(&self) -> DecoderStatistics {
        self.statistics()
    }

    fn codec_name(&self) -> &str {
        "OpenH264-CPU"
    }
}
