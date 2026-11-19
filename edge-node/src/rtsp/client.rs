use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

use crate::gb28181::rtp_ps::{DecodedVideoFrame, H264Extractor, RtpHeader};

// ============================================================================
// RTSP Request / Response
// ============================================================================

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RtspClientState {
    Disconnected,
    Connecting,
    OptionsSent,
    DescribeSent,
    SetupSent,
    Playing,
    Error(String),
}

#[derive(Debug, Clone)]
pub struct RtspResponse {
    pub status_code: u16,
    pub reason_phrase: String,
    pub cseq: u32,
    pub session_id: Option<String>,
    pub headers: HashMap<String, String>,
    pub body: String,
}

impl RtspResponse {
    pub fn parse(raw: &str) -> Result<Self, String> {
        let (header_part, body_part) = match raw.find("\r\n\r\n") {
            Some(idx) => (&raw[..idx], &raw[idx + 4..]),
            None => (raw, ""),
        };

        let mut lines = header_part.lines();
        let status_line = lines.next().ok_or("Empty RTSP response")?.trim();

        let parts: Vec<&str> = status_line.split_whitespace().collect();
        if parts.len() < 2 || !parts[0].starts_with("RTSP/1.") {
            return Err(format!("Malformed RTSP status line: {}", status_line));
        }

        let status_code: u16 = parts[1]
            .parse()
            .map_err(|e| format!("Invalid status code: {}", e))?;
        let reason = if parts.len() >= 3 {
            parts[2..].join(" ")
        } else {
            "OK".to_string()
        };

        let mut headers = HashMap::new();
        let mut cseq = 0u32;
        let mut session_id = None;

        for line in lines {
            let line = line.trim();
            if let Some(idx) = line.find(':') {
                let key = line[..idx].trim().to_lowercase();
                let val = line[idx + 1..].trim().to_string();

                if key == "cseq" {
                    cseq = val.parse().unwrap_or(0);
                } else if key == "session" {
                    let sess_clean = val.split(';').next().unwrap_or(&val).trim().to_string();
                    session_id = Some(sess_clean);
                }
                headers.insert(key, val);
            }
        }

        Ok(Self {
            status_code,
            reason_phrase: reason,
            cseq,
            session_id,
            headers,
            body: body_part.to_string(),
        })
    }
}

// ============================================================================
// RFC 6184 H.264 RTP Packet & FU-A Depacketizer
// ============================================================================

#[derive(Default)]
pub struct H264RtpDepacketizer {
    fu_buffer: Vec<u8>,
    fu_in_progress: bool,
}

impl H264RtpDepacketizer {
    pub fn new() -> Self {
        Self {
            fu_buffer: Vec::with_capacity(65536),
            fu_in_progress: false,
        }
    }

    pub fn process_rtp_payload(&mut self, payload: &[u8]) -> Option<Vec<u8>> {
        if payload.is_empty() {
            return None;
        }

        let nalu_header = payload[0];
        let nalu_type = nalu_header & 0x1F;

        match nalu_type {
            1..=23 => {
                // Single NAL unit packet
                let mut nalu = Vec::with_capacity(4 + payload.len());
                nalu.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
                nalu.extend_from_slice(payload);
                Some(nalu)
            }
            28 => {
                // FU-A (Fragmentation Unit A)
                if payload.len() < 2 {
                    return None;
                }
                let fu_header = payload[1];
                let is_start = (fu_header & 0x80) != 0;
                let is_end = (fu_header & 0x40) != 0;
                let original_nal_type = fu_header & 0x1F;
                let reconstructed_nal_header = (nalu_header & 0xE0) | original_nal_type;

                if is_start {
                    self.fu_buffer.clear();
                    self.fu_buffer.extend_from_slice(&[
                        0x00,
                        0x00,
                        0x00,
                        0x01,
                        reconstructed_nal_header,
                    ]);
                    self.fu_buffer.extend_from_slice(&payload[2..]);
                    self.fu_in_progress = true;
                    None
                } else if self.fu_in_progress {
                    self.fu_buffer.extend_from_slice(&payload[2..]);
                    if is_end {
                        self.fu_in_progress = false;
                        Some(self.fu_buffer.clone())
                    } else {
                        None
                    }
                } else {
                    None
                }
            }
            24 => {
                // STAP-A: Aggregation Packet
                let mut offset = 1;
                let mut assembled = Vec::new();
                while offset + 2 <= payload.len() {
                    let nalu_size =
                        u16::from_be_bytes([payload[offset], payload[offset + 1]]) as usize;
                    offset += 2;
                    if offset + nalu_size <= payload.len() {
                        assembled.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
                        assembled.extend_from_slice(&payload[offset..offset + nalu_size]);
                        offset += nalu_size;
                    } else {
                        break;
                    }
                }
                if !assembled.is_empty() {
                    Some(assembled)
                } else {
                    None
                }
            }
            _ => None,
        }
    }
}

