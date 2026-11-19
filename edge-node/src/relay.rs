use crate::DeviceBinding;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    io::{Read, Write},
    net::{SocketAddr, TcpStream},
    sync::{
        atomic::{AtomicU16, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

/// Modbus CRC-16 校验码计算（高位在前/低位在后，多项式 0xA001）
pub fn calculate_crc16(data: &[u8]) -> u16 {
    let mut crc: u16 = 0xFFFF;
    for &byte in data {
        crc ^= byte as u16;
        for _ in 0..8 {
            if (crc & 0x0001) != 0 {
                crc = (crc >> 1) ^ 0xA001;
            } else {
                crc >>= 1;
            }
        }
    }
    crc
}

/// Modbus 协议报文构造与解析器
#[derive(Debug, Clone)]
pub struct ModbusFrame;

impl ModbusFrame {
    /// 构造 Modbus RTU 写单个线圈指令 (0x05)
    /// 格式: [SlaveID 1B, 0x05, AddrHi, AddrLo, ValueHi, ValueLo, CrcLo, CrcHi]
    pub fn build_rtu_write_coil(slave_id: u8, channel: u16, state: bool) -> Vec<u8> {
        let mut frame = Vec::with_capacity(8);
        frame.push(slave_id);
        frame.push(0x05);
        frame.push((channel >> 8) as u8);
        frame.push((channel & 0xFF) as u8);
        if state {
            frame.push(0xFF);
            frame.push(0x00);
        } else {
            frame.push(0x00);
            frame.push(0x00);
        }
        let crc = calculate_crc16(&frame);
        frame.push((crc & 0xFF) as u8); // CRC 低位在前
        frame.push((crc >> 8) as u8); // CRC 高位在后
        frame
    }

    /// 构造 Modbus TCP 写单个线圈指令 (0x05)
    /// 格式: [TxIDHi, TxIDLo, 0x00, 0x00, LengthHi, LengthLo, UnitID, 0x05, AddrHi, AddrLo, ValHi, ValLo]
    pub fn build_tcp_write_coil(tx_id: u16, unit_id: u8, channel: u16, state: bool) -> Vec<u8> {
        let mut frame = Vec::with_capacity(12);
        // MBAP Header
        frame.push((tx_id >> 8) as u8);
        frame.push((tx_id & 0xFF) as u8);
        frame.push(0x00); // 协议标识 0x0000 = Modbus
        frame.push(0x00);
        frame.push(0x00); // 长度 6 字节 (UnitID + PDU)
        frame.push(0x06);
        frame.push(unit_id);
        // PDU
        frame.push(0x05);
        frame.push((channel >> 8) as u8);
        frame.push((channel & 0xFF) as u8);
        if state {
            frame.push(0xFF);
            frame.push(0x00);
        } else {
            frame.push(0x00);
            frame.push(0x00);
        }
        frame
    }

    /// 构造 Modbus RTU 读线圈状态指令 (0x01)
    pub fn build_rtu_read_coils(slave_id: u8, start_channel: u16, count: u16) -> Vec<u8> {
        let mut frame = Vec::with_capacity(8);
        frame.push(slave_id);
        frame.push(0x01);
        frame.push((start_channel >> 8) as u8);
        frame.push((start_channel & 0xFF) as u8);
        frame.push((count >> 8) as u8);
        frame.push((count & 0xFF) as u8);
        let crc = calculate_crc16(&frame);
        frame.push((crc & 0xFF) as u8);
        frame.push((crc >> 8) as u8);
        frame
    }

    /// 构造 Modbus TCP 读线圈状态指令 (0x01)
    pub fn build_tcp_read_coils(
        tx_id: u16,
        unit_id: u8,
        start_channel: u16,
        count: u16,
    ) -> Vec<u8> {
        vec![
            (tx_id >> 8) as u8,
            (tx_id & 0xFF) as u8,
            0x00,
            0x00,
            0x00,
            0x06,
            unit_id,
            0x01,
            (start_channel >> 8) as u8,
            (start_channel & 0xFF) as u8,
            (count >> 8) as u8,
            (count & 0xFF) as u8,
        ]
    }

    /// 校验并解析 Modbus RTU 响应帧
    pub fn parse_rtu_response(frame: &[u8]) -> Result<&[u8], String> {
        if frame.len() < 4 {
            return Err("RTU 响应帧长度不足".into());
        }
        let payload_len = frame.len() - 2;
        let expected_crc = ((frame[frame.len() - 1] as u16) << 8) | (frame[frame.len() - 2] as u16);
        let actual_crc = calculate_crc16(&frame[..payload_len]);
        if expected_crc != actual_crc {
            return Err(format!(
                "Modbus RTU CRC 校验失败: 期望 0x{:04X}, 实际 0x{:04X}",
                expected_crc, actual_crc
            ));
        }
        if frame[1] & 0x80 != 0 {
            return Err(format!("Modbus 从站异常响应: 错误码 0x{:02X}", frame[2]));
        }
        Ok(&frame[..payload_len])
    }

    /// 校验并解析 Modbus TCP 响应帧
    pub fn parse_tcp_response(frame: &[u8], expected_tx_id: u16) -> Result<&[u8], String> {
        if frame.len() < 9 {
            return Err("TCP 响应帧长度不足".into());
        }
        let tx_id = ((frame[0] as u16) << 8) | (frame[1] as u16);
        if tx_id != expected_tx_id {
            return Err(format!(
                "Modbus TCP 事务号不匹配: 期望 {}, 实际 {}",
                expected_tx_id, tx_id
            ));
        }
        if frame[7] & 0x80 != 0 {
            return Err(format!("Modbus TCP 异常响应: 错误码 0x{:02X}", frame[8]));
        }
        Ok(&frame[7..])
    }
}

/// 继电器单通道详细状态
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RelayChannelStatus {
    pub space_id: String,
    pub channel: u16,
    pub output_active: bool,
    pub alarm_requested: bool,
    pub last_triggered_at: Option<DateTime<Utc>>,
    pub auto_muted: bool,
}

/// 继电器控制器通用特征
pub trait RelayController: Send + Sync {
    /// 原子替换车位到继电器通道的绑定关系。
    fn apply_bindings(&self, bindings: &[DeviceBinding]) -> Result<(), String>;
    /// 设置对应车位的报警状态（开/关）
    fn set_alarm(&self, space_id: &str, enabled: bool) -> Result<(), String>;
    /// 获取当前所有车位的简化报警快照
    fn snapshot(&self) -> Result<HashMap<String, bool>, String>;
    /// 获取详细的通道与声光保护状态
    fn detailed_snapshot(&self) -> Result<HashMap<String, RelayChannelStatus>, String>;
    /// 定期自检消警超时保护（如超过 300 秒自动关闭物理继电器，防止长时间扰民）
    fn tick_timeout_protection(&self, max_alarm_duration_seconds: i64) -> Result<(), String>;
}

/// 内存继电器控制器（开发、模拟与自动化测试）
#[derive(Default)]
pub struct MemoryRelayController {
    channels: Mutex<HashMap<String, RelayChannelStatus>>,
}

impl MemoryRelayController {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register_space(&self, space_id: &str, channel: u16) -> Result<(), String> {
        let mut channels = self
            .channels
            .lock()
            .map_err(|_| "relay lock poisoned".to_string())?;
        channels.insert(
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

impl RelayController for MemoryRelayController {
    fn apply_bindings(&self, bindings: &[DeviceBinding]) -> Result<(), String> {
        let mut channels = self
            .channels
            .lock()
            .map_err(|_| "relay lock poisoned".to_string())?;
        let mut replacement = HashMap::new();
        for binding in bindings.iter().filter(|binding| binding.enabled) {
            let status = channels
                .get(&binding.space_id)
                .filter(|status| status.channel == binding.relay_channel)
                .cloned()
                .unwrap_or_else(|| RelayChannelStatus {
                    space_id: binding.space_id.clone(),
                    channel: binding.relay_channel,
                    output_active: false,
                    alarm_requested: false,
                    last_triggered_at: None,
                    auto_muted: false,
                });
            replacement.insert(binding.space_id.clone(), status);
        }
        *channels = replacement;
        Ok(())
    }

    fn set_alarm(&self, space_id: &str, enabled: bool) -> Result<(), String> {
        let mut channels = self
            .channels
            .lock()
            .map_err(|_| "relay lock poisoned".to_string())?;
        let entry = channels
            .get_mut(space_id)
            .ok_or_else(|| format!("relay channel for space {space_id} is not configured"))?;

        if enabled {
            if !entry.alarm_requested {
                entry.last_triggered_at = Some(Utc::now());
                entry.auto_muted = false;
            }
            entry.alarm_requested = true;
            entry.output_active = !entry.auto_muted;
        } else {
            // 车辆驶离或恢复正常，立即消警
            entry.alarm_requested = false;
            entry.output_active = false;
            entry.auto_muted = false;
        }
        Ok(())
    }

    fn snapshot(&self) -> Result<HashMap<String, bool>, String> {
        let channels = self
            .channels
            .lock()
            .map_err(|_| "relay lock poisoned".to_string())?;
        Ok(channels
            .iter()
            .map(|(k, v)| (k.clone(), v.output_active))
            .collect())
    }

    fn detailed_snapshot(&self) -> Result<HashMap<String, RelayChannelStatus>, String> {
        let channels = self
            .channels
            .lock()
            .map_err(|_| "relay lock poisoned".to_string())?;
        Ok(channels.clone())
    }

    fn tick_timeout_protection(&self, max_alarm_duration_seconds: i64) -> Result<(), String> {
        let mut channels = self
            .channels
            .lock()
            .map_err(|_| "relay lock poisoned".to_string())?;
        let now = Utc::now();
        for status in channels.values_mut() {
            if status.alarm_requested && status.output_active {
                if let Some(triggered_at) = status.last_triggered_at {
                    if (now - triggered_at).num_seconds() >= max_alarm_duration_seconds {
                        // 达到最大持续报警时间，自动静音，保护现场声光设备并不扰民
                        status.output_active = false;
                        status.auto_muted = true;
                    }
                }
            }
        }
        Ok(())
    }
}

/// 工业级 Modbus TCP 继电器控制器（连接真实 16 路网络继电器）
pub struct ModbusTcpRelayController {
    server_addr: SocketAddr,
    unit_id: u8,
    space_to_channel: Mutex<HashMap<String, u16>>,
    memory_state: MemoryRelayController,
    tx_counter: AtomicU16,
}

impl ModbusTcpRelayController {
    pub fn new(server_addr: SocketAddr, unit_id: u8) -> Self {
        Self {
            server_addr,
            unit_id,
            space_to_channel: Mutex::new(HashMap::new()),
            memory_state: MemoryRelayController::new(),
            tx_counter: AtomicU16::new(1),
        }
    }

    fn next_tx_id(&self) -> u16 {
        let prev = self.tx_counter.fetch_add(1, Ordering::SeqCst);
        if prev == 0 {
            self.tx_counter.store(2, Ordering::SeqCst);
            1
        } else {
            prev
        }
    }

    pub fn bind_space_channel(&self, space_id: &str, channel: u16) -> Result<(), String> {
        let mut map = self
            .space_to_channel
            .lock()
            .map_err(|_| "relay map lock poisoned".to_string())?;
        map.insert(space_id.to_string(), channel);
        self.memory_state.register_space(space_id, channel)?;
        Ok(())
    }

    fn send_modbus_tcp_write(&self, channel: u16, state: bool) -> Result<(), String> {
        let mut stream = TcpStream::connect_timeout(&self.server_addr, Duration::from_millis(1500))
            .map_err(|e| format!("无法连接 Modbus TCP 设备 {}: {}", self.server_addr, e))?;
        stream
            .set_read_timeout(Some(Duration::from_millis(1500)))
            .map_err(|e| e.to_string())?;
        stream
            .set_write_timeout(Some(Duration::from_millis(1500)))
            .map_err(|e| e.to_string())?;

        // 1. 发送写单个线圈指令 (0x05) 并严格校验响应与事务号
        let write_tx = self.next_tx_id();
        let frame = ModbusFrame::build_tcp_write_coil(write_tx, self.unit_id, channel, state);
        stream
            .write_all(&frame)
            .map_err(|e| format!("发送 Modbus TCP 写线圈指令失败: {}", e))?;

        let mut buffer = [0u8; 32];
        let bytes_read = stream
            .read(&mut buffer)
            .map_err(|e| format!("读取 Modbus TCP 响应超时或失败: {}", e))?;
        ModbusFrame::parse_tcp_response(&buffer[..bytes_read], write_tx)?;

        // 2. 写后回读物理状态校验 (0x01 Read Coils Verification)
        let read_tx = self.next_tx_id();
        let read_frame = ModbusFrame::build_tcp_read_coils(read_tx, self.unit_id, channel, 1);
        stream
            .write_all(&read_frame)
            .map_err(|e| format!("发送 Modbus TCP 回读线圈指令失败: {}", e))?;

        let bytes_read = stream
            .read(&mut buffer)
            .map_err(|e| format!("读取 Modbus TCP 回读响应超时或失败: {}", e))?;
        let pdu = ModbusFrame::parse_tcp_response(&buffer[..bytes_read], read_tx)?;
        if pdu.len() < 3 || pdu[0] != 0x01 {
            return Err("Modbus TCP 回读线圈响应格式异常".into());
        }
        let actual_state = (pdu[2] & 0x01) != 0;
        if actual_state != state {
            return Err(format!(
                "Modbus TCP 线圈 {} 回读校验失败: 期望 {}, 实际状态 {}",
                channel, state, actual_state
            ));
        }

        Ok(())
    }
}

impl RelayController for ModbusTcpRelayController {
    fn apply_bindings(&self, bindings: &[DeviceBinding]) -> Result<(), String> {
        let replacement: HashMap<String, u16> = bindings
            .iter()
            .filter(|binding| binding.enabled)
            .map(|binding| (binding.space_id.clone(), binding.relay_channel))
            .collect();
        let existing = self
            .space_to_channel
            .lock()
            .map_err(|_| "relay map lock poisoned".to_string())?
            .clone();
        let active = self.memory_state.snapshot()?;
        for (space_id, channel) in &existing {
            if active.get(space_id).copied().unwrap_or(false)
                && replacement.get(space_id) != Some(channel)
            {
                self.send_modbus_tcp_write(*channel, false)?;
            }
        }
        self.memory_state.apply_bindings(bindings)?;
        *self
            .space_to_channel
            .lock()
            .map_err(|_| "relay map lock poisoned".to_string())? = replacement;
        Ok(())
    }

    fn set_alarm(&self, space_id: &str, enabled: bool) -> Result<(), String> {
        let channel = {
            let map = self
                .space_to_channel
                .lock()
                .map_err(|_| "relay map lock poisoned".to_string())?;
            map.get(space_id)
                .copied()
                .ok_or_else(|| format!("relay channel for space {space_id} is not configured"))?
        };

        let detailed_before = self.memory_state.detailed_snapshot()?;
        let previous_output = detailed_before.get(space_id).map(|s| s.output_active);

        // 更新逻辑状态并应用消警保护
        self.memory_state.set_alarm(space_id, enabled)?;

        let detailed = self.memory_state.detailed_snapshot()?;
        let should_output = detailed
            .get(space_id)
            .map(|s| s.output_active)
            .unwrap_or(enabled);

        // 若物理触点状态未发生跳变，抑制重复的网络总线指令
        if previous_output == Some(should_output) {
            return Ok(());
        }

        // 发送物理 Modbus 信号
        self.send_modbus_tcp_write(channel, should_output)?;
        Ok(())
    }

    fn snapshot(&self) -> Result<HashMap<String, bool>, String> {
        self.memory_state.snapshot()
    }

    fn detailed_snapshot(&self) -> Result<HashMap<String, RelayChannelStatus>, String> {
        self.memory_state.detailed_snapshot()
    }

    fn tick_timeout_protection(&self, max_alarm_duration_seconds: i64) -> Result<(), String> {
        let before = self.memory_state.snapshot()?;
        self.memory_state
            .tick_timeout_protection(max_alarm_duration_seconds)?;
        let after = self.memory_state.snapshot()?;
        let channels = self
            .space_to_channel
            .lock()
            .map_err(|_| "relay map lock poisoned".to_string())?
            .clone();
        for (space_id, was_active) in before {
            if was_active && !after.get(&space_id).copied().unwrap_or(false) {
                let channel = channels.get(&space_id).copied().ok_or_else(|| {
                    format!("relay channel for space {space_id} is not configured")
                })?;
                self.send_modbus_tcp_write(channel, false)?;
            }
        }
        Ok(())
    }
}

/// 串口语音播报指令枚举
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum VoiceAlarmCommand {
    /// 停止播报
    Stop = 0x00,
    /// 燃油车占位劝离（音轨 1）
    FuelVehicleViolation = 0x01,
    /// 未充电超时劝离（音轨 2）
    NotChargingTimeout = 0x02,
    /// 充电完成超时劝离（音轨 3）
    CompletedTimeout = 0x03,
}

impl VoiceAlarmCommand {
    pub fn from_violation_type(violation_type: Option<&str>) -> Self {
        match violation_type {
            Some("FUEL_VEHICLE") => Self::FuelVehicleViolation,
            Some("NOT_CHARGING_OVERTIME") => Self::NotChargingTimeout,
            Some("CHARGE_COMPLETED_OVERTIME") => Self::CompletedTimeout,
            _ => Self::FuelVehicleViolation,
        }
    }
}

/// 串口语音控制帧编解码器
#[derive(Debug, Clone)]
pub struct VoiceFrame;

impl VoiceFrame {
    /// 构造标准工业 RS485 语音播报数据帧
    /// 格式: [0xAA, 0x55, 目标地址 1B, 命令字 1B, 通道/车位 1B, 累加和校验 1B]
    pub fn build_serial_frame(device_addr: u8, channel: u8, cmd: VoiceAlarmCommand) -> Vec<u8> {
        let mut frame = Vec::with_capacity(6);
        frame.push(0xAA);
        frame.push(0x55);
        frame.push(device_addr);
        frame.push(cmd as u8);
        frame.push(channel);
        let checksum = frame.iter().fold(0u8, |acc, &x| acc.wrapping_add(x));
        frame.push(checksum);
        frame
    }

    /// 校验串口语音响应
    pub fn verify_ack(response: &[u8]) -> bool {
        if response.len() < 4 {
            return false;
        }
        response[0] == 0xAA && response[1] == 0x55 && response[2] == 0x00
    }
}

/// 语音报警器驱动特征
pub trait VoiceAlarmController: Send + Sync {
    fn play_voice(&self, space_id: &str, channel: u8, cmd: VoiceAlarmCommand)
        -> Result<(), String>;
    fn stop_voice(&self, space_id: &str, channel: u8) -> Result<(), String>;
    fn get_last_command(&self, space_id: &str) -> Option<VoiceAlarmCommand>;
}

/// 内存语音报警模拟器
#[derive(Default)]
pub struct MemoryVoiceAlarmController {
    current_commands: Mutex<HashMap<String, VoiceAlarmCommand>>,
}

impl VoiceAlarmController for MemoryVoiceAlarmController {
    fn play_voice(
        &self,
        space_id: &str,
        _channel: u8,
        cmd: VoiceAlarmCommand,
    ) -> Result<(), String> {
        let mut map = self
            .current_commands
            .lock()
            .map_err(|_| "voice lock poisoned".to_string())?;
        map.insert(space_id.to_string(), cmd);
        Ok(())
    }

    fn stop_voice(&self, space_id: &str, _channel: u8) -> Result<(), String> {
        let mut map = self
            .current_commands
            .lock()
            .map_err(|_| "voice lock poisoned".to_string())?;
        map.insert(space_id.to_string(), VoiceAlarmCommand::Stop);
        Ok(())
    }

    fn get_last_command(&self, space_id: &str) -> Option<VoiceAlarmCommand> {
        self.current_commands.lock().ok()?.get(space_id).copied()
    }
}

/// 双模驱动控制器（高低电平继电器触点 + RS485 串口语音播报）
pub struct DualModeAlarmController {
    relay: Arc<dyn RelayController>,
    voice: Arc<dyn VoiceAlarmController>,
    space_to_channel: Mutex<HashMap<String, u8>>,
}

impl DualModeAlarmController {
    pub fn new(relay: Arc<dyn RelayController>, voice: Arc<dyn VoiceAlarmController>) -> Self {
        Self {
            relay,
            voice,
            space_to_channel: Mutex::new(HashMap::new()),
        }
    }

    pub fn bind_space(&self, space_id: &str, channel: u8) {
        if let Ok(mut map) = self.space_to_channel.lock() {
            map.insert(space_id.to_string(), channel);
        }
    }

    /// 触发双模告警
    pub fn trigger_alarm(
        &self,
        space_id: &str,
        violation_type: Option<&str>,
        enabled: bool,
    ) -> Result<(), String> {
        let channel = self
            .space_to_channel
            .lock()
            .map(|m| m.get(space_id).copied().unwrap_or(0))
            .unwrap_or(0);

        // 1. 继电器高低电平触点通断控制
        self.relay.set_alarm(space_id, enabled)?;

        // 2. 串口定制语音播报驱动
        if enabled {
            let cmd = VoiceAlarmCommand::from_violation_type(violation_type);
            self.voice.play_voice(space_id, channel, cmd)?;
        } else {
            self.voice.stop_voice(space_id, channel)?;
        }
        Ok(())
    }

    pub fn relay(&self) -> &Arc<dyn RelayController> {
        &self.relay
    }

    pub fn voice(&self) -> &Arc<dyn VoiceAlarmController> {
        &self.voice
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_modbus_crc16_standard() {
        // 标准 Modbus 测试用例: [0x01, 0x05, 0x00, 0x00, 0xFF, 0x00]
        // 低字节 0x8C, 高字节 0x3A -> u16 为 0x3A8C
        let data = [0x01, 0x05, 0x00, 0x00, 0xFF, 0x00];
        let crc = calculate_crc16(&data);
        assert_eq!(crc, 0x3A8C);
    }

    #[test]
    fn test_build_rtu_write_coil_and_validate() {
        let frame = ModbusFrame::build_rtu_write_coil(1, 0, true);
        assert_eq!(frame.len(), 8);
        assert_eq!(frame[0], 0x01); // SlaveID
        assert_eq!(frame[1], 0x05); // Function Code
        assert_eq!(frame[2], 0x00); // Addr High
        assert_eq!(frame[3], 0x00); // Addr Low
        assert_eq!(frame[4], 0xFF); // Value High (ON)
        assert_eq!(frame[5], 0x00); // Value Low

        // 校验生成的 CRC
        assert!(ModbusFrame::parse_rtu_response(&frame).is_ok());
    }

    #[test]
    fn test_build_tcp_write_coil_and_parse() {
        let frame = ModbusFrame::build_tcp_write_coil(42, 1, 3, false);
        assert_eq!(frame.len(), 12);
        assert_eq!(frame[0], 0x00); // TxID Hi
        assert_eq!(frame[1], 42); // TxID Lo
        assert_eq!(frame[7], 0x05); // Function Code
        assert_eq!(frame[9], 3); // Channel 3
        assert_eq!(frame[10], 0x00); // Value OFF

        let parsed = ModbusFrame::parse_tcp_response(&frame, 42);
        assert!(parsed.is_ok());
    }

    #[test]
    fn test_alarm_auto_stop_and_release() {
        let controller = MemoryRelayController::new();
        controller.register_space("A-01", 0).unwrap();

        // 触发报警
        controller.set_alarm("A-01", true).unwrap();
        assert_eq!(controller.snapshot().unwrap().get("A-01"), Some(&true));

        // 模拟超时保护：若报警持续达到 300 秒，自动断开声光
        controller.tick_timeout_protection(0).unwrap(); // 传 0 立即触发超时
        let detailed = controller.detailed_snapshot().unwrap();
        let status = detailed.get("A-01").unwrap();
        assert!(!status.output_active, "继电器物理输出应当已被自动断开");
        assert!(status.auto_muted, "应当标记为 auto_muted");
        assert!(status.alarm_requested, "业务判定依然是违规报警状态");

        // 当车辆驶离或合规，消警复位
        controller.set_alarm("A-01", false).unwrap();
        let reset_status = controller
            .detailed_snapshot()
            .unwrap()
            .get("A-01")
            .unwrap()
            .clone();
        assert!(!reset_status.output_active);
        assert!(!reset_status.auto_muted);
        assert!(!reset_status.alarm_requested);
    }

    #[test]
    fn test_voice_frame_and_dual_mode_controller() {
        let frame = VoiceFrame::build_serial_frame(1, 0, VoiceAlarmCommand::FuelVehicleViolation);
        assert_eq!(frame.len(), 6);
        assert_eq!(frame[0], 0xAA);
        assert_eq!(frame[1], 0x55);
        assert_eq!(frame[2], 0x01); // Device Addr
        assert_eq!(frame[3], 0x01); // Fuel Vehicle Track 1
        assert_eq!(frame[4], 0x00); // Channel 0

        let relay = Arc::new(MemoryRelayController::new());
        relay.register_space("A-01", 0).unwrap();
        let voice = Arc::new(MemoryVoiceAlarmController::default());
        let dual = DualModeAlarmController::new(relay.clone(), voice.clone());
        dual.bind_space("A-01", 0);

        // 触发燃油车违规：继电器触点闭合 + 语音播报音轨 1
        dual.trigger_alarm("A-01", Some("FUEL_VEHICLE"), true)
            .unwrap();
        assert_eq!(relay.snapshot().unwrap().get("A-01"), Some(&true));
        assert_eq!(
            voice.get_last_command("A-01"),
            Some(VoiceAlarmCommand::FuelVehicleViolation)
        );

        // 车辆离场消警：继电器断开 + 语音停止
        dual.trigger_alarm("A-01", None, false).unwrap();
        assert_eq!(relay.snapshot().unwrap().get("A-01"), Some(&false));
        assert_eq!(
            voice.get_last_command("A-01"),
            Some(VoiceAlarmCommand::Stop)
        );
    }
}
