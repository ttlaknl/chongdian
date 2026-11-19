use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;
use tokio::sync::RwLock;

// ============================================================================
// RFC 1321 Pure Rust MD5 Implementation (Zero external crate dependency)
// ============================================================================

pub fn md5_hex(input: &[u8]) -> String {
    let mut message = Vec::with_capacity(input.len() + 64);
    message.extend_from_slice(input);
    let original_bit_len = (input.len() as u64).wrapping_mul(8);

    // Step 1: Append padding bit 1 (0x80)
    message.push(0x80);

    // Step 2: Append 0s until length in bytes % 64 == 56
    while message.len() % 64 != 56 {
        message.push(0x00);
    }

    // Step 3: Append original length as 64-bit little-endian integer
    message.extend_from_slice(&original_bit_len.to_le_bytes());

    // Step 4: Initialize MD5 state
    let mut a: u32 = 0x6745_2301;
    let mut b: u32 = 0xefcd_ab89;
    let mut c: u32 = 0x98ba_dcfe;
    let mut d: u32 = 0x1032_5476;

    // Shift amounts
    let s: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5,
        9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10,
        15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];

    // Precomputed sine table T[i] = floor(4294967296 * abs(sin(i + 1)))
    let k: [u32; 64] = [
        0xd76a_a478,
        0xe8c7_b756,
        0x2420_70db,
        0xc1bd_ceee,
        0xf57c_0faf,
        0x4787_c62a,
        0xa830_4613,
        0xfd46_9501,
        0x6980_98d8,
        0x8b44_f7af,
        0xffff_5bb1,
        0x895c_d7be,
        0x6b90_1122,
        0xfd98_7193,
        0xa679_438e,
        0x49b4_0821,
        0xf61e_2562,
        0xc040_b340,
        0x265e_5a51,
        0xe9b6_c7aa,
        0xd62f_105d,
        0x0244_1453,
        0xd8a1_e681,
        0xe7d3_fbc8,
        0x21e1_cde6,
        0xc337_07d6,
        0xf4d5_0d87,
        0x455a_14ed,
        0xa9e3_e905,
        0xfcefa3f8,
        0x676f_02d9,
        0x8d2a_4c8a,
        0xfffa_3942,
        0x8771_f681,
        0x6d9d_6122,
        0xfde5_380c,
        0xa4be_ea44,
        0x4bde_cfa9,
        0xf6bb_4b60,
        0xbebf_bc70,
        0x289b_7ec6,
        0xeaa1_27fa,
        0xd4ef_3085,
        0x0488_1d05,
        0xd9d4_d039,
        0xe6db_99e5,
        0x1fa2_7cf8,
        0xc4ac_5665,
        0xf429_2244,
        0x432a_ff97,
        0xab94_23a7,
        0xfc93_a039,
        0x655b_59c3,
        0x8f0c_cc92,
        0xffef_f47d,
        0x8584_5dd1,
        0x6fa8_7e4f,
        0xfe2c_e6e0,
        0xa301_4314,
        0x4e08_11a1,
        0xf753_7e82,
        0xbd3a_f235,
        0x2ad7_d2bb,
        0xeb86_d391,
    ];

    // Process each 64-byte block
    for chunk in message.chunks_exact(64) {
        let mut m = [0u32; 16];
        for i in 0..16 {
            m[i] = u32::from_le_bytes([
                chunk[i * 4],
                chunk[i * 4 + 1],
                chunk[i * 4 + 2],
                chunk[i * 4 + 3],
            ]);
        }

        let mut aa = a;
        let mut bb = b;
        let mut cc = c;
        let mut dd = d;

        for i in 0..64 {
            let (f, g) = match i {
                0..=15 => ((bb & cc) | ((!bb) & dd), i),
                16..=31 => ((dd & bb) | ((!dd) & cc), (5 * i + 1) % 16),
                32..=47 => (bb ^ cc ^ dd, (3 * i + 5) % 16),
                _ => (cc ^ (bb | (!dd)), (7 * i) % 16),
            };

            let temp = dd;
            dd = cc;
            cc = bb;
            let sum = aa.wrapping_add(f).wrapping_add(k[i]).wrapping_add(m[g]);
            bb = bb.wrapping_add(sum.rotate_left(s[i]));
            aa = temp;
        }

        a = a.wrapping_add(aa);
        b = b.wrapping_add(bb);
        c = c.wrapping_add(cc);
        d = d.wrapping_add(dd);
    }

    let mut result = [0u8; 16];
    result[0..4].copy_from_slice(&a.to_le_bytes());
    result[4..8].copy_from_slice(&b.to_le_bytes());
    result[8..12].copy_from_slice(&c.to_le_bytes());
    result[12..16].copy_from_slice(&d.to_le_bytes());

    let mut hex_str = String::with_capacity(32);
    for byte in &result {
        use std::fmt::Write;
        let _ = write!(&mut hex_str, "{:02x}", byte);
    }
    hex_str
}

