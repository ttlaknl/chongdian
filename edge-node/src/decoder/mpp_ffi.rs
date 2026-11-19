//! Rockchip MPP (Media Process Platform) C-API 动态加载与 FFI 定义
//!
//! 在 RK3588 上，通过 libloading 在运行时探测并加载 `librockchip_mpp.so`，
//! 避免非 RK3588/开发机环境下的静态链接编译依赖。

use std::ffi::c_void;

pub type MppCtx = *mut c_void;
pub type MppPacket = *mut c_void;
pub type MppFrame = *mut c_void;
pub type MppBuffer = *mut c_void;
pub type MppRet = i32;

pub const MPP_OK: MppRet = 0;
pub const MPP_ERR_BUFFER_FULL: MppRet = -2;

pub const MPP_CTX_DEC: u32 = 0;
pub const MPP_VIDEO_CODING_AVC: u32 = 7; // H.264
pub const MPP_VIDEO_CODING_HEVC: u32 = 8; // H.265 / HEVC

pub const MPP_DEC_SET_PARSER_SPLIT_MODE: u32 = 0x0001_0001;
pub const MPP_DEC_SET_INFO_CHANGE_READY: u32 = 0x0001_0005;

/// MPP 核心函数虚表结构体（对应 C 头文件 MppApi_t）
#[repr(C)]
pub struct MppApi {
    pub size: u32,
    pub version: u32,
    pub decode_put_packet: unsafe extern "C" fn(ctx: MppCtx, packet: MppPacket) -> MppRet,
    pub decode_get_frame: unsafe extern "C" fn(ctx: MppCtx, frame: *mut MppFrame) -> MppRet,
    pub encode_put_frame: unsafe extern "C" fn(ctx: MppCtx, frame: MppFrame) -> MppRet,
    pub encode_get_packet: unsafe extern "C" fn(ctx: MppCtx, packet: *mut MppPacket) -> MppRet,
    pub control: unsafe extern "C" fn(ctx: MppCtx, cmd: u32, param: *mut c_void) -> MppRet,
    pub reset: unsafe extern "C" fn(ctx: MppCtx) -> MppRet,
}
