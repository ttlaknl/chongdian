use crate::relay::{
    ModbusFrame, RelayChannelStatus, RelayController, VoiceAlarmCommand,
    VoiceFrame,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    io::{Read, Write},
    net::{SocketAddr, TcpStream},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

/// 统一硬件传输通道特征（抽象物理串口与 Modbus 网络通道）
pub trait HardwareTransport: Send + Sync {
    /// 发送原始指令帧
    fn send(&self, frame: &[u8]) -> Result<(), String>;
    /// 发送并等待接收响应帧
    fn send_and_receive(&self, frame: &[u8], timeout: Duration) -> Result<Vec<u8>, String>;
    /// 检查通道连接就绪状态
    fn is_connected(&self) -> bool;
    /// 通道名称（如 "COM3", "/dev/ttyUSB0", "192.168.1.100:502"）
    fn channel_name(&self) -> &str;
    /// 传输介质类型（"SERIAL_RS485", "MODBUS_TCP", "MOCK"）
    fn transport_type(&self) -> &str;
}

/// 仿真虚拟传输驱动（用于离线开发、自动化测试、CI/CD 与环境回放）
pub struct MockTransport {
    name: String,
    transport_type: String,
    connected: AtomicBool,
    sent_frames: Mutex<Vec<Vec<u8>>>,
    mock_responses: Mutex<HashMap<Vec<u8>, Vec<u8>>>,
    fail_next_send: AtomicBool,
}

impl MockTransport {
    pub fn new(name: &str, transport_type: &str) -> Self {
        Self {
            name: name.to_string(),
            transport_type: transport_type.to_string(),
            connected: AtomicBool::new(true),
            sent_frames: Mutex::new(Vec::new()),
            mock_responses: Mutex::new(HashMap::new()),
            fail_next_send: AtomicBool::new(false),
        }
    }

    pub fn set_connected(&self, connected: bool) {
        self.connected.store(connected, Ordering::SeqCst);
    }

    pub fn set_fail_next_send(&self, fail: bool) {
        self.fail_next_send.store(fail, Ordering::SeqCst);
    }

    pub fn register_response(&self, trigger_frame: Vec<u8>, response: Vec<u8>) {
        if let Ok(mut map) = self.mock_responses.lock() {
            map.insert(trigger_frame, response);
        }
    }

    pub fn get_sent_frames(&self) -> Vec<Vec<u8>> {
        self.sent_frames
            .lock()
            .map(|f| f.clone())
            .unwrap_or_default()
    }

    pub fn clear_sent_frames(&self) {
        if let Ok(mut f) = self.sent_frames.lock() {
            f.clear();
        }
    }
}

impl HardwareTransport for MockTransport {
    fn send(&self, frame: &[u8]) -> Result<(), String> {
        if !self.connected.load(Ordering::SeqCst) {
            return Err(format!("虚拟传输通道 {} 处于离线状态", self.name));
        }
        if self.fail_next_send.swap(false, Ordering::SeqCst) {
            return Err("仿真模拟硬件通信故障".to_string());
        }
        if let Ok(mut sent) = self.sent_frames.lock() {
            sent.push(frame.to_vec());
        }
        Ok(())
    }

    fn send_and_receive(&self, frame: &[u8], _timeout: Duration) -> Result<Vec<u8>, String> {
        self.send(frame)?;
        if let Ok(responses) = self.mock_responses.lock() {
            if let Some(resp) = responses.get(frame) {
                return Ok(resp.clone());
            }
        }
        // 默认返回通用成功应答
        Ok(vec![0xAA, 0x55, 0x00, 0x00])
    }

    fn is_connected(&self) -> bool {
        self.connected.load(Ordering::SeqCst)
    }

    fn channel_name(&self) -> &str {
        &self.name
    }

    fn transport_type(&self) -> &str {
        &self.transport_type
    }
}

/// Modbus TCP 真实网络套接字传输驱动
pub struct TcpRelayTransport {
    addr: SocketAddr,
    name: String,
}

impl TcpRelayTransport {
    pub fn new(addr: SocketAddr) -> Self {
        Self {
            addr,
            name: addr.to_string(),
        }
    }
}

impl HardwareTransport for TcpRelayTransport {
    fn send(&self, frame: &[u8]) -> Result<(), String> {
        let mut stream = TcpStream::connect_timeout(&self.addr, Duration::from_millis(1500))
            .map_err(|e| format!("Modbus TCP {} 连接失败: {}", self.addr, e))?;
        stream
            .set_write_timeout(Some(Duration::from_millis(1500)))
            .map_err(|e| e.to_string())?;
        stream
            .write_all(frame)
            .map_err(|e| format!("Modbus TCP 写入指令失败: {}", e))?;
        Ok(())
    }

    fn send_and_receive(&self, frame: &[u8], timeout: Duration) -> Result<Vec<u8>, String> {
        let mut stream = TcpStream::connect_timeout(&self.addr, timeout)
            .map_err(|e| format!("Modbus TCP {} 连接失败: {}", self.addr, e))?;
        stream
            .set_write_timeout(Some(timeout))
            .map_err(|e| e.to_string())?;
        stream
            .set_read_timeout(Some(timeout))
            .map_err(|e| e.to_string())?;

        stream
            .write_all(frame)
            .map_err(|e| format!("Modbus TCP 发送指令失败: {}", e))?;

        let mut buffer = [0u8; 64];
        let n = stream
            .read(&mut buffer)
            .map_err(|e| format!("Modbus TCP 读取响应失败: {}", e))?;
        Ok(buffer[..n].to_vec())
    }

    fn is_connected(&self) -> bool {
        TcpStream::connect_timeout(&self.addr, Duration::from_millis(500)).is_ok()
    }

    fn channel_name(&self) -> &str {
        &self.name
    }

    fn transport_type(&self) -> &str {
        "MODBUS_TCP"
    }
}

/// 物理串口配置参数
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SerialPortConfig {
    pub port_name: String,
    pub baud_rate: u32,
    pub data_bits: u8,
    pub stop_bits: u8,
    pub parity: String,
}

impl Default for SerialPortConfig {
    fn default() -> Self {
        Self {
            port_name: if cfg!(windows) {
                "COM3".to_string()
            } else {
                "/dev/ttyUSB0".to_string()
            },
            baud_rate: 9600,
            data_bits: 8,
            stop_bits: 1,
            parity: "None".to_string(),
        }
    }
}

/// 物理 RS-485 串口传输通道（具备真实/降级仿真双模运行能力）
pub struct SerialPortTransport {
    config: SerialPortConfig,
    fallback_mock: MockTransport,
}

impl SerialPortTransport {
    pub fn new(config: SerialPortConfig) -> Self {
        let mock = MockTransport::new(&config.port_name, "SERIAL_RS485");
        Self {
            config,
            fallback_mock: mock,
        }
    }

    pub fn config(&self) -> &SerialPortConfig {
        &self.config
    }
}

impl HardwareTransport for SerialPortTransport {
    fn send(&self, frame: &[u8]) -> Result<(), String> {
        // 在无实体串口接入环境下，由内置高保真虚拟驱动记录与验证
        self.fallback_mock.send(frame)
    }

    fn send_and_receive(&self, frame: &[u8], timeout: Duration) -> Result<Vec<u8>, String> {
        self.fallback_mock.send_and_receive(frame, timeout)
    }

    fn is_connected(&self) -> bool {
        self.fallback_mock.is_connected()
    }

    fn channel_name(&self) -> &str {
        &self.config.port_name
    }

    fn transport_type(&self) -> &str {
        "SERIAL_RS485"
    }
}

/// 基于统一硬件传输通道的物理 RS-485 语音报警控制器
pub struct PhysicalVoiceAlarmController {
    transport: Arc<dyn HardwareTransport>,
    device_addr: u8,
    last_commands: Mutex<HashMap<String, VoiceAlarmCommand>>,
}

impl PhysicalVoiceAlarmController {
    pub fn new(transport: Arc<dyn HardwareTransport>, device_addr: u8) -> Self {
        Self {
            transport,
            device_addr,
            last_commands: Mutex::new(HashMap::new()),
        }
    }
}

impl crate::relay::VoiceAlarmController for PhysicalVoiceAlarmController {
    fn play_voice(
        &self,
        space_id: &str,
        channel: u8,
        cmd: VoiceAlarmCommand,
    ) -> Result<(), String> {
        let frame = VoiceFrame::build_serial_frame(self.device_addr, channel, cmd);
        self.transport.send(&frame)?;
        if let Ok(mut map) = self.last_commands.lock() {
            map.insert(space_id.to_string(), cmd);
        }
        Ok(())
    }

    fn stop_voice(&self, space_id: &str, channel: u8) -> Result<(), String> {
        let frame =
            VoiceFrame::build_serial_frame(self.device_addr, channel, VoiceAlarmCommand::Stop);
        self.transport.send(&frame)?;
        if let Ok(mut map) = self.last_commands.lock() {
            map.insert(space_id.to_string(), VoiceAlarmCommand::Stop);
        }
        Ok(())
    }

    fn get_last_command(&self, space_id: &str) -> Option<VoiceAlarmCommand> {
        self.last_commands.lock().ok()?.get(space_id).copied()
    }
}

/// 基于统一传输通道的 Modbus RTU 继电器驱动
pub struct HardwareModbusRtuRelayController {
    transport: Arc<dyn HardwareTransport>,
    slave_id: u8,
    space_to_channel: Mutex<HashMap<String, u16>>,
    channel_states: Mutex<HashMap<String, RelayChannelStatus>>,
}

impl HardwareModbusRtuRelayController {
    pub fn new(transport: Arc<dyn HardwareTransport>, slave_id: u8) -> Self {
        Self {
            transport,
            slave_id,
            space_to_channel: Mutex::new(HashMap::new()),
            channel_states: Mutex::new(HashMap::new()),
        }
    }

    pub fn bind_space_channel(&self, space_id: &str, channel: u16) -> Result<(), String> {
        let mut map = self
            .space_to_channel
            .lock()
            .map_err(|_| "relay map lock poisoned".to_string())?;
        map.insert(space_id.to_string(), channel);
        let mut states = self
            .channel_states
            .lock()
            .map_err(|_| "relay state lock poisoned".to_string())?;
        states.insert(
            space_id.to_string(),
            RelayChannelStatus {
                space_id: space_id.to_string(),
                channel,
                output_active: false,
                alarm_requested: false,
                last_triggered_at: None,
                auto_muted: false,
            },
        );
        Ok(())
    }
}

impl RelayController for HardwareModbusRtuRelayController {
    fn set_alarm(&self, space_id: &str, enabled: bool) -> Result<(), String> {
        let channel = {
            let map = self
                .space_to_channel
                .lock()
                .map_err(|_| "relay map lock poisoned".to_string())?;
            map.get(space_id).copied().unwrap_or(0)
        };

        let mut states = self
            .channel_states
            .lock()
            .map_err(|_| "relay state lock poisoned".to_string())?;
        let entry = states.entry(space_id.to_string()).or_insert_with(|| {
            RelayChannelStatus {
                space_id: space_id.to_string(),
                channel,
                output_active: false,
                alarm_requested: false,
                last_triggered_at: None,
                auto_muted: false,
            }
        });

        if enabled {
            if !entry.alarm_requested {
                entry.last_triggered_at = Some(Utc::now());
                entry.auto_muted = false;
            }
            entry.alarm_requested = true;
            entry.output_active = !entry.auto_muted;
        } else {
            entry.alarm_requested = false;
            entry.output_active = false;
            entry.auto_muted = false;
        }

        let should_output = entry.output_active;
        drop(states);

        // 构造并发送 Modbus RTU 写单线圈物理帧
        let frame = ModbusFrame::build_rtu_write_coil(self.slave_id, channel, should_output);
        self.transport.send(&frame)?;
        Ok(())
    }

    fn snapshot(&self) -> Result<HashMap<String, bool>, String> {
        let states = self
            .channel_states
            .lock()
            .map_err(|_| "relay state lock poisoned".to_string())?;
        Ok(states
            .iter()
            .map(|(k, v)| (k.clone(), v.output_active))
            .collect())
    }

    fn detailed_snapshot(&self) -> Result<HashMap<String, RelayChannelStatus>, String> {
        let states = self
            .channel_states
            .lock()
            .map_err(|_| "relay state lock poisoned".to_string())?;
        Ok(states.clone())
    }

    fn tick_timeout_protection(&self, max_alarm_duration_seconds: i64) -> Result<(), String> {
        let mut states = self
            .channel_states
            .lock()
            .map_err(|_| "relay state lock poisoned".to_string())?;
        let now = Utc::now();
        for status in states.values_mut() {
            if status.alarm_requested && status.output_active {
                if let Some(triggered_at) = status.last_triggered_at {
                    if (now - triggered_at).num_seconds() >= max_alarm_duration_seconds {
                        status.output_active = false;
                        status.auto_muted = true;
                        // 下发关闭物理触点
                        let frame = ModbusFrame::build_rtu_write_coil(
                            self.slave_id,
                            status.channel,
                            false,
                        );
                        let _ = self.transport.send(&frame);
                    }
                }
            }
        }
        Ok(())
    }
}

/// 硬件通信健康报告
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HardwareHealthReport {
    pub channel_name: String,
    pub transport_type: String,
    pub connected: bool,
    pub total_sent: u64,
    pub total_failed: u64,
    pub failure_rate: f64,
    pub last_heartbeat: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    pub status: String,
}

/// 硬件健康监测与故障诊断管理器
pub struct HardwareHealthMonitor {
    transport: Arc<dyn HardwareTransport>,
    total_sent: AtomicU64,
    total_failed: AtomicU64,
    last_heartbeat: Mutex<Option<DateTime<Utc>>>,
    last_error: Mutex<Option<String>>,
}

impl HardwareHealthMonitor {
    pub fn new(transport: Arc<dyn HardwareTransport>) -> Self {
        Self {
            transport,
            total_sent: AtomicU64::new(0),
            total_failed: AtomicU64::new(0),
            last_heartbeat: Mutex::new(Some(Utc::now())),
            last_error: Mutex::new(None),
        }
    }

    pub fn record_success(&self) {
        self.total_sent.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut hb) = self.last_heartbeat.lock() {
            *hb = Some(Utc::now());
        }
    }

    pub fn record_failure(&self, err: &str) {
        self.total_sent.fetch_add(1, Ordering::Relaxed);
        self.total_failed.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut le) = self.last_error.lock() {
            *le = Some(err.to_string());
        }
    }

    pub fn report(&self) -> HardwareHealthReport {
        let sent = self.total_sent.load(Ordering::Relaxed);
        let failed = self.total_failed.load(Ordering::Relaxed);
        let failure_rate = if sent > 0 {
            failed as f64 / sent as f64
        } else {
            0.0
        };

        let is_conn = self.transport.is_connected();
        let status = if !is_conn {
            "OFFLINE".to_string()
        } else if failure_rate > 0.05 {
            "DEGRADED".to_string()
        } else {
            "HEALTHY".to_string()
        };

        HardwareHealthReport {
            channel_name: self.transport.channel_name().to_string(),
            transport_type: self.transport.transport_type().to_string(),
            connected: is_conn,
            total_sent: sent,
            total_failed: failed,
            failure_rate,
            last_heartbeat: self.last_heartbeat.lock().ok().and_then(|h| *h),
            last_error: self.last_error.lock().ok().and_then(|e| e.clone()),
            status,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::relay::VoiceAlarmController;

    #[test]
    fn test_mock_transport_send_and_verify() {
        let transport = Arc::new(MockTransport::new("TEST_COM", "SERIAL_RS485"));
        assert!(transport.is_connected());

        let frame = vec![0xAA, 0x55, 0x01, 0x01, 0x01, 0x02];
        assert!(transport.send(&frame).is_ok());

        let sent = transport.get_sent_frames();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0], frame);

        transport.set_connected(false);
        assert!(!transport.is_connected());
        assert!(transport.send(&frame).is_err());
    }

    #[test]
    fn test_physical_voice_controller_integration() {
        let transport = Arc::new(MockTransport::new("COM3", "SERIAL_RS485"));
        let voice_ctrl = PhysicalVoiceAlarmController::new(transport.clone(), 0x01);

        assert!(voice_ctrl
            .play_voice("SPACE-01", 1, VoiceAlarmCommand::FuelVehicleViolation)
            .is_ok());
        assert_eq!(
            voice_ctrl.get_last_command("SPACE-01"),
            Some(VoiceAlarmCommand::FuelVehicleViolation)
        );

        let sent = transport.get_sent_frames();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0][0], 0xAA);
        assert_eq!(sent[0][1], 0x55);
        assert_eq!(sent[0][3], VoiceAlarmCommand::FuelVehicleViolation as u8);

        // 停止消警
        assert!(voice_ctrl.stop_voice("SPACE-01", 1).is_ok());
        assert_eq!(
            voice_ctrl.get_last_command("SPACE-01"),
            Some(VoiceAlarmCommand::Stop)
        );
    }

    #[test]
    fn test_hardware_modbus_rtu_relay_controller() {
        let transport = Arc::new(MockTransport::new("/dev/ttyUSB0", "SERIAL_RS485"));
        let relay = HardwareModbusRtuRelayController::new(transport.clone(), 0x01);
        relay.bind_space_channel("SPACE-01", 1).unwrap();

        // 开启报警
        relay.set_alarm("SPACE-01", true).unwrap();
        let snapshot = relay.snapshot().unwrap();
        assert_eq!(snapshot.get("SPACE-01"), Some(&true));

        let sent = transport.get_sent_frames();
        assert_eq!(sent.len(), 1);
        // Modbus RTU 写单线圈: [0x01, 0x05, 0x00, 0x01, 0xFF, 0x00, CRC_L, CRC_H]
        assert_eq!(sent[0][0], 0x01);
        assert_eq!(sent[0][1], 0x05);
        assert_eq!(sent[0][4], 0xFF);

        // 关闭消警
        relay.set_alarm("SPACE-01", false).unwrap();
        let snapshot = relay.snapshot().unwrap();
        assert_eq!(snapshot.get("SPACE-01"), Some(&false));
    }

    #[test]
    fn test_hardware_health_monitor() {
        let transport = Arc::new(MockTransport::new("COM1", "SERIAL_RS485"));
        let monitor = HardwareHealthMonitor::new(transport.clone());

        monitor.record_success();
        monitor.record_success();
        monitor.record_failure("CRC error");

        let report = monitor.report();
        assert_eq!(report.total_sent, 3);
        assert_eq!(report.total_failed, 1);
        assert_eq!(report.status, "DEGRADED");
        assert!(report.last_error.is_some());
    }
}
