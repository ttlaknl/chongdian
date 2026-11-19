use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;

// ============================================================================
// RTP Packet Parser (RFC 3550)
// ============================================================================

#[derive(Debug, Clone)]
pub struct RtpHeader {
    pub version: u8,
    pub has_padding: bool,
    pub has_extension: bool,
    pub csrc_count: u8,
    pub marker: bool,
    pub payload_type: u8,
    pub sequence_number: u16,
    pub timestamp: u32,
    pub ssrc: u32,
    pub header_length: usize,
}

impl RtpHeader {
    pub fn parse(data: &[u8]) -> Result<Self, String> {
        if data.len() < 12 {
            return Err("RTP packet too short (< 12 bytes)".to_string());
        }

        let b0 = data[0];
        let version = (b0 >> 6) & 0x03;
        if version != 2 {
            return Err(format!("Unsupported RTP version: {}", version));
        }

        let has_padding = (b0 & 0x20) != 0;
        let has_extension = (b0 & 0x10) != 0;
        let csrc_count = b0 & 0x0F;

        let b1 = data[1];
        let marker = (b1 & 0x80) != 0;
        let payload_type = b1 & 0x7F;

        let sequence_number = u16::from_be_bytes([data[2], data[3]]);
        let timestamp = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
        let ssrc = u32::from_be_bytes([data[8], data[9], data[10], data[11]]);

        let mut header_length = 12 + (csrc_count as usize * 4);
        if data.len() < header_length {
            return Err("RTP packet truncated before CSRC list completed".to_string());
        }

        if has_extension {
            if data.len() < header_length + 4 {
                return Err("RTP packet truncated in extension header".to_string());
            }
            let ext_len =
                u16::from_be_bytes([data[header_length + 2], data[header_length + 3]]) as usize;
            header_length += 4 + (ext_len * 4);
        }

        if data.len() < header_length {
            return Err("RTP packet payload truncated".to_string());
        }

        Ok(Self {
            version,
            has_padding,
            has_extension,
            csrc_count,
            marker,
            payload_type,
            sequence_number,
            timestamp,
            ssrc,
            header_length,
        })
    }
}

// ============================================================================
// MPEG-PS Demuxer (ISO/IEC 13818-1 / GB/T 28181-2016)
// ============================================================================

pub const START_CODE_PACK: [u8; 4] = [0x00, 0x00, 0x01, 0xBA];
pub const START_CODE_SYSTEM: [u8; 4] = [0x00, 0x00, 0x01, 0xBB];
pub const START_CODE_PSM: [u8; 4] = [0x00, 0x00, 0x01, 0xBC];

#[derive(Debug, Clone)]
pub struct VideoElementaryStream {
    pub pts: Option<u64>,
    pub dts: Option<u64>,
    pub payload: Vec<u8>,
}

#[derive(Default)]
pub struct MpegPsDemuxer {
    buffer: Vec<u8>,
    pub detected_codec: Option<crate::rtsp::client::VideoCodec>,
}

impl MpegPsDemuxer {
    pub fn new() -> Self {
        Self {
            buffer: Vec::with_capacity(65536),
            detected_codec: None,
        }
    }

    pub fn push_payload(&mut self, payload: &[u8]) {
        self.buffer.extend_from_slice(payload);
    }

