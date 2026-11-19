//! 解码器单元测试

use super::*;

#[test]
fn test_openh264_decoder_new() {
    let decoder = OpenH264Decoder::new();
    let stats = decoder.statistics();
    assert_eq!(stats.frames_decoded, 0);
    assert_eq!(stats.decode_errors, 0);
    assert_eq!(stats.frames_dropped, 0);
    assert_eq!(decoder.codec_name(), "OpenH264-CPU");
}

#[test]
fn test_openh264_invalid_data() {
    let mut decoder = OpenH264Decoder::new();
    let dummy_data = [0x00, 0x00, 0x00, 0x01, 0x65, 0xFF, 0xFF];
    let res = decoder.decode_frame(&dummy_data);
    // 无效 H.264 码流返回 Err 且不 panic
    assert!(res.is_err());
    let stats = decoder.statistics();
    assert!(stats.decode_errors > 0 || stats.frames_dropped > 0);
}

#[test]
fn test_create_default_decoder() {
    let decoder = create_default_decoder();
    assert!(!decoder.codec_name().is_empty());
    let stats = decoder.statistics();
    assert_eq!(stats.frames_decoded, 0);
}

#[test]
fn test_create_h265_decoder() {
    let decoder = create_h265_decoder();
    assert!(!decoder.codec_name().is_empty());
    let stats = decoder.statistics();
    assert_eq!(stats.frames_decoded, 0);
}