// ============================================================================
// SIP Message Models & Parsing (GB/T 28181-2016 & RFC 3261)
// ============================================================================

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SipMethod {
    Register,
    Message,
    Invite,
    Ack,
    Bye,
    Cancel,
    Unknown(String),
}

impl std::str::FromStr for SipMethod {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s.to_uppercase().as_str() {
            "REGISTER" => SipMethod::Register,
            "MESSAGE" => SipMethod::Message,
            "INVITE" => SipMethod::Invite,
            "ACK" => SipMethod::Ack,
            "BYE" => SipMethod::Bye,
            "CANCEL" => SipMethod::Cancel,
            _ => SipMethod::Unknown(s.to_string()),
        })
    }
}

impl SipMethod {
    pub fn as_str(&self) -> &str {
        match self {
            SipMethod::Register => "REGISTER",
            SipMethod::Message => "MESSAGE",
            SipMethod::Invite => "INVITE",
            SipMethod::Ack => "ACK",
            SipMethod::Bye => "BYE",
            SipMethod::Cancel => "CANCEL",
            SipMethod::Unknown(s) => s.as_str(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct SipRequest {
    pub method: SipMethod,
    pub uri: String,
    pub headers: HashMap<String, String>,
    pub body: String,
}

#[derive(Debug, Clone)]
pub struct SipResponse {
    pub status_code: u16,
    pub reason_phrase: String,
    pub headers: HashMap<String, String>,
    pub body: String,
}

#[derive(Debug, Clone)]
pub enum SipPacket {
    Request(SipRequest),
    Response(SipResponse),
}

impl SipPacket {
    pub fn parse(raw: &str) -> Result<Self, String> {
        let (header_part, body_part) = match raw.find("\r\n\r\n") {
            Some(idx) => (&raw[..idx], &raw[idx + 4..]),
            None => match raw.find("\n\n") {
                Some(idx) => (&raw[..idx], &raw[idx + 2..]),
                None => (raw, ""),
            },
        };

        let mut lines = header_part.lines();
        let start_line = lines.next().ok_or("Empty SIP message")?.trim();

        let mut headers = HashMap::new();
        for line in lines {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Some(idx) = line.find(':') {
                let key = line[..idx].trim().to_lowercase();
                let val = line[idx + 1..].trim().to_string();
                headers.insert(key, val);
            }
        }

        if start_line.starts_with("SIP/2.0") {
            let parts: Vec<&str> = start_line.split_whitespace().collect();
            if parts.len() < 2 {
                return Err(format!("Malformed SIP status line: {}", start_line));
            }
            let status_code: u16 = parts[1]
                .parse()
                .map_err(|e| format!("Invalid status code: {}", e))?;
            let reason = if parts.len() >= 3 {
                parts[2..].join(" ")
            } else {
                "OK".to_string()
            };
            Ok(SipPacket::Response(SipResponse {
                status_code,
                reason_phrase: reason,
                headers,
                body: body_part.to_string(),
            }))
        } else {
            let parts: Vec<&str> = start_line.split_whitespace().collect();
            if parts.len() < 3 {
                return Err(format!("Malformed SIP request line: {}", start_line));
            }
            let method: SipMethod = parts[0]
                .parse()
                .unwrap_or(SipMethod::Unknown(parts[0].to_string()));
            let uri = parts[1].to_string();
            Ok(SipPacket::Request(SipRequest {
                method,
                uri,
                headers,
                body: body_part.to_string(),
            }))
        }
    }
}

// ============================================================================
// Digest Authentication (RFC 2617 / RFC 3261)
// ============================================================================

#[derive(Debug, Clone, Default)]
pub struct DigestAuthParams {
    pub username: String,
    pub realm: String,
    pub nonce: String,
    pub uri: String,
    pub response: String,
    pub algorithm: String,
    pub qop: Option<String>,
    pub nc: Option<String>,
    pub cnonce: Option<String>,
}

impl DigestAuthParams {
    pub fn parse(auth_header: &str) -> Option<Self> {
        let auth_str = auth_header.strip_prefix("Digest ")?;
        let mut params = DigestAuthParams {
            algorithm: "MD5".to_string(),
            ..Default::default()
        };

        for token in auth_str.split(',') {
            let token = token.trim();
            if let Some(eq_idx) = token.find('=') {
                let key = token[..eq_idx].trim();
                let mut val = token[eq_idx + 1..].trim();
                if val.starts_with('"') && val.ends_with('"') && val.len() >= 2 {
                    val = &val[1..val.len() - 1];
                }

                match key {
                    "username" => params.username = val.to_string(),
                    "realm" => params.realm = val.to_string(),
                    "nonce" => params.nonce = val.to_string(),
                    "uri" => params.uri = val.to_string(),
                    "response" => params.response = val.to_string(),
                    "algorithm" => params.algorithm = val.to_uppercase(),
                    "qop" => params.qop = Some(val.to_string()),
                    "nc" => params.nc = Some(val.to_string()),
                    "cnonce" => params.cnonce = Some(val.to_string()),
                    _ => {}
                }
            }
        }
        Some(params)
    }

    pub fn verify(&self, method: &str, password: &str) -> bool {
        let ha1 = md5_hex(format!("{}:{}:{}", self.username, self.realm, password).as_bytes());
        let ha2 = md5_hex(format!("{}:{}", method, self.uri).as_bytes());

        let expected_response = if let Some(qop) = &self.qop {
            let nc = self.nc.as_deref().unwrap_or("00000001");
            let cnonce = self.cnonce.as_deref().unwrap_or("");
            md5_hex(format!("{}:{}:{}:{}:{}:{}", ha1, self.nonce, nc, cnonce, qop, ha2).as_bytes())
        } else {
            md5_hex(format!("{}:{}:{}", ha1, self.nonce, ha2).as_bytes())
        };

        self.response.eq_ignore_ascii_case(&expected_response)
    }
}

// ============================================================================
// Device Registry & Session Management
// ============================================================================

#[derive(Debug, Clone)]
pub struct GbDeviceStatus {
    pub device_id: String,
    pub ip_addr: String,
    pub sip_port: u16,
    pub is_online: bool,
    pub last_keepalive: Instant,
    pub active_stream_id: Option<String>,
}

#[derive(Clone)]
pub struct Gb28181DeviceRegistry {
    devices: Arc<RwLock<HashMap<String, GbDeviceStatus>>>,
    realm: String,
    server_id: String,
    server_ip: String,
    server_port: u16,
    device_password: Arc<RwLock<String>>,
}

impl Gb28181DeviceRegistry {
    pub fn new(
        server_id: &str,
        server_ip: &str,
        server_port: u16,
        realm: &str,
        password: &str,
    ) -> Self {
        Self {
            devices: Arc::new(RwLock::new(HashMap::new())),
            realm: realm.to_string(),
            server_id: server_id.to_string(),
            server_ip: server_ip.to_string(),
            server_port,
            device_password: Arc::new(RwLock::new(password.to_string())),
        }
    }

    pub async fn get_device(&self, device_id: &str) -> Option<GbDeviceStatus> {
        let guard = self.devices.read().await;
        guard.get(device_id).cloned()
    }

    pub async fn list_online_devices(&self) -> Vec<GbDeviceStatus> {
        let guard = self.devices.read().await;
        guard.values().filter(|d| d.is_online).cloned().collect()
    }

    pub async fn update_keepalive(&self, device_id: &str) -> bool {
        let mut guard = self.devices.write().await;
        if let Some(dev) = guard.get_mut(device_id) {
            dev.last_keepalive = Instant::now();
            dev.is_online = true;
            true
        } else {
            false
        }
    }

    pub async fn check_offline_timeout(&self, timeout_secs: u64) -> Vec<String> {
        let mut timed_out = Vec::new();
        let mut guard = self.devices.write().await;
        let now = Instant::now();
        for (id, dev) in guard.iter_mut() {
            if dev.is_online
                && now.duration_since(dev.last_keepalive) > Duration::from_secs(timeout_secs)
            {
                dev.is_online = false;
                timed_out.push(id.clone());
            }
        }
        timed_out
    }

    pub fn generate_nonce() -> String {
        let raw = format!(
            "{}-{}",
            uuid::Uuid::new_v4(),
            chrono::Utc::now().timestamp_micros()
        );
        md5_hex(raw.as_bytes())
    }

    pub async fn handle_sip_request(
        &self,
        req: &SipRequest,
        remote_ip: &str,
        remote_port: u16,
    ) -> Option<String> {
        let call_id = req.headers.get("call-id")?.as_str();
        let cseq = req.headers.get("cseq")?.as_str();
        let from = req.headers.get("from")?.as_str();
        let to = req.headers.get("to")?.as_str();

        match req.method {
            SipMethod::Register => {
                let device_id = Self::extract_device_id_from_header(from)?;

                // Check authorization header
                if let Some(auth_val) = req.headers.get("authorization") {
                    if let Some(params) = DigestAuthParams::parse(auth_val) {
                        let pwd = self.device_password.read().await.clone();
                        if params.verify("REGISTER", &pwd) {
                            let mut guard = self.devices.write().await;
                            guard.insert(
                                device_id.clone(),
                                GbDeviceStatus {
                                    device_id: device_id.clone(),
                                    ip_addr: remote_ip.to_string(),
                                    sip_port: remote_port,
                                    is_online: true,
                                    last_keepalive: Instant::now(),
                                    active_stream_id: None,
                                },
                            );

                            let now_str =
                                chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S").to_string();
                            let resp = format!(
                                "SIP/2.0 200 OK\r\n\
                                Via: {}\r\n\
                                From: {}\r\n\
                                To: {};tag={}\r\n\
                                Call-ID: {}\r\n\
                                CSeq: {}\r\n\
                                Date: {}\r\n\
                                Contact: <sip:{}@{}:{}>\r\n\
                                Expires: 3600\r\n\
                                Content-Length: 0\r\n\r\n",
                                req.headers.get("via").map(|s| s.as_str()).unwrap_or(""),
                                from,
                                to,
                                &call_id[..call_id.len().min(8)],
                                call_id,
                                cseq,
                                now_str,
                                self.server_id,
                                self.server_ip,
                                self.server_port,
                            );
                            return Some(resp);
                        }
                    }
                }

                // First unauthenticated REGISTER or auth failed -> 401 Unauthorized
                let nonce = Self::generate_nonce();
                let resp = format!(
                    "SIP/2.0 401 Unauthorized\r\n\
                    Via: {}\r\n\
                    From: {}\r\n\
                    To: {};tag={}\r\n\
                    Call-ID: {}\r\n\
                    CSeq: {}\r\n\
                    WWW-Authenticate: Digest realm=\"{}\", nonce=\"{}\", algorithm=MD5\r\n\
                    Content-Length: 0\r\n\r\n",
                    req.headers.get("via").map(|s| s.as_str()).unwrap_or(""),
                    from,
                    to,
                    &call_id[..call_id.len().min(8)],
                    call_id,
                    cseq,
                    self.realm,
                    nonce,
                );
                Some(resp)
            }
            SipMethod::Message => {
                if req.body.contains("Keepalive") {
                    if let Some(dev_id) = Self::extract_xml_field(&req.body, "DeviceID") {
                        self.update_keepalive(&dev_id).await;
                    }
                }
                let resp = format!(
                    "SIP/2.0 200 OK\r\n\
                    Via: {}\r\n\
                    From: {}\r\n\
                    To: {};tag={}\r\n\
                    Call-ID: {}\r\n\
                    CSeq: {}\r\n\
                    Content-Length: 0\r\n\r\n",
                    req.headers.get("via").map(|s| s.as_str()).unwrap_or(""),
                    from,
                    to,
                    &call_id[..call_id.len().min(8)],
                    call_id,
                    cseq,
                );
                Some(resp)
            }
            SipMethod::Ack => None,
            SipMethod::Bye => {
                let resp = format!(
                    "SIP/2.0 200 OK\r\n\
                    Via: {}\r\n\
                    From: {}\r\n\
                    To: {}\r\n\
                    Call-ID: {}\r\n\
                    CSeq: {}\r\n\
                    Content-Length: 0\r\n\r\n",
                    req.headers.get("via").map(|s| s.as_str()).unwrap_or(""),
                    from,
                    to,
                    call_id,
                    cseq,
                );
                Some(resp)
            }
            _ => None,
        }
    }

    pub fn build_invite_request(
        &self,
        target_device_id: &str,
        target_ip: &str,
        target_port: u16,
        local_rtp_port: u16,
        ssrc: &str,
    ) -> (String, String) {
        let call_id = format!("call-{}", uuid::Uuid::new_v4());
        let sdp = format!(
            "v=0\r\n\
            o={} 0 0 IN IP4 {}\r\n\
            s=Play\r\n\
            c=IN IP4 {}\r\n\
            t=0 0\r\n\
            m=video {} RTP/AVP 96\r\n\
            a=recvonly\r\n\
            a=rtpmap:96 PS/90000\r\n\
            y={}\r\n",
            self.server_id, self.server_ip, self.server_ip, local_rtp_port, ssrc,
        );

        let req = format!(
            "INVITE sip:{}@{}:{} SIP/2.0\r\n\
            Via: SIP/2.0/UDP {}:{};branch=z9hG4bK{}\r\n\
            From: <sip:{}@{}:{}>;tag={}\r\n\
            To: <sip:{}@{}:{}>\r\n\
            Call-ID: {}\r\n\
            CSeq: 1 INVITE\r\n\
            Contact: <sip:{}@{}:{}>\r\n\
            Content-Type: application/sdp\r\n\
            Content-Length: {}\r\n\r\n{}",
            target_device_id,
            target_ip,
            target_port,
            self.server_ip,
            self.server_port,
            &call_id[..8],
            self.server_id,
            self.server_ip,
            self.server_port,
            &call_id[..8],
            target_device_id,
            target_ip,
            target_port,
            call_id,
            self.server_id,
            self.server_ip,
            self.server_port,
            sdp.len(),
            sdp,
        );

        (call_id, req)
    }

    pub fn extract_device_id_from_header(val: &str) -> Option<String> {
        let sip_pos = val.find("sip:")?;
        let rest = &val[sip_pos + 4..];
        let at_pos = rest.find('@').unwrap_or(rest.len());
        let id_part = &rest[..at_pos];
        let clean = id_part.trim_matches(|c: char| !c.is_ascii_alphanumeric());
        if clean.is_empty() {
            None
        } else {
            Some(clean.to_string())
        }
    }

    pub fn extract_xml_field(xml: &str, tag: &str) -> Option<String> {
        let open_tag = format!("<{}>", tag);
        let close_tag = format!("</{}>", tag);
        let start = xml.find(&open_tag)? + open_tag.len();
        let end = xml[start..].find(&close_tag)? + start;
        Some(xml[start..end].trim().to_string())
    }
}

// ============================================================================
// GB28181 SIP Server (UDP listener)
// ============================================================================

pub struct Gb28181SipServer {
    socket: Arc<UdpSocket>,
    registry: Gb28181DeviceRegistry,
}

impl Gb28181SipServer {
    pub async fn bind(addr: &str, registry: Gb28181DeviceRegistry) -> Result<Self, std::io::Error> {
        let socket = UdpSocket::bind(addr).await?;
        Ok(Self {
            socket: Arc::new(socket),
            registry,
        })
    }

    pub async fn run(&self) -> Result<(), std::io::Error> {
        let mut buf = [0u8; 4096];
        loop {
            let (len, remote_addr) = self.socket.recv_from(&mut buf).await?;
            if let Ok(raw_str) = std::str::from_utf8(&buf[..len]) {
                if let Ok(SipPacket::Request(req)) = SipPacket::parse(raw_str) {
                    let remote_ip = remote_addr.ip().to_string();
                    let remote_port = remote_addr.port();
                    if let Some(resp) = self
                        .registry
                        .handle_sip_request(&req, &remote_ip, remote_port)
                        .await
                    {
                        let _ = self.socket.send_to(resp.as_bytes(), remote_addr).await;
                    }
                }
            }
        }
    }
}

// ============================================================================
// GB/T 28181 SIP INVITE (实时点播), ACK & BYE (停播) 会话控制
// ============================================================================

#[derive(Debug, Clone)]
pub struct InviteRequest {
    pub device_id: String,
    pub device_ip: String,
    pub device_port: u16,
    pub platform_id: String,
    pub platform_ip: String,
    pub platform_port: u16,
    pub media_ip: String,
    pub media_port: u16,
    pub ssrc: String,
    pub call_id: String,
    pub cseq: u32,
    pub from_tag: String,
}

impl InviteRequest {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        device_id: &str,
        device_ip: &str,
        device_port: u16,
        platform_id: &str,
        platform_ip: &str,
        platform_port: u16,
        media_ip: &str,
        media_port: u16,
        ssrc: &str,
    ) -> Self {
        let call_id = format!("call-{}", uuid::Uuid::new_v4());
        let from_tag = format!("tag-{}", (chrono::Utc::now().timestamp_micros() % 1000000));
        Self {
            device_id: device_id.to_string(),
            device_ip: device_ip.to_string(),
            device_port,
            platform_id: platform_id.to_string(),
            platform_ip: platform_ip.to_string(),
            platform_port,
            media_ip: media_ip.to_string(),
            media_port,
            ssrc: ssrc.to_string(),
            call_id,
            cseq: 1,
            from_tag,
        }
    }

    /// 构造符合 GB/T 28181-2016 规范的 SIP INVITE 报文（包含 SDP 媒体协商体）
    pub fn build_invite(&self) -> String {
        let sdp = format!(
            "v=0\r\n\
            o={} 0 0 IN IP4 {}\r\n\
            s=Play\r\n\
            c=IN IP4 {}\r\n\
            t=0 0\r\n\
            m=video {} RTP/AVP 96\r\n\
            a=recvonly\r\n\
            a=rtpmap:96 PS/90000\r\n\
            y={}\r\n",
            self.platform_id,
            self.platform_ip,
            self.media_ip,
            self.media_port,
            self.ssrc
        );

        let branch = format!("z9hG4bK-inv-{}", uuid::Uuid::new_v4());

        format!(
            "INVITE sip:{}@{}:{} SIP/2.0\r\n\
            Via: SIP/2.0/UDP {}:{};rport;branch={}\r\n\
            From: <sip:{}@{}:{}>;tag={}\r\n\
            To: <sip:{}@{}:{}>\r\n\
            Call-ID: {}\r\n\
            CSeq: {} INVITE\r\n\
            Contact: <sip:{}@{}:{}>\r\n\
            Content-Type: APPLICATION/SDP\r\n\
            Max-Forwards: 70\r\n\
            Subject: {}:0,{}:0\r\n\
            Content-Length: {}\r\n\r\n{}",
            self.device_id,
            self.device_ip,
            self.device_port,
            self.platform_ip,
            self.platform_port,
            branch,
            self.platform_id,
            self.platform_ip,
            self.platform_port,
            self.from_tag,
            self.device_id,
            self.device_ip,
            self.device_port,
            self.call_id,
            self.cseq,
            self.platform_id,
            self.platform_ip,
            self.platform_port,
            self.device_id,
            self.platform_id,
            sdp.len(),
            sdp
        )
    }

    /// 收到 200 OK 后，发送 ACK 确认媒体建立
    pub fn build_ack(&self, to_tag: Option<&str>) -> String {
        let branch = format!("z9hG4bK-ack-{}", uuid::Uuid::new_v4());
        let to_header = if let Some(t) = to_tag {
            format!("<sip:{}@{}:{}>;tag={}", self.device_id, self.device_ip, self.device_port, t)
        } else {
            format!("<sip:{}@{}:{}>", self.device_id, self.device_ip, self.device_port)
        };

        format!(
            "ACK sip:{}@{}:{} SIP/2.0\r\n\
            Via: SIP/2.0/UDP {}:{};rport;branch={}\r\n\
            From: <sip:{}@{}:{}>;tag={}\r\n\
            To: {}\r\n\
            Call-ID: {}\r\n\
            CSeq: {} ACK\r\n\
            Max-Forwards: 70\r\n\
            Content-Length: 0\r\n\r\n",
            self.device_id,
            self.device_ip,
            self.device_port,
            self.platform_ip,
            self.platform_port,
            branch,
            self.platform_id,
            self.platform_ip,
            self.platform_port,
            self.from_tag,
            to_header,
            self.call_id,
            self.cseq
        )
    }

    /// 停止点播发送 BYE 报文
    pub fn build_bye(&mut self, to_tag: Option<&str>) -> String {
        self.cseq += 1;
        let branch = format!("z9hG4bK-bye-{}", uuid::Uuid::new_v4());
        let to_header = if let Some(t) = to_tag {
            format!("<sip:{}@{}:{}>;tag={}", self.device_id, self.device_ip, self.device_port, t)
        } else {
            format!("<sip:{}@{}:{}>", self.device_id, self.device_ip, self.device_port)
        };

        format!(
            "BYE sip:{}@{}:{} SIP/2.0\r\n\
            Via: SIP/2.0/UDP {}:{};rport;branch={}\r\n\
            From: <sip:{}@{}:{}>;tag={}\r\n\
            To: {}\r\n\
            Call-ID: {}\r\n\
            CSeq: {} BYE\r\n\
            Max-Forwards: 70\r\n\
            Content-Length: 0\r\n\r\n",
            self.device_id,
            self.device_ip,
            self.device_port,
            self.platform_ip,
            self.platform_port,
            branch,
            self.platform_id,
            self.platform_ip,
            self.platform_port,
            self.from_tag,
            to_header,
            self.call_id,
            self.cseq
        )
    }
}

// ============================================================================
// GB/T 28181 设备目录查询 (Catalog Query)
// ============================================================================

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogItem {
    pub device_id: String,
    pub name: String,
    pub manufacturer: String,
    pub model: String,
    pub status: String,
}

pub struct CatalogQuery;

impl CatalogQuery {
    /// 构造设备目录查询 MESSAGE 报文
    pub fn build_query(
        device_id: &str,
        device_ip: &str,
        device_port: u16,
        platform_id: &str,
        platform_ip: &str,
        platform_port: u16,
        sn: u32,
    ) -> String {
        let xml = format!(
            "<?xml version=\"1.0\" encoding=\"GB2312\"?>\r\n\
            <Query>\r\n\
            <CmdType>Catalog</CmdType>\r\n\
            <SN>{}</SN>\r\n\
            <DeviceID>{}</DeviceID>\r\n\
            </Query>\r\n",
            sn, device_id
        );

        let branch = format!("z9hG4bK-cat-{}", uuid::Uuid::new_v4());
        let call_id = format!("call-cat-{}", uuid::Uuid::new_v4());

        format!(
            "MESSAGE sip:{}@{}:{} SIP/2.0\r\n\
            Via: SIP/2.0/UDP {}:{};rport;branch={}\r\n\
            From: <sip:{}@{}:{}>;tag=cat-{}\r\n\
            To: <sip:{}@{}:{}>\r\n\
            Call-ID: {}\r\n\
            CSeq: 1 MESSAGE\r\n\
            Content-Type: Application/MANSCDP+xml\r\n\
            Max-Forwards: 70\r\n\
            Content-Length: {}\r\n\r\n{}",
            device_id,
            device_ip,
            device_port,
            platform_ip,
            platform_port,
            branch,
            platform_id,
            platform_ip,
            platform_port,
            sn,
            device_id,
            device_ip,
            device_port,
            call_id,
            xml.len(),
            xml
        )
    }

    /// 解析摄像机响应的设备目录 XML 报文
    pub fn parse_catalog_response(xml: &str) -> Vec<CatalogItem> {
        let mut items = Vec::new();
        let mut cur_slice = xml;

        while let Some(item_start) = cur_slice.find("<Item>") {
            let after_start = &cur_slice[item_start + 6..];
            let item_end = match after_start.find("</Item>") {
                Some(idx) => idx,
                None => break,
            };

            let item_content = &after_start[..item_end];
            let device_id = extract_xml_val(item_content, "DeviceID").unwrap_or_default();
            let name = extract_xml_val(item_content, "Name").unwrap_or_default();
            let manufacturer = extract_xml_val(item_content, "Manufacturer").unwrap_or_default();
            let model = extract_xml_val(item_content, "Model").unwrap_or_default();
            let status = extract_xml_val(item_content, "Status").unwrap_or_else(|| "ON".into());

            if !device_id.is_empty() {
                items.push(CatalogItem {
                    device_id,
                    name,
                    manufacturer,
                    model,
                    status,
                });
            }

            cur_slice = &after_start[item_end + 7..];
        }

        items
    }
}

fn extract_xml_val(xml: &str, tag: &str) -> Option<String> {
    let open_tag = format!("<{}>", tag);
    let close_tag = format!("</{}>", tag);
    let start = xml.find(&open_tag)? + open_tag.len();
    let end = xml[start..].find(&close_tag)? + start;
    Some(xml[start..end].trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rfc1321_md5_vectors() {
        assert_eq!(md5_hex(b""), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(md5_hex(b"a"), "0cc175b9c0f1b6a831c399e269772661");
        assert_eq!(md5_hex(b"abc"), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(
            md5_hex(b"message digest"),
            "f96b697d7cb7938d525a2f31aaf161d0"
        );
        assert_eq!(
            md5_hex(b"abcdefghijklmnopqrstuvwxyz"),
            "c3fcd3d76192e4007dfb496cca67e13b"
        );
    }

    #[test]
    fn test_digest_auth_verification() {
        let ha1 = md5_hex(b"34020000002000000001:3402000000:admin123");
        let ha2 = md5_hex(b"REGISTER:sip:34020000002000000001@192.168.1.10:5060");
        let nonce = "12345678abcdef";
        let expected_response = md5_hex(format!("{}:{}:{}", ha1, nonce, ha2).as_bytes());

        let auth_header = format!(
            "Digest username=\"34020000002000000001\", realm=\"3402000000\", nonce=\"{}\", uri=\"sip:34020000002000000001@192.168.1.10:5060\", response=\"{}\", algorithm=MD5",
            nonce, expected_response
        );

        let params = DigestAuthParams::parse(&auth_header).expect("Must parse digest header");
        assert!(params.verify("REGISTER", "admin123"));
        assert!(!params.verify("REGISTER", "wrongpass"));
    }

    #[tokio::test]
    async fn test_device_registry_register_and_keepalive() {
        let registry = Gb28181DeviceRegistry::new(
            "34020000002000000001",
            "192.168.1.10",
            5060,
            "3402000000",
            "admin123",
        );

        let unauth_req_str = "REGISTER sip:34020000002000000001@192.168.1.10:5060 SIP/2.0\r\n\
            Via: SIP/2.0/UDP 192.168.1.50:5060;branch=z9hG4bK111\r\n\
            From: <sip:34020000001320000001@3402000000>;tag=123\r\n\
            To: <sip:34020000002000000001@3402000000>\r\n\
            Call-ID: call-test-1\r\n\
            CSeq: 1 REGISTER\r\n\
            Content-Length: 0\r\n\r\n";

        let parsed = SipPacket::parse(unauth_req_str).unwrap();
        if let SipPacket::Request(req) = parsed {
            let resp = registry
                .handle_sip_request(&req, "192.168.1.50", 5060)
                .await;
            assert!(resp.is_some());
            let resp_str = resp.unwrap();
            assert!(resp_str.starts_with("SIP/2.0 401 Unauthorized"));
            assert!(resp_str.contains("WWW-Authenticate: Digest"));
        } else {
            panic!("Expected request");
        }

        let keepalive_xml = "<?xml version=\"1.0\" encoding=\"GB2312\"?>\r\n\
            <Notify>\r\n\
            <CmdType>Keepalive</CmdType>\r\n\
            <SN>123</SN>\r\n\
            <DeviceID>34020000001320000001</DeviceID>\r\n\
            <Status>OK</Status>\r\n\
            </Notify>";

        let msg_req_str = format!(
            "MESSAGE sip:34020000002000000001@192.168.1.10:5060 SIP/2.0\r\n\
            Via: SIP/2.0/UDP 192.168.1.50:5060;branch=z9hG4bK222\r\n\
            From: <sip:34020000001320000001@3402000000>;tag=456\r\n\
            To: <sip:34020000002000000001@3402000000>\r\n\
            Call-ID: call-test-msg\r\n\
            CSeq: 2 MESSAGE\r\n\
            Content-Type: Application/MANSCDP+xml\r\n\
            Content-Length: {}\r\n\r\n{}",
            keepalive_xml.len(),
            keepalive_xml
        );

        let parsed_msg = SipPacket::parse(&msg_req_str).unwrap();
        if let SipPacket::Request(req) = parsed_msg {
            let resp = registry
                .handle_sip_request(&req, "192.168.1.50", 5060)
                .await;
            assert!(resp.is_some());
            let resp_str = resp.unwrap();
            assert!(resp_str.starts_with("SIP/2.0 200 OK"));
        }
    }

    #[test]
    fn test_invite_request_build() {
        let invite = InviteRequest::new(
            "34020000001320000001",
            "192.168.1.64",
            5060,
            "34020000002000000001",
            "192.168.1.10",
            5060,
            "192.168.1.10",
            15000,
            "0100000001",
        );

        let msg = invite.build_invite();
        assert!(msg.starts_with("INVITE sip:34020000001320000001@192.168.1.64:5060 SIP/2.0"));
        assert!(msg.contains("Content-Type: APPLICATION/SDP"));
        assert!(msg.contains("s=Play"));
        assert!(msg.contains("m=video 15000 RTP/AVP 96"));
        assert!(msg.contains("a=rtpmap:96 PS/90000"));
        assert!(msg.contains("y=0100000001"));
    }

    #[test]
    fn test_invite_ack_and_bye() {
        let mut invite = InviteRequest::new(
            "34020000001320000001",
            "192.168.1.64",
            5060,
            "34020000002000000001",
            "192.168.1.10",
            5060,
            "192.168.1.10",
            15000,
            "0100000001",
        );

        let ack = invite.build_ack(Some("remote-tag-999"));
        assert!(ack.starts_with("ACK sip:34020000001320000001@192.168.1.64:5060 SIP/2.0"));
        assert!(ack.contains(";tag=remote-tag-999"));

        let bye = invite.build_bye(Some("remote-tag-999"));
        assert!(bye.starts_with("BYE sip:34020000001320000001@192.168.1.64:5060 SIP/2.0"));
        assert!(bye.contains("CSeq: 2 BYE"));
    }

    #[test]
    fn test_catalog_query_and_response() {
        let query = CatalogQuery::build_query(
            "34020000001320000001",
            "192.168.1.64",
            5060,
            "34020000002000000001",
            "192.168.1.10",
            5060,
            1,
        );
        assert!(query.starts_with("MESSAGE sip:34020000001320000001@192.168.1.64:5060 SIP/2.0"));
        assert!(query.contains("<CmdType>Catalog</CmdType>"));

        let catalog_xml = "<?xml version=\"1.0\" encoding=\"GB2312\"?>\r\n\
            <Response>\r\n\
            <CmdType>Catalog</CmdType>\r\n\
            <SN>1</SN>\r\n\
            <DeviceID>34020000001320000001</DeviceID>\r\n\
            <SumNum>1</SumNum>\r\n\
            <DeviceList Num=\"1\">\r\n\
            <Item>\r\n\
            <DeviceID>34020000001310000001</DeviceID>\r\n\
            <Name>Camera-Ch1</Name>\r\n\
            <Manufacturer>Hikvision</Manufacturer>\r\n\
            <Model>DS-2CD2T46WDV3-I</Model>\r\n\
            <Status>ON</Status>\r\n\
            </Item>\r\n\
            </DeviceList>\r\n\
            </Response>";

        let items = CatalogQuery::parse_catalog_response(catalog_xml);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].device_id, "34020000001310000001");
        assert_eq!(items[0].name, "Camera-Ch1");
        assert_eq!(items[0].manufacturer, "Hikvision");
        assert_eq!(items[0].model, "DS-2CD2T46WDV3-I");
        assert_eq!(items[0].status, "ON");
    }
}