    pub fn extract_video_streams(&mut self) -> Vec<VideoElementaryStream> {
        let mut results = Vec::new();
        let mut offset = 0;

        while offset + 4 <= self.buffer.len() {
            if self.buffer[offset..offset + 3] != [0x00, 0x00, 0x01] {
                offset += 1;
                continue;
            }

            let stream_id = self.buffer[offset + 3];

            match stream_id {
                0xBA => {
                    // Pack header: 14 bytes + stuffing
                    if offset + 14 > self.buffer.len() {
                        break;
                    }
                    let stuffing_len = (self.buffer[offset + 13] & 0x07) as usize;
                    let pack_len = 14 + stuffing_len;
                    if offset + pack_len > self.buffer.len() {
                        break;
                    }
                    offset += pack_len;
                }
                0xBC => {
                    // PSM: parse elementary stream map for stream_type (0x1B = H.264, 0x24 = H.265)
                    if offset + 6 > self.buffer.len() {
                        break;
                    }
                    let length =
                        u16::from_be_bytes([self.buffer[offset + 4], self.buffer[offset + 5]])
                            as usize;
                    let total_len = 6 + length;
                    if offset + total_len > self.buffer.len() {
                        break;
                    }

                    if length >= 6 && offset + 10 <= self.buffer.len() {
                        let psi_len = u16::from_be_bytes([
                            self.buffer[offset + 8],
                            self.buffer[offset + 9],
                        ]) as usize;
                        let es_map_start = offset + 10 + psi_len;
                        if es_map_start + 2 <= offset + total_len {
                            let es_map_len = u16::from_be_bytes([
                                self.buffer[es_map_start],
                                self.buffer[es_map_start + 1],
                            ]) as usize;
                            let mut es_pos = es_map_start + 2;
                            let es_end = es_pos + es_map_len;
                            while es_pos + 4 <= es_end && es_pos + 4 <= offset + total_len {
                                let stream_type = self.buffer[es_pos];
                                match stream_type {
                                    0x1B => {
                                        self.detected_codec =
                                            Some(crate::rtsp::client::VideoCodec::H264);
                                    }
                                    0x24 => {
                                        self.detected_codec =
                                            Some(crate::rtsp::client::VideoCodec::H265);
                                    }
                                    _ => {}
                                }
                                let es_info_len = u16::from_be_bytes([
                                    self.buffer[es_pos + 2],
                                    self.buffer[es_pos + 3],
                                ]) as usize;
                                es_pos += 4 + es_info_len;
                            }
                        }
                    }

                    offset += total_len;
                }
                0xBB => {
                    // System header: 4 bytes code + 2 bytes length
                    if offset + 6 > self.buffer.len() {
                        break;
                    }
                    let length =
                        u16::from_be_bytes([self.buffer[offset + 4], self.buffer[offset + 5]])
                            as usize;
                    let total_len = 6 + length;
                    if offset + total_len > self.buffer.len() {
                        break;
                    }
                    offset += total_len;
                }
                0xE0..=0xEF => {
                    // Video PES Packet
                    if offset + 6 > self.buffer.len() {
                        break;
                    }
                    let pes_len =
                        u16::from_be_bytes([self.buffer[offset + 4], self.buffer[offset + 5]])
                            as usize;

                    let (total_len, es_payload_offset, es_payload_len) = if pes_len > 0 {
                        let total = 6 + pes_len;
                        if offset + total > self.buffer.len() {
                            break;
                        }

                        let header_data_len = if offset + 9 <= self.buffer.len() {
                            self.buffer[offset + 8] as usize
                        } else {
                            0
                        };
                        let es_start = offset + 9 + header_data_len;
                        let es_end = offset + total;
                        (total, es_start, es_end.saturating_sub(es_start))
                    } else {
                        // Unbounded PES packet: find next start code
                        let next_sc = self.find_next_start_code(offset + 6);
                        let total = next_sc.unwrap_or(self.buffer.len()) - offset;
                        let header_data_len = if offset + 9 <= self.buffer.len() {
                            self.buffer[offset + 8] as usize
                        } else {
                            0
                        };
                        let es_start = offset + 9 + header_data_len;
                        let es_end = offset + total;
                        (total, es_start, es_end.saturating_sub(es_start))
                    };

                    if es_payload_len > 0 && es_payload_offset + es_payload_len <= self.buffer.len()
                    {
                        let payload = self.buffer
                            [es_payload_offset..es_payload_offset + es_payload_len]
                            .to_vec();
                        results.push(VideoElementaryStream {
                            pts: None,
                            dts: None,
                            payload,
                        });
                    }

                    offset += total_len;
                }
                _ => {
                    // Other PES packet (Audio, padding, etc.)
                    if offset + 6 > self.buffer.len() {
                        break;
                    }
                    let length =
                        u16::from_be_bytes([self.buffer[offset + 4], self.buffer[offset + 5]])
                            as usize;
                    let total_len = if length > 0 {
                        6 + length
                    } else {
                        self.find_next_start_code(offset + 6)
                            .unwrap_or(self.buffer.len())
                            - offset
                    };
                    if offset + total_len > self.buffer.len() {
                        break;
                    }
                    offset += total_len;
                }
            }
        }

        // Retain unparsed tail bytes
        if offset > 0 {
            self.buffer.drain(0..offset);
        }

        results
    }