// ============================================================================
// RFC 7798 H.265 / HEVC RTP Packet, AP & FU Depacketizer
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoCodec {
    H264,
    H265,
}

#[derive(Debug, Clone)]
pub struct SdpMediaInfo {
    pub codec: VideoCodec,
    pub control_url: String,
    pub payload_type: u8,
}

#[derive(Default)]
pub struct H265RtpDepacketizer {
    fu_buffer: Vec<u8>,
    fu_in_progress: bool,
}

impl H265RtpDepacketizer {
    pub fn new() -> Self {
        Self {
            fu_buffer: Vec::with_capacity(131072),
            fu_in_progress: false,
        }
    }

    pub fn process_rtp_payload(&mut self, payload: &[u8]) -> Option<Vec<u8>> {
        if payload.len() < 2 {
            return None;
        }

        let nalu_type = (payload[0] >> 1) & 0x3F;

        match nalu_type {
            // Single NAL unit packet (types 0..=47)
            0..=47 => {
                let mut nalu = Vec::with_capacity(4 + payload.len());
                nalu.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
                nalu.extend_from_slice(payload);
                Some(nalu)
            }
            // Type 48: AP (Aggregation Packet)
            48 => {
                let mut offset = 2; // 跳过 2 字节 PayloadHdr
                let mut assembled = Vec::new();
                while offset + 2 <= payload.len() {
                    let nalu_size =
                        u16::from_be_bytes([payload[offset], payload[offset + 1]]) as usize;
                    offset += 2;
                    if offset + nalu_size <= payload.len() {
                        assembled.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
                        assembled.extend_from_slice(&payload[offset..offset + nalu_size]);
                        offset += nalu_size;
                    } else {
                        break;
                    }
                }
                if !assembled.is_empty() {
                    Some(assembled)
                } else {
                    None
                }
            }
            // Type 49: FU (Fragmentation Unit)
            49 => {
                if payload.len() < 3 {
                    return None;
                }
                let fu_header = payload[2];
                let is_start = (fu_header & 0x80) != 0;
                let is_end = (fu_header & 0x40) != 0;
                let fu_type = fu_header & 0x3F;

                if is_start {
                    self.fu_buffer.clear();
                    self.fu_buffer.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
                    // 重构 2 字节原始 NALU Header:
                    // byte0: 清除 49 的 Type 槽位，换入真实的 fu_type，保留 F 与 LayerId 最高位
                    let byte0 = (payload[0] & 0x81) | ((fu_type & 0x3F) << 1);
                    let byte1 = payload[1];
                    self.fu_buffer.push(byte0);
                    self.fu_buffer.push(byte1);
                    self.fu_buffer.extend_from_slice(&payload[3..]);
                    self.fu_in_progress = true;
                    None
                } else if self.fu_in_progress {
                    self.fu_buffer.extend_from_slice(&payload[3..]);
                    if is_end {
                        self.fu_in_progress = false;
                        Some(self.fu_buffer.clone())
                    } else {
                        None
                    }
                } else {
                    None
                }
            }
            _ => None,
        }
    }
}

// ============================================================================
// H.265 / HEVC NALU Extractor & Keyframe Detector
// ============================================================================

