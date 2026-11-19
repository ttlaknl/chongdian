//! 基于瑞芯微 RK3588 MPP (Media Process Platform) 的硬件 VPU 解码器

use super::mpp_ffi::*;
use super::{nv12_to_rgb24, DecoderStatistics, VideoDecoder};
use crate::gb28181::rtp_ps::DecodedVideoFrame;
use libloading::{Library, Symbol};
use std::ffi::c_void;
use std::sync::Arc;

pub struct MppHardwareDecoder {
    _lib: Arc<Library>,
    ctx: MppCtx,
    mpi: *mut MppApi,
    is_hevc: bool,
    stats: DecoderStatistics,
    last_width: u32,
    last_height: u32,
}

// MppHardwareDecoder 拥有内部上下文指针，在安全线程间迁移
unsafe impl Send for MppHardwareDecoder {}

impl MppHardwareDecoder {
    pub fn new(is_hevc: bool) -> Result<Self, String> {
        let lib = unsafe {
            Library::new("librockchip_mpp.so")
                .map_err(|e| format!("Failed to load librockchip_mpp.so: {}", e))?
        };
        let lib = Arc::new(lib);

        unsafe {
            type MppCreateFn = unsafe extern "C" fn(*mut MppCtx, *mut *mut MppApi) -> MppRet;
            type MppInitFn = unsafe extern "C" fn(MppCtx, u32, u32) -> MppRet;

            let mpp_create: Symbol<MppCreateFn> = lib
                .get(b"mpp_create\0")
                .map_err(|e| format!("mpp_create not found: {}", e))?;
            let mpp_init: Symbol<MppInitFn> = lib
                .get(b"mpp_init\0")
                .map_err(|e| format!("mpp_init not found: {}", e))?;

            let mut ctx: MppCtx = std::ptr::null_mut();
            let mut mpi: *mut MppApi = std::ptr::null_mut();

            if mpp_create(&mut ctx, &mut mpi) != MPP_OK || ctx.is_null() || mpi.is_null() {
                return Err("mpp_create invocation returned non-zero".to_string());
            }

            // 开启连续码流切片支持
            let mut need_split: u32 = 1;
            let _ = ((*mpi).control)(
                ctx,
                MPP_DEC_SET_PARSER_SPLIT_MODE,
                &mut need_split as *mut u32 as *mut c_void,
            );

            let coding_type = if is_hevc {
                MPP_VIDEO_CODING_HEVC
            } else {
                MPP_VIDEO_CODING_AVC
            };

            if mpp_init(ctx, MPP_CTX_DEC, coding_type) != MPP_OK {
                type MppDestroyFn = unsafe extern "C" fn(MppCtx) -> MppRet;
                if let Ok(mpp_destroy) = lib.get::<MppDestroyFn>(b"mpp_destroy\0") {
                    mpp_destroy(ctx);
                }
                return Err("mpp_init failed for VPU decoder".to_string());
            }

            Ok(Self {
                _lib: lib,
                ctx,
                mpi,
                is_hevc,
                stats: DecoderStatistics::default(),
                last_width: 1920,
                last_height: 1080,
            })
        }
    }
}