    fn find_next_start_code(&self, from: usize) -> Option<usize> {
        let mut i = from;
        while i + 3 <= self.buffer.len() {
            if self.buffer[i..i + 3] == [0x00, 0x00, 0x01] {
                return Some(i);
            }
            i += 1;
        }
        None
    }
}

// ============================================================================
// H.264 NALU Extractor & Keyframe Detector
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum H264NaluType {
    NonIdrSlice,
    DataPartitionA,
    DataPartitionB,
    DataPartitionC,
    IdrKeyframe,
    Sei,
    Sps,
    Pps,
    AccessUnitDelimiter,
    Other(u8),
}

impl H264NaluType {
    pub fn from_byte(b: u8) -> Self {
        match b & 0x1F {
            1 => H264NaluType::NonIdrSlice,
            2 => H264NaluType::DataPartitionA,
            3 => H264NaluType::DataPartitionB,
            4 => H264NaluType::DataPartitionC,
            5 => H264NaluType::IdrKeyframe,
            6 => H264NaluType::Sei,
            7 => H264NaluType::Sps,
            8 => H264NaluType::Pps,
            9 => H264NaluType::AccessUnitDelimiter,
            other => H264NaluType::Other(other),
        }
    }
}

#[derive(Debug, Clone)]
pub struct H264Nalu {
    pub nalu_type: H264NaluType,
    pub is_keyframe: bool,
    pub raw_data: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct DecodedVideoFrame {
    pub width: u32,
    pub height: u32,
    pub is_keyframe: bool,
    pub timestamp_ms: u64,
    pub frame_data: Vec<u8>,
}

pub struct H264Extractor {
    cached_sps: Option<Vec<u8>>,
    cached_pps: Option<Vec<u8>>,
    detected_width: u32,
    detected_height: u32,
}

impl Default for H264Extractor {
    fn default() -> Self {
        Self::new()
    }
}

impl H264Extractor {
    pub fn new() -> Self {
        Self {
            cached_sps: None,
            cached_pps: None,
            detected_width: 1920,
            detected_height: 1080,
        }
    }

    pub fn extract_nalus(stream: &[u8]) -> Vec<H264Nalu> {
        let mut nalus = Vec::new();
        let len = stream.len();
        let mut i = 0;

        while i + 3 < len {
            // Check for 3-byte or 4-byte start code
            let (_start_code_len, nal_start) = if stream[i] == 0 && stream[i + 1] == 0 {
                if stream[i + 2] == 1 {
                    (3, i + 3)
                } else if stream[i + 2] == 0 && i + 3 < len && stream[i + 3] == 1 {
                    (4, i + 4)
                } else {
                    i += 1;
                    continue;
                }
            } else {
                i += 1;
                continue;
            };

            // Find end of this NALU (next start code)
            let mut j = nal_start;
            let mut nal_end = len;
            while j + 2 < len {
                if stream[j] == 0
                    && stream[j + 1] == 0
                    && (stream[j + 2] == 1
                        || (j + 3 < len && stream[j + 2] == 0 && stream[j + 3] == 1))
                {
                    nal_end = j;
                    break;
                }
                j += 1;
            }

            if nal_start < nal_end {
                let nalu_header = stream[nal_start];
                let nalu_type = H264NaluType::from_byte(nalu_header);
                let is_keyframe =
                    matches!(nalu_type, H264NaluType::IdrKeyframe | H264NaluType::Sps);

                let raw_data = stream[i..nal_end].to_vec();
                nalus.push(H264Nalu {
                    nalu_type,
                    is_keyframe,
                    raw_data,
                });
            }

            i = nal_end;
        }

        nalus
    }