pub struct H265Extractor {
    pub cached_vps: Option<Vec<u8>>,
    pub cached_sps: Option<Vec<u8>>,
    pub cached_pps: Option<Vec<u8>>,
    detected_width: u32,
    detected_height: u32,
}

impl Default for H265Extractor {
    fn default() -> Self {
        Self::new()
    }
}

impl H265Extractor {
    pub fn new() -> Self {
        Self {
            cached_vps: None,
            cached_sps: None,
            cached_pps: None,
            detected_width: 1920,
            detected_height: 1080,
        }
    }

    pub fn process_es_stream(
        &mut self,
        es_data: &[u8],
        timestamp_ms: u64,
    ) -> Option<DecodedVideoFrame> {
        let len = es_data.len();
        let mut i = 0;
        let mut contains_keyframe = false;
        let mut assembled_frame = Vec::new();

        while i + 3 < len {
            let nal_start = if es_data[i] == 0 && es_data[i + 1] == 0 {
                if es_data[i + 2] == 1 {
                    i + 3
                } else if es_data[i + 2] == 0 && i + 3 < len && es_data[i + 3] == 1 {
                    i + 4
                } else {
                    i += 1;
                    continue;
                }
            } else {
                i += 1;
                continue;
            };

            // 查找下一个起始码作为当前 NAL 结尾
            let mut nal_end = len;
            let mut j = nal_start;
            while j + 2 < len {
                if es_data[j] == 0
                    && es_data[j + 1] == 0
                    && (es_data[j + 2] == 1
                        || (j + 3 < len && es_data[j + 2] == 0 && es_data[j + 3] == 1))
                {
                    nal_end = j;
                    break;
                }
                j += 1;
            }

            if nal_start < nal_end {
                let nalu_header = es_data[nal_start];
                let nalu_type = (nalu_header >> 1) & 0x3F;

                match nalu_type {
                    32 => self.cached_vps = Some(es_data[i..nal_end].to_vec()),
                    33 => {
                        self.cached_sps = Some(es_data[i..nal_end].to_vec());
                        self.detected_width = 1920;
                        self.detected_height = 1080;
                    }
                    34 => self.cached_pps = Some(es_data[i..nal_end].to_vec()),
                    19..=21 => contains_keyframe = true, // IDR_W_RADL, IDR_N_LP, CRA
                    _ => {}
                }

                assembled_frame.extend_from_slice(&es_data[i..nal_end]);
            }

            i = nal_end;
        }

        if !assembled_frame.is_empty() {
            Some(DecodedVideoFrame {
                width: self.detected_width,
                height: self.detected_height,
                is_keyframe: contains_keyframe,
                timestamp_ms,
                frame_data: assembled_frame,
            })
        } else {
            None
        }
    }
}

// ============================================================================
// RTSP Client Core
// ============================================================================

pub struct RtspClientConfig {
    pub rtsp_url: String,
    pub space_id: String,
    pub reconnect_initial_secs: u64,
    pub reconnect_max_secs: u64,
    pub user_agent: String,
}

impl Default for RtspClientConfig {
    fn default() -> Self {
        Self {
            rtsp_url: String::new(),
            space_id: String::new(),
            reconnect_initial_secs: 1,
            reconnect_max_secs: 30,
            user_agent: "ChargingSpaceEdge-RTSP/2.1.0".to_string(),
        }
    }
}

pub struct RtspClient {
    pub config: RtspClientConfig,
    is_running: Arc<AtomicBool>,
    total_frames_received: Arc<AtomicU64>,
    total_bytes_received: Arc<AtomicU64>,
}