impl VideoDecoder for MppHardwareDecoder {
    fn decode_frame(&mut self, data: &[u8]) -> Result<DecodedVideoFrame, String> {
        if data.is_empty() {
            self.stats.frames_dropped += 1;
            return Err("Empty data packet".to_string());
        }

        unsafe {
            type MppPacketInitFn =
                unsafe extern "C" fn(*mut MppPacket, *const c_void, usize) -> MppRet;
            type MppPacketDeinitFn = unsafe extern "C" fn(*mut MppPacket) -> MppRet;
            type MppFrameDeinitFn = unsafe extern "C" fn(*mut MppFrame) -> MppRet;
            type MppFrameGetInfoChangeFn = unsafe extern "C" fn(MppFrame) -> u32;
            type MppFrameGetWidthFn = unsafe extern "C" fn(MppFrame) -> u32;
            type MppFrameGetHeightFn = unsafe extern "C" fn(MppFrame) -> u32;
            type MppFrameGetHorStrideFn = unsafe extern "C" fn(MppFrame) -> u32;
            type MppFrameGetVerStrideFn = unsafe extern "C" fn(MppFrame) -> u32;
            type MppFrameGetBufferFn = unsafe extern "C" fn(MppFrame) -> MppBuffer;
            type MppBufferGetPtrFn = unsafe extern "C" fn(MppBuffer) -> *mut c_void;

            let mpp_packet_init: Symbol<MppPacketInitFn> = self
                ._lib
                .get(b"mpp_packet_init\0")
                .map_err(|e| e.to_string())?;
            let mpp_packet_deinit: Symbol<MppPacketDeinitFn> = self
                ._lib
                .get(b"mpp_packet_deinit\0")
                .map_err(|e| e.to_string())?;
            let mpp_frame_deinit: Symbol<MppFrameDeinitFn> = self
                ._lib
                .get(b"mpp_frame_deinit\0")
                .map_err(|e| e.to_string())?;
            let mpp_frame_get_info_change: Symbol<MppFrameGetInfoChangeFn> = self
                ._lib
                .get(b"mpp_frame_get_info_change\0")
                .map_err(|e| e.to_string())?;
            let mpp_frame_get_width: Symbol<MppFrameGetWidthFn> = self
                ._lib
                .get(b"mpp_frame_get_width\0")
                .map_err(|e| e.to_string())?;
            let mpp_frame_get_height: Symbol<MppFrameGetHeightFn> = self
                ._lib
                .get(b"mpp_frame_get_height\0")
                .map_err(|e| e.to_string())?;
            let mpp_frame_get_hor_stride: Symbol<MppFrameGetHorStrideFn> = self
                ._lib
                .get(b"mpp_frame_get_hor_stride\0")
                .map_err(|e| e.to_string())?;
            let mpp_frame_get_ver_stride: Symbol<MppFrameGetVerStrideFn> = self
                ._lib
                .get(b"mpp_frame_get_ver_stride\0")
                .map_err(|e| e.to_string())?;
            let mpp_frame_get_buffer: Symbol<MppFrameGetBufferFn> = self
                ._lib
                .get(b"mpp_frame_get_buffer\0")
                .map_err(|e| e.to_string())?;
            let mpp_buffer_get_ptr: Symbol<MppBufferGetPtrFn> = self
                ._lib
                .get(b"mpp_buffer_get_ptr\0")
                .map_err(|e| e.to_string())?;

            let mut packet: MppPacket = std::ptr::null_mut();
            if mpp_packet_init(&mut packet, data.as_ptr() as *const c_void, data.len()) != MPP_OK {
                self.stats.decode_errors += 1;
                return Err("mpp_packet_init failed".to_string());
            }

            let put_ret = ((*self.mpi).decode_put_packet)(self.ctx, packet);
            mpp_packet_deinit(&mut packet);

            if put_ret != MPP_OK {
                self.stats.decode_errors += 1;
                return Err(format!("decode_put_packet error: {}", put_ret));
            }

            let mut frame: MppFrame = std::ptr::null_mut();
            let get_ret = ((*self.mpi).decode_get_frame)(self.ctx, &mut frame);

            if get_ret != MPP_OK || frame.is_null() {
                self.stats.frames_dropped += 1;
                return Err("No decoded frame ready from MPP VPU yet".to_string());
            }

            // 处理分辨率自适应
            if mpp_frame_get_info_change(frame) != 0 {
                let _ = ((*self.mpi).control)(
                    self.ctx,
                    MPP_DEC_SET_INFO_CHANGE_READY,
                    std::ptr::null_mut(),
                );
                mpp_frame_deinit(&mut frame);
                self.stats.frames_dropped += 1;
                return Err("MPP resolution/info change received, re-synchronizing".to_string());
            }

            let width = mpp_frame_get_width(frame);
            let height = mpp_frame_get_height(frame);
            let hor_stride = mpp_frame_get_hor_stride(frame);
            let ver_stride = mpp_frame_get_ver_stride(frame);
            let buffer = mpp_frame_get_buffer(frame);

            if buffer.is_null() {
                mpp_frame_deinit(&mut frame);
                self.stats.decode_errors += 1;
                return Err("MPP frame buffer pointer is null".to_string());
            }

            let buf_ptr = mpp_buffer_get_ptr(buffer) as *const u8;
            if buf_ptr.is_null() {
                mpp_frame_deinit(&mut frame);
                self.stats.decode_errors += 1;
                return Err("MPP mpp_buffer_get_ptr is null".to_string());
            }

            // NV12 格式：Y 平面 + UV 平面
            let y_size = (hor_stride * ver_stride) as usize;
            let uv_size = (hor_stride * (ver_stride / 2)) as usize;

            let y_plane = std::slice::from_raw_parts(buf_ptr, y_size);
            let uv_plane = std::slice::from_raw_parts(buf_ptr.add(y_size), uv_size);

            let rgb24 = nv12_to_rgb24(y_plane, uv_plane, width, height, hor_stride, hor_stride);

            mpp_frame_deinit(&mut frame);

            self.stats.frames_decoded += 1;
            self.last_width = width;
            self.last_height = height;
            let now_ms = chrono::Utc::now().timestamp_millis() as u64;

            Ok(DecodedVideoFrame {
                width,
                height,
                is_keyframe: true,
                timestamp_ms: now_ms,
                frame_data: rgb24,
            })
        }
    }

    fn statistics(&self) -> DecoderStatistics {
        self.stats.clone()
    }

    fn codec_name(&self) -> &str {
        if self.is_hevc {
            "RK3588-MPP-H265"
        } else {
            "RK3588-MPP-H264"
        }
    }
}

impl Drop for MppHardwareDecoder {
    fn drop(&mut self) {
        unsafe {
            type MppDestroyFn = unsafe extern "C" fn(MppCtx) -> MppRet;
            if let Ok(mpp_destroy) = self._lib.get::<MppDestroyFn>(b"mpp_destroy\0") {
                mpp_destroy(self.ctx);
            }
        }
    }
}