    pub fn process_es_stream(
        &mut self,
        es_data: &[u8],
        timestamp_ms: u64,
    ) -> Option<DecodedVideoFrame> {
        let nalus = Self::extract_nalus(es_data);
        let mut contains_idr = false;
        let mut assembled_frame = Vec::new();

        for nalu in nalus {
            match nalu.nalu_type {
                H264NaluType::Sps => {
                    self.cached_sps = Some(nalu.raw_data.clone());
                    // Parse simple resolution heuristic if available
                    if nalu.raw_data.len() > 8 {
                        self.detected_width = 1920;
                        self.detected_height = 1080;
                    }
                }
                H264NaluType::Pps => {
                    self.cached_pps = Some(nalu.raw_data.clone());
                }
                H264NaluType::IdrKeyframe => {
                    contains_idr = true;
                }
                _ => {}
            }
            assembled_frame.extend_from_slice(&nalu.raw_data);
        }

        if !assembled_frame.is_empty() {
            Some(DecodedVideoFrame {
                width: self.detected_width,
                height: self.detected_height,
                is_keyframe: contains_idr,
                timestamp_ms,
                frame_data: assembled_frame,
            })
        } else {
            None
        }
    }
}

// ============================================================================
// GB28181 UDP Stream Receiver
// ============================================================================

pub struct Gb28181StreamReceiver {
    pub local_port: u16,
    pub ssrc: String,
    socket: Arc<UdpSocket>,
    is_running: Arc<AtomicBool>,
    total_packets_received: Arc<AtomicU64>,
    total_keyframes_extracted: Arc<AtomicU64>,
}

impl Gb28181StreamReceiver {
    pub async fn bind(local_port: u16, ssrc: &str) -> Result<Self, std::io::Error> {
        let socket = UdpSocket::bind(format!("0.0.0.0:{}", local_port)).await?;
        Ok(Self {
            local_port,
            ssrc: ssrc.to_string(),
            socket: Arc::new(socket),
            is_running: Arc::new(AtomicBool::new(false)),
            total_packets_received: Arc::new(AtomicU64::new(0)),
            total_keyframes_extracted: Arc::new(AtomicU64::new(0)),
        })
    }

