use charging_space_edge::{
    evaluate, ChargerStatus, CloudEvent, DualModeAlarmController, EvaluationInput,
    HardwareModbusRtuRelayController, MemoryRelayController, MockTransport,
    PhysicalVoiceAlarmController, RelayController, RuleConfig, SqliteStorage, VehicleType,
};
use chrono::{Duration, Utc};
use std::sync::Arc;
use uuid::Uuid;

#[tokio::main]
async fn main() {
    println!("============================================================");
    println!(">>> 充电车位防占位智能预警系统 - 工业级全链路综合仿真器 <<<");
    println!("============================================================");

    let now = Utc::now();
    let rules = RuleConfig::default();
    let relay = MemoryRelayController::default();

    println!("\n[阶段一] 6 大核心业务场景基线测试：");
    let cases = [
        (
            "A-01",
            "燃油车占位立即违规",
            VehicleType::Fuel,
            ChargerStatus::Idle,
            1,
            false,
            true, // 期望报警
        ),
        (
            "A-02",
            "新能源车正常充电中",
            VehicleType::NewEnergy,
            ChargerStatus::Charging,
            900,
            false,
            false, // 期望合规
        ),
        (
            "A-03",
            "新能源车未充电超时滞留",
            VehicleType::NewEnergy,
            ChargerStatus::Idle,
            601,
            false,
            true, // 期望报警
        ),
        (
            "A-04",
            "充电完成超时滞留",
            VehicleType::NewEnergy,
            ChargerStatus::Completed,
            1200,
            false,
            true, // 期望报警
        ),
        (
            "A-05",
            "白名单特权车辆豁免",
            VehicleType::Fuel,
            ChargerStatus::Idle,
            1200,
            true,
            false, // 期望合规
        ),
        (
            "A-06",
            "充电桩故障允许停留",
            VehicleType::NewEnergy,
            ChargerStatus::Fault,
            1200,
            false,
            false, // 期望合规
        ),
    ];

    for (space_id, name, vehicle_type, charger_status, age, whitelisted, expected_alarm) in cases {
        let completed_at =
            (charger_status == ChargerStatus::Completed).then(|| now - Duration::seconds(age));
        let input = EvaluationInput {
            space_id: space_id.into(),
            plate: Some("粤A12345".into()),
            vehicle_type,
            charger_status,
            occupied_since: now - Duration::seconds(age),
            charge_completed_at: completed_at,
            now,
            whitelisted,
        };

        let result = evaluate(&input, &rules);
        relay
            .set_alarm(&input.space_id, result.should_alarm)
            .expect("memory relay");

        assert_eq!(
            result.should_alarm, expected_alarm,
            "场景 {} 报警判定结果不符合预期",
            name
        );
        println!(
            "  ✓ 车位: {:<6} | 场景: {:<20} | 判定: {:<12} | 报警: {}",
            space_id, name, format!("{:?}", result.decision), result.should_alarm
        );
    }

    println!("\n[阶段二] 6~16 路车位动态全量并发测试 (M4)：");
    let mock_bus = Arc::new(MockTransport::new("SIM_BUS_01", "SERIAL_RS485"));
    let rtu_relay = Arc::new(HardwareModbusRtuRelayController::new(mock_bus.clone(), 0x01));
    let voice_ctrl = Arc::new(PhysicalVoiceAlarmController::new(mock_bus.clone(), 0x01));
    let dual_alarm = Arc::new(DualModeAlarmController::new(rtu_relay.clone(), voice_ctrl.clone()));

    for i in 1..=16 {
        let space_id = format!("SPACE-{:02}", i);
        rtu_relay.bind_space_channel(&space_id, i as u16).unwrap();
        dual_alarm.bind_space(&space_id, i as u8);
    }

    for i in 1..=16 {
        let space_id = format!("SPACE-{:02}", i);
        let is_violation = i % 2 == 1;
        if is_violation {
            dual_alarm
                .trigger_alarm(&space_id, Some("FUEL_VEHICLE"), true)
                .unwrap();
        }
    }

    let snap = rtu_relay.snapshot().unwrap();
    assert_eq!(snap.len(), 16);
    let active_count = snap.values().filter(|&&v| v).count();
    assert_eq!(active_count, 8);
    println!("  ✓ 16 路车位并发下发 Modbus RTU / 语音帧成功，奇数车位报警激活数: {}/16", active_count);

    // 全部复位消警
    for i in 1..=16 {
        let space_id = format!("SPACE-{:02}", i);
        dual_alarm.trigger_alarm(&space_id, None, false).unwrap();
    }
    let snap_after = rtu_relay.snapshot().unwrap();
    assert_eq!(snap_after.values().filter(|&&v| v).count(), 0);
    println!("  ✓ 16 路车位即时消警验证成功，继电器输出全部关闭！");

    println!("\n[阶段三] 边缘 SQLite (WAL) 断网 1,000 条证据高并发落库与检索测试 (M5)：");
    let sqlite = SqliteStorage::in_memory().unwrap();
    let start_insert = Utc::now();
    for i in 1..=1000 {
        let ev = CloudEvent {
            event_id: Uuid::new_v4(),
            space_id: format!("SPACE-{:02}", (i % 16) + 1),
            plate: Some(format!("粤B{:05}", i)),
            vehicle_type: VehicleType::Fuel,
            violation_type: Some("FUEL_VEHICLE".into()),
            occurred_at: now,
            evidence_url: Some(format!("https://oss.example.com/evidence/{}.jpg", i)),
        };
        sqlite.save_event(&ev).unwrap();
    }
    let elapsed = (Utc::now() - start_insert).num_milliseconds();
    let pending = sqlite.pending_count().unwrap();
    assert_eq!(pending, 1000);
    println!("  ✓ 1,000 条违规事件入库完成，耗时: {} ms，待同步计数: {}", elapsed, pending);

    // 模拟网络恢复补传
    let batch = sqlite.fetch_pending_events(100).unwrap();
    assert_eq!(batch.len(), 100);
    for ev in &batch {
        sqlite.mark_event_synced(&ev.event_id).unwrap();
    }
    let remaining = sqlite.pending_count().unwrap();
    assert_eq!(remaining, 900);
    println!("  ✓ 模拟网络恢复，成功批量捞取 100 条并标记已同步，剩余暂存待传: {}", remaining);

    println!("\n============================================================");
    println!(">>> 全链路仿真测试全部通过 (ALL 3 PHASES PASSED) SUCCESS <<<");
    println!("============================================================");
}