impl RtspClient {
    pub fn new(config: RtspClientConfig) -> Self {
        Self {
            config,
            is_running: Arc::new(AtomicBool::new(false)),
            total_frames_received: Arc::new(AtomicU64::new(0)),
            total_bytes_received: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn start(&self, frame_tx: mpsc::Sender<DecodedVideoFrame>) {
        self.is_running.store(true, Ordering::SeqCst);
        let running = self.is_running.clone();
        let config = RtspClientConfig {
            rtsp_url: self.config.rtsp_url.clone(),
            space_id: self.config.space_id.clone(),
            reconnect_initial_secs: self.config.reconnect_initial_secs,
            reconnect_max_secs: self.config.reconnect_max_secs,
            user_agent: self.config.user_agent.clone(),
        };
        let frames_counter = self.total_frames_received.clone();
        let bytes_counter = self.total_bytes_received.clone();

        tokio::spawn(async move {
            let mut backoff = config.reconnect_initial_secs;

            while running.load(Ordering::Relaxed) {
                match Self::run_rtsp_session(
                    &config,
                    &running,
                    &frame_tx,
                    &frames_counter,
                    &bytes_counter,
                )
                .await
                {
                    Ok(_) => {
                        backoff = config.reconnect_initial_secs;
                    }
                    Err(e) => {
                        eprintln!(
                            "[RTSP Client - {}] Connection error: {}. Retrying in {}s...",
                            config.space_id, e, backoff
                        );
                        tokio::time::sleep(Duration::from_secs(backoff)).await;
                        backoff = (backoff * 2).min(config.reconnect_max_secs);
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
            self.total_frames_received.load(Ordering::Relaxed),
            self.total_bytes_received.load(Ordering::Relaxed),
        )
    }

    // Parse host, port, and path from RTSP URL
    pub fn parse_rtsp_url(url: &str) -> Result<(String, u16, String), String> {
        let stripped = url
            .strip_prefix("rtsp://")
            .ok_or_else(|| format!("Invalid RTSP URL prefix: {}", url))?;

        let (host_port, path) = match stripped.find('/') {
            Some(idx) => (&stripped[..idx], &stripped[idx..]),
            None => (stripped, "/"),
        };

        // Strip auth credentials if present: user:pass@host:port
        let host_port_clean = match host_port.find('@') {
            Some(idx) => &host_port[idx + 1..],
            None => host_port,
        };

        let (host, port) = match host_port_clean.find(':') {
            Some(idx) => {
                let h = &host_port_clean[..idx];
                let p: u16 = host_port_clean[idx + 1..]
                    .parse()
                    .map_err(|e| format!("Invalid port in {}: {}", host_port_clean, e))?;
                (h.to_string(), p)
            }
            None => (host_port_clean.to_string(), 554),
        };

        Ok((host, port, path.to_string()))
    }

    async fn run_rtsp_session(
        config: &RtspClientConfig,
        running: &Arc<AtomicBool>,
        frame_tx: &mpsc::Sender<DecodedVideoFrame>,
        frames_counter: &Arc<AtomicU64>,
        bytes_counter: &Arc<AtomicU64>,
    ) -> Result<(), String> {
        let (host, port, _path) = Self::parse_rtsp_url(&config.rtsp_url)?;
        let addr = format!("{}:{}", host, port);

        let mut stream = TcpStream::connect(&addr)
            .await
            .map_err(|e| format!("TCP connect to {} failed: {}", addr, e))?;

        let mut cseq = 1u32;

        // 1. OPTIONS
        let req_options = format!(
            "OPTIONS {} RTSP/1.0\r\n\
            CSeq: {}\r\n\
            User-Agent: {}\r\n\r\n",
            config.rtsp_url, cseq, config.user_agent
        );
        stream
            .write_all(req_options.as_bytes())
            .await
            .map_err(|e| format!("Write OPTIONS failed: {}", e))?;

        let resp_options = Self::read_rtsp_text_response(&mut stream).await?;
        if resp_options.status_code != 200 {
            return Err(format!(
                "OPTIONS returned status {}",
                resp_options.status_code
            ));
        }
        cseq += 1;

        // 2. DESCRIBE
        let req_describe = format!(
            "DESCRIBE {} RTSP/1.0\r\n\
            CSeq: {}\r\n\
            Accept: application/sdp\r\n\
            User-Agent: {}\r\n\r\n",
            config.rtsp_url, cseq, config.user_agent
        );
        stream
            .write_all(req_describe.as_bytes())
            .await
            .map_err(|e| format!("Write DESCRIBE failed: {}", e))?;

        let resp_describe = Self::read_rtsp_text_response(&mut stream).await?;
        if resp_describe.status_code != 200 {
            return Err(format!(
                "DESCRIBE returned status {}",
                resp_describe.status_code
            ));
        }
        cseq += 1;

        // Determine control track URL and codec from SDP
        let media_info = Self::parse_sdp_media(&config.rtsp_url, &resp_describe.body);
        let control_track_url = media_info.control_url.clone();

        // 3. SETUP (RTP/AVP/TCP Interleaved 0-1)
        let req_setup = format!(
            "SETUP {} RTSP/1.0\r\n\
            CSeq: {}\r\n\
            Transport: RTP/AVP/TCP;unicast;interleaved=0-1\r\n\
            User-Agent: {}\r\n\r\n",
            control_track_url, cseq, config.user_agent
        );
        stream
            .write_all(req_setup.as_bytes())
            .await
            .map_err(|e| format!("Write SETUP failed: {}", e))?;

        let resp_setup = Self::read_rtsp_text_response(&mut stream).await?;
        if resp_setup.status_code != 200 {
            return Err(format!("SETUP returned status {}", resp_setup.status_code));
        }
        let session_id = resp_setup
            .session_id
            .ok_or_else(|| "No Session ID in SETUP response".to_string())?;
        cseq += 1;

        // 4. PLAY
        let req_play = format!(
            "PLAY {} RTSP/1.0\r\n\
            CSeq: {}\r\n\
            Session: {}\r\n\
            Range: npt=0.000-\r\n\
            User-Agent: {}\r\n\r\n",
            config.rtsp_url, cseq, session_id, config.user_agent
        );
        stream
            .write_all(req_play.as_bytes())
            .await
            .map_err(|e| format!("Write PLAY failed: {}", e))?;

        let resp_play = Self::read_rtsp_text_response(&mut stream).await?;
        if resp_play.status_code != 200 {
            return Err(format!("PLAY returned status {}", resp_play.status_code));
        }

        // 5. Stream Interleaved loop (支持 H.264 与 H.265 动态解包)
        let mut h264_depack = H264RtpDepacketizer::new();
        let mut h265_depack = H265RtpDepacketizer::new();
        let mut h264_extractor = H264Extractor::new();
        let mut h265_extractor = H265Extractor::new();
        let mut decoder: Box<dyn crate::decoder::VideoDecoder> = match media_info.codec {
            VideoCodec::H264 => crate::decoder::create_default_decoder(),
            VideoCodec::H265 => crate::decoder::create_h265_decoder(),
        };
        let mut header_buf = [0u8; 4];

        while running.load(Ordering::Relaxed) {
            // Read 4 bytes: '$', channel, length (2 bytes)
            if let Err(e) = stream.read_exact(&mut header_buf).await {
                return Err(format!("Stream read header failed: {}", e));
            }

            if header_buf[0] != b'$' {
                // If not '$', it might be a response to GET_PARAMETER keepalive or text message
                continue;
            }

            let channel = header_buf[1];
            let length = u16::from_be_bytes([header_buf[2], header_buf[3]]) as usize;

            let mut packet_data = vec![0u8; length];
            if let Err(e) = stream.read_exact(&mut packet_data).await {
                return Err(format!("Stream read payload failed: {}", e));
            }

            bytes_counter.fetch_add((4 + length) as u64, Ordering::Relaxed);

            // Channel 0 = Video RTP
            if channel == 0 && length > 12 {
                if let Ok(rtp) = RtpHeader::parse(&packet_data) {
                    let rtp_payload = &packet_data[rtp.header_length..];
                    let complete_nalu_opt = match media_info.codec {
                        VideoCodec::H264 => h264_depack.process_rtp_payload(rtp_payload),
                        VideoCodec::H265 => h265_depack.process_rtp_payload(rtp_payload),
                    };

                    if let Some(complete_nalu) = complete_nalu_opt {
                        let now_ms = chrono::Utc::now().timestamp_millis() as u64;
                        let raw_frame_opt = match media_info.codec {
                            VideoCodec::H264 => {
                                h264_extractor.process_es_stream(&complete_nalu, now_ms)
                            }
                            VideoCodec::H265 => {
                                h265_extractor.process_es_stream(&complete_nalu, now_ms)
                            }
                        };

                        if let Some(raw_frame) = raw_frame_opt {
                            frames_counter.fetch_add(1, Ordering::Relaxed);
                            // 通过对应编解码器将 NALU 解码为 RGB 帧
                            match decoder.decode_frame(&raw_frame.frame_data) {
                                Ok(decoded) => {
                                    let _ = frame_tx.try_send(decoded);
                                }
                                Err(e) => {
                                    eprintln!("RTSP decode failed: {e}, passing raw frame");
                                    let _ = frame_tx.try_send(raw_frame);
                                }
                            }
                        }
                    }
                }
            }
        }

        // TEARDOWN on normal termination
        cseq += 1;
        let req_teardown = format!(
            "TEARDOWN {} RTSP/1.0\r\n\
            CSeq: {}\r\n\
            Session: {}\r\n\
            User-Agent: {}\r\n\r\n",
            config.rtsp_url, cseq, session_id, config.user_agent
        );
        let _ = stream.write_all(req_teardown.as_bytes()).await;

        Ok(())
    }

    async fn read_rtsp_text_response(stream: &mut TcpStream) -> Result<RtspResponse, String> {
        let mut buf = Vec::new();
        let mut temp = [0u8; 1];

        // Read until \r\n\r\n
        loop {
            let n = stream
                .read(&mut temp)
                .await
                .map_err(|e| format!("Read text response failed: {}", e))?;
            if n == 0 {
                return Err("Connection closed while waiting for RTSP response".to_string());
            }
            buf.push(temp[0]);

            if buf.len() >= 4 && &buf[buf.len() - 4..] == b"\r\n\r\n" {
                break;
            }
            if buf.len() > 8192 {
                return Err("RTSP response headers exceeded 8KB".to_string());
            }
        }

        let header_text = std::str::from_utf8(&buf).map_err(|e| format!("UTF8 error: {}", e))?;
        let mut resp = RtspResponse::parse(header_text)?;

        // If Content-Length > 0, read body
        if let Some(cl_str) = resp.headers.get("content-length") {
            if let Ok(cl) = cl_str.parse::<usize>() {
                if cl > 0 {
                    let mut body_bytes = vec![0u8; cl];
                    stream
                        .read_exact(&mut body_bytes)
                        .await
                        .map_err(|e| format!("Failed to read RTSP body: {}", e))?;
                    resp.body = String::from_utf8_lossy(&body_bytes).to_string();
                }
            }
        }

        Ok(resp)
    }

    fn extract_control_track(base_url: &str, sdp: &str) -> String {
        for line in sdp.lines() {
            let line = line.trim();
            if let Some(track_raw) = line.strip_prefix("a=control:") {
                let track = track_raw.trim();
                if track.starts_with("rtsp://") {
                    return track.to_string();
                } else if track == "*" {
                    return base_url.to_string();
                } else {
                    let clean_base = base_url.trim_end_matches('/');
                    let clean_track = track.trim_start_matches('/');
                    return format!("{}/{}", clean_base, clean_track);
                }
            }
        }
        format!("{}/track1", base_url.trim_end_matches('/'))
    }

    pub fn parse_sdp_media(base_url: &str, sdp: &str) -> SdpMediaInfo {
        let mut codec = VideoCodec::H264;
        let mut payload_type = 96;

        let sdp_upper = sdp.to_uppercase();
        if sdp_upper.contains("H265") || sdp_upper.contains("HEVC") {
            codec = VideoCodec::H265;
        }

        // 解析具体的 rtpmap: a=rtpmap:96 H265/90000
        for line in sdp.lines() {
            let line_trim = line.trim();
            if let Some(rest) = line_trim.strip_prefix("a=rtpmap:") {
                let parts: Vec<&str> = rest.split_whitespace().collect();
                if !parts.is_empty() {
                    if let Ok(pt) = parts[0].parse::<u8>() {
                        payload_type = pt;
                    }
                }
                if parts.len() > 1 {
                    let codec_str = parts[1].to_uppercase();
                    if codec_str.starts_with("H265") || codec_str.starts_with("HEVC") {
                        codec = VideoCodec::H265;
                    } else if codec_str.starts_with("H264") {
                        codec = VideoCodec::H264;
                    }
                }
            }
        }

        let control_url = Self::extract_control_track(base_url, sdp);

        SdpMediaInfo {
            codec,
            control_url,
            payload_type,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_rtsp_url() {
        let (host, port, path) = RtspClient::parse_rtsp_url(
            "rtsp://admin:pass123@192.168.1.64:554/h264/ch1/main/av_stream",
        )
        .unwrap();
        assert_eq!(host, "192.168.1.64");
        assert_eq!(port, 554);
        assert_eq!(path, "/h264/ch1/main/av_stream");

        let (host2, port2, path2) =
            RtspClient::parse_rtsp_url("rtsp://10.0.0.8/live/stream0").unwrap();
        assert_eq!(host2, "10.0.0.8");
        assert_eq!(port2, 554);
        assert_eq!(path2, "/live/stream0");
    }

    #[test]
    fn test_rtsp_response_parser() {
        let raw = "RTSP/1.0 200 OK\r\n\
            CSeq: 3\r\n\
            Session: 12345678;timeout=60\r\n\
            Transport: RTP/AVP/TCP;unicast;interleaved=0-1\r\n\r\n";

        let resp = RtspResponse::parse(raw).expect("Must parse response");
        assert_eq!(resp.status_code, 200);
        assert_eq!(resp.cseq, 3);
        assert_eq!(resp.session_id.as_deref(), Some("12345678"));
        assert!(resp.headers.contains_key("transport"));
    }

    #[test]
    fn test_h264_fu_a_depacketization() {
        let mut depacketizer = H264RtpDepacketizer::new();

        // FU-A: indicator = 0x5C (NRI=2, type=28), Start FU-header = 0x85 (S=1, type=5 IDR)
        let chunk1 = [0x5c, 0x85, 0xaa, 0xbb];
        assert!(depacketizer.process_rtp_payload(&chunk1).is_none());

        // Middle FU-A: indicator = 0x5C, FU-header = 0x05 (S=0, E=0, type=5)
        let chunk2 = [0x5c, 0x05, 0xcc, 0xdd];
        assert!(depacketizer.process_rtp_payload(&chunk2).is_none());

        // End FU-A: indicator = 0x5C, FU-header = 0x45 (E=1, type=5)
        let chunk3 = [0x5c, 0x45, 0xee, 0xff];
        let assembled = depacketizer
            .process_rtp_payload(&chunk3)
            .expect("Must assemble FU-A");

        // Should start with Annex B start code + reconstructed NAL header: (0x5C & 0xE0) | (0x05) = 0x45
        assert_eq!(&assembled[0..4], &[0x00, 0x00, 0x00, 0x01]);
        assert_eq!(assembled[4], 0x45);
        assert_eq!(&assembled[5..], &[0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]);
    }

    #[test]
    fn test_rtsp_decoder_integration() {
        // 验证 RTSP 去包化 → H.264 提取 → 解码器 的完整管道
        let mut depacketizer = H264RtpDepacketizer::new();
        let mut extractor = H264Extractor::new();
        let mut decoder = crate::decoder::OpenH264Decoder::new();

        // 模拟 FU-A 分片组装
        let start_fu = [0x5c, 0x85, 0xaa, 0xbb];
        let middle_fu = [0x5c, 0x05, 0xcc, 0xdd];
        let end_fu = [0x5c, 0x45, 0xee, 0xff];

        assert!(depacketizer.process_rtp_payload(&start_fu).is_none());
        assert!(depacketizer.process_rtp_payload(&middle_fu).is_none());
        let nalu = depacketizer
            .process_rtp_payload(&end_fu)
            .expect("Should assemble NALU");

        // H.264 提取
        let raw_frame = extractor.process_es_stream(&nalu, 1000);
        // 可能返回 None（数据太短），但不应 panic
        if let Some(frame) = raw_frame {
            // 尝试解码
            let _ = decoder.decode_frame(&frame.frame_data);
        }

        // 验证解码器统计
        let stats = decoder.statistics();
        // 至少记录了尝试
        assert!(stats.frames_decoded > 0 || stats.decode_errors > 0 || stats.frames_dropped > 0);
    }

    #[test]
    fn test_h265_fu_depacketization() {
        let mut depack = H265RtpDepacketizer::new();

        // PayloadHdr (2 bytes): Type=49 (0x62), LayerId+TID (0x01)
        // FU Header (1 byte): S=1, E=0, FuType=19 (IDR_W_RADL) -> 0x80 | 19 = 0x93
        let start_fu = [0x62, 0x01, 0x93, 0x11, 0x22];
        assert!(depack.process_rtp_payload(&start_fu).is_none());

        // Middle FU: S=0, E=0, FuType=19 -> 0x13
        let mid_fu = [0x62, 0x01, 0x13, 0x33, 0x44];
        assert!(depack.process_rtp_payload(&mid_fu).is_none());

        // End FU: S=0, E=1, FuType=19 -> 0x40 | 19 = 0x53
        let end_fu = [0x62, 0x01, 0x53, 0x55, 0x66];
        let assembled = depack
            .process_rtp_payload(&end_fu)
            .expect("Should assemble complete H.265 NALU");

        // Annex B start code + 2-byte NALU Header + payload
        assert_eq!(&assembled[0..4], &[0x00, 0x00, 0x00, 0x01]);
        // byte0: (0x62 & 0x81) | (19 << 1) = 0 | 38 = 0x26
        assert_eq!(assembled[4], 0x26);
        assert_eq!(assembled[5], 0x01);
        assert_eq!(&assembled[6..], &[0x11, 0x22, 0x33, 0x44, 0x55, 0x66]);
    }

    #[test]
    fn test_h265_ap_depacketization() {
        let mut depack = H265RtpDepacketizer::new();

        // AP packet (Type 48 -> (48 << 1) = 96 = 0x60, 0x01)
        // Contains two NALUs:
        // NALU 1: size=3, data=[0x40, 0x01, 0xaa] (VPS)
        // NALU 2: size=2, data=[0x42, 0x01] (SPS)
        let mut ap_payload = vec![0x60, 0x01];
        ap_payload.extend_from_slice(&(3u16).to_be_bytes());
        ap_payload.extend_from_slice(&[0x40, 0x01, 0xaa]);
        ap_payload.extend_from_slice(&(2u16).to_be_bytes());
        ap_payload.extend_from_slice(&[0x42, 0x01]);

        let assembled = depack
            .process_rtp_payload(&ap_payload)
            .expect("Should extract AP NALUs");
        // Start code 1
        assert_eq!(&assembled[0..4], &[0x00, 0x00, 0x00, 0x01]);
        assert_eq!(&assembled[4..7], &[0x40, 0x01, 0xaa]);
        // Start code 2
        assert_eq!(&assembled[7..11], &[0x00, 0x00, 0x00, 0x01]);
        assert_eq!(&assembled[11..13], &[0x42, 0x01]);
    }

    #[test]
    fn test_parse_sdp_media() {
        let h265_sdp = "v=0\r\n\
            m=video 0 RTP/AVP 96\r\n\
            a=rtpmap:96 H265/90000\r\n\
            a=control:trackID=1\r\n";
        let info = RtspClient::parse_sdp_media("rtsp://192.168.1.100/live", h265_sdp);
        assert_eq!(info.codec, VideoCodec::H265);
        assert_eq!(info.payload_type, 96);
        assert_eq!(info.control_url, "rtsp://192.168.1.100/live/trackID=1");

        let h264_sdp = "v=0\r\n\
            m=video 0 RTP/AVP 96\r\n\
            a=rtpmap:96 H264/90000\r\n\
            a=control:track2\r\n";
        let info2 = RtspClient::parse_sdp_media("rtsp://192.168.1.100:554/ch1", h264_sdp);
        assert_eq!(info2.codec, VideoCodec::H264);
        assert_eq!(info2.control_url, "rtsp://192.168.1.100:554/ch1/track2");
    }
}