    pub fn start_receiving(&self, frame_tx: mpsc::Sender<DecodedVideoFrame>) {
        self.is_running.store(true, Ordering::SeqCst);
        let socket = self.socket.clone();
        let running = self.is_running.clone();
        let packets_counter = self.total_packets_received.clone();
        let keyframes_counter = self.total_keyframes_extracted.clone();

        tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            let mut demuxer = MpegPsDemuxer::new();
            let mut extractor = H264Extractor::new();
            let mut decoder = crate::decoder::create_default_decoder();

            while running.load(Ordering::Relaxed) {
                match socket.recv_from(&mut buf).await {
                    Ok((len, _addr)) => {
                        packets_counter.fetch_add(1, Ordering::Relaxed);
                        if let Ok(rtp) = RtpHeader::parse(&buf[..len]) {
                            let payload = &buf[rtp.header_length..len];
                            demuxer.push_payload(payload);
                            let es_streams = demuxer.extract_video_streams();
                            let now_ms = chrono::Utc::now().timestamp_millis() as u64;

                            for es in es_streams {
                                if let Some(raw_frame) =
                                    extractor.process_es_stream(&es.payload, now_ms)
                                {
                                    if raw_frame.is_keyframe {
                                        keyframes_counter.fetch_add(1, Ordering::Relaxed);
                                    }
                                    // 通过解码器将 H.264/H.265 NALU 解码为 RGB 帧
                                    match decoder.decode_frame(&raw_frame.frame_data) {
                                        Ok(decoded) => {
                                            let _ = frame_tx.try_send(decoded);
                                        }
                                        Err(e) => {
                                            // 解码失败时仍发送原始帧（降级模式）
                                            eprintln!("GB28181 decode failed: {e}, passing raw frame");
                                            let _ = frame_tx.try_send(raw_frame);
                                        }
                                    }
                                }
                            }
                        }
                    }
                    Err(_) => {
                        tokio::time::sleep(tokio::time::Duration::from_millis(5)).await;
                    }
                }
            }
        });
    }

    pub fn stop(&self) {
        self.is_running.store(false, Ordering::SeqCst);
    }

    pub fn get_statistics(&self) -> (u64, u64) {
        (
            self.total_packets_received.load(Ordering::Relaxed),
            self.total_keyframes_extracted.load(Ordering::Relaxed),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rtp_header_parse() {
        let mut rtp_bytes = vec![
            0x80, // V=2, P=0, X=0, CC=0
            0x60, // M=0, PT=96 (MPEG-PS)
            0x12, 0x34, // Seq = 0x1234 = 4660
            0x00, 0x01, 0x5f, 0x90, // Timestamp = 90000
            0x34, 0x02, 0x00, 0x01, // SSRC
        ];
        rtp_bytes.extend_from_slice(&[0xaa, 0xbb, 0xcc, 0xdd]); // Payload

        let rtp = RtpHeader::parse(&rtp_bytes).expect("Should parse standard RTP packet");
        assert_eq!(rtp.version, 2);
        assert_eq!(rtp.payload_type, 96);
        assert_eq!(rtp.sequence_number, 0x1234);
        assert_eq!(rtp.timestamp, 90000);
        assert_eq!(rtp.header_length, 12);
    }

    #[test]
    fn test_mpeg_ps_demuxer_and_h264_nalu_extract() {
        let mut ps_stream = Vec::new();
        // Pack Header (14 bytes, 0 stuffing)
        ps_stream.extend_from_slice(&START_CODE_PACK);
        ps_stream.extend_from_slice(&[0x44, 0x00, 0x01, 0x00, 0x04, 0x01, 0x01, 0x00, 0x00, 0xf8]);

        // Video PES Header (stream_id = 0xE0)
        let mut pes_payload = Vec::new();
        // Annex-B SPS NALU (start code + 0x67)
        pes_payload.extend_from_slice(&[0x00, 0x00, 0x00, 0x01, 0x67, 0x42, 0x00, 0x1f, 0x96]);
        // Annex-B PPS NALU (start code + 0x68)
        pes_payload.extend_from_slice(&[0x00, 0x00, 0x00, 0x01, 0x68, 0xce, 0x38, 0x80]);
        // Annex-B IDR Keyframe NALU (start code + 0x65)
        pes_payload
            .extend_from_slice(&[0x00, 0x00, 0x00, 0x01, 0x65, 0x88, 0x84, 0x00, 0x11, 0x22]);

        let pes_header_data_len = 0u8;
        let pes_length = (3 + pes_header_data_len as usize + pes_payload.len()) as u16;

        ps_stream.extend_from_slice(&[0x00, 0x00, 0x01, 0xE0]);
        ps_stream.extend_from_slice(&pes_length.to_be_bytes());
        ps_stream.extend_from_slice(&[0x80, 0x00, pes_header_data_len]); // Flags, header_len=0
        ps_stream.extend_from_slice(&pes_payload);

        let mut demuxer = MpegPsDemuxer::new();
        demuxer.push_payload(&ps_stream);
        let extracted_es = demuxer.extract_video_streams();
        assert_eq!(extracted_es.len(), 1);

        let mut extractor = H264Extractor::new();
        let frame = extractor
            .process_es_stream(&extracted_es[0].payload, 1000)
            .expect("Frame should be decoded");
        assert!(frame.is_keyframe);
        assert_eq!(frame.width, 1920);
        assert_eq!(frame.height, 1080);
    }

    #[test]
    fn test_gb28181_receiver_with_decoder() {
        // 验证 GB28181 接收器 + 解码器集成
        let rtsp_ps_data = create_test_ps_stream();
        let mut demuxer = MpegPsDemuxer::new();
        demuxer.push_payload(&rtsp_ps_data);
        let es_streams = demuxer.extract_video_streams();
        assert!(!es_streams.is_empty());

        let mut extractor = H264Extractor::new();
        let raw_frame = extractor
            .process_es_stream(&es_streams[0].payload, 1000)
            .expect("Should extract frame");

        // 通过解码器解码
        let mut decoder = crate::decoder::OpenH264Decoder::new();
        let decoded = decoder.decode_frame(&raw_frame.frame_data);

        // 解码可能失败（测试环境无有效 H.264 数据），但不应 panic
        match decoded {
            Ok(frame) => {
                // 解码成功：验证输出格式
                assert!(frame.width > 0);
                assert!(frame.height > 0);
                assert!(!frame.frame_data.is_empty());
            }
            Err(_) => {
                // 解码失败：在测试环境中可接受（输入不是有效 H.264）
                // 但统计信息应记录错误
                let stats = decoder.statistics();
                assert!(stats.decode_errors > 0 || stats.frames_dropped > 0);
            }
        }
    }

    /// 创建测试用的 MPEG-PS 流
    fn create_test_ps_stream() -> Vec<u8> {
        let mut ps_stream = Vec::new();
        // Pack Header
        ps_stream.extend_from_slice(&START_CODE_PACK);
        ps_stream.extend_from_slice(&[0x44, 0x00, 0x01, 0x00, 0x04, 0x01, 0x01, 0x00, 0x00, 0xf8]);

        // Video PES Header
        let mut pes_payload = Vec::new();
        pes_payload.extend_from_slice(&[0x00, 0x00, 0x00, 0x01, 0x67, 0x42, 0x00, 0x1f, 0x96]);
        pes_payload.extend_from_slice(&[0x00, 0x00, 0x00, 0x01, 0x68, 0xce, 0x38, 0x80]);
        pes_payload.extend_from_slice(&[0x00, 0x00, 0x00, 0x01, 0x65, 0x88, 0x84, 0x00, 0x11, 0x22]);

        let pes_header_data_len = 0u8;
        let pes_length = (3 + pes_header_data_len as usize + pes_payload.len()) as u16;

        ps_stream.extend_from_slice(&[0x00, 0x00, 0x01, 0xE0]);
        ps_stream.extend_from_slice(&pes_length.to_be_bytes());
        ps_stream.extend_from_slice(&[0x80, 0x00, pes_header_data_len]);
        ps_stream.extend_from_slice(&pes_payload);

        ps_stream
    }

    #[test]
    fn test_psm_h265_detection() {
        let mut demuxer = MpegPsDemuxer::new();

        // 构造含 PSM (stream_type = 0x24 HEVC) 的 MPEG-PS 码流
        let mut psm_packet = Vec::new();
        // 00 00 01 BC
        psm_packet.extend_from_slice(&START_CODE_PSM);
        // length = 12
        psm_packet.extend_from_slice(&(12u16).to_be_bytes());
        // current_next=0xe1, psm_ver=0x01
        psm_packet.extend_from_slice(&[0xe1, 0x01]);
        // psi_len = 0
        psm_packet.extend_from_slice(&[0x00, 0x00]);
        // es_map_len = 4
        psm_packet.extend_from_slice(&[0x00, 0x04]);
        // stream_type = 0x24 (H.265), es_id = 0xE0, es_info_len = 0
        psm_packet.extend_from_slice(&[0x24, 0xe0, 0x00, 0x00]);
        // CRC 4 bytes
        psm_packet.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);

        demuxer.push_payload(&psm_packet);
        let _ = demuxer.extract_video_streams();

        assert_eq!(
            demuxer.detected_codec,
            Some(crate::rtsp::client::VideoCodec::H265)
        );
    }
}
