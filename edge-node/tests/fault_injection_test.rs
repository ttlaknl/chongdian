use charging_space_edge::{
    calculate_staleness, ChargerStatus, CloudEvent, EventStatus, EvidenceStorage,
    HardwareHealthMonitor, MockTransport, SqliteStorage, StalenessStatus, VehicleType,
};
use chrono::Utc;
use std::{fs, sync::Arc};
use uuid::Uuid;

#[test]
fn test_fault_1_power_loss_sqlite_recovery() {
    let temp_dir = std::env::temp_dir().join(format!("test_fault1_{}", Uuid::new_v4()));
    fs::create_dir_all(&temp_dir).unwrap();
    let db_path = temp_dir.join("edge.db");

    let event_id = Uuid::new_v4();
    let now = Utc::now();
    let event = CloudEvent {
        event_id,
        session_id: "session-fault-1".into(),
        space_id: "A-01".into(),
        plate: Some("粤B11111".into()),
        vehicle_type: VehicleType::Fuel,
        violation_type: Some("FUEL_VEHICLE".into()),
        occurred_at: now,
        evidence_url: Some("file:///evidence/1.jpg".into()),
        status: EventStatus::Alarming,
        cleared_at: None,
        updated_at: now,
    };

    // 1. 模拟断电前写入事件
    {
        let storage = SqliteStorage::open(&db_path).unwrap();
        storage.save_event(&event).unwrap();
        assert_eq!(storage.pending_count().unwrap(), 1);
        // 作用域结束模拟进程被强制杀死（模拟断电）
    }

    // 2. 模拟系统重启，重新加载 SQLite 数据库
    {
        let reopened = SqliteStorage::open(&db_path).unwrap();
        let pending = reopened.fetch_pending_events(100).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].event_id, event_id);
        assert_eq!(pending[0].space_id, "A-01");

        // 标记上传成功
        reopened.mark_event_synced(&event_id).unwrap();
        assert_eq!(reopened.pending_count().unwrap(), 0);
    }

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_fault_2_cloud_network_partition_and_drain() {
    let temp_dir = std::env::temp_dir().join(format!("test_fault2_{}", Uuid::new_v4()));
    fs::create_dir_all(&temp_dir).unwrap();
    let db_path = temp_dir.join("edge.db");
    let storage = SqliteStorage::open(&db_path).unwrap();

    let now = Utc::now();
    // 模拟云端断网期间本地堆积了 5 个告警事件
    let mut ids = Vec::new();
    for i in 0..5 {
        let eid = Uuid::new_v4();
        ids.push(eid);
        let event = CloudEvent {
            event_id: eid,
            session_id: format!("session-partition-{}", i),
            space_id: format!("A-{:02}", i + 1),
            plate: Some(format!("粤B{:05}", 30000 + i)),
            vehicle_type: VehicleType::Fuel,
            violation_type: Some("FUEL_VEHICLE".into()),
            occurred_at: now,
            evidence_url: None,
            status: EventStatus::Alarming,
            cleared_at: None,
            updated_at: now,
        };
        storage.save_event(&event).unwrap();
    }

    // 断网期间离线缓存计数为 5
    assert_eq!(storage.pending_count().unwrap(), 5);

    // 模拟网络恢复，补传全部待传队列
    let pending_batch = storage.fetch_pending_events(100).unwrap();
    assert_eq!(pending_batch.len(), 5);
    for ev in pending_batch {
        storage.mark_event_synced(&ev.event_id).unwrap();
    }

    // 补传完毕，积压清零
    assert_eq!(storage.pending_count().unwrap(), 0);

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_fault_3_operator_api_timeout_and_staleness() {
    let now = Utc::now();
    // 1. 桩企接口数据在 5 秒内，判定为新鲜
    let fresh_time = now - chrono::Duration::seconds(5);
    let s_fresh = calculate_staleness(Some(fresh_time), &ChargerStatus::Idle, now, 15);
    assert_eq!(s_fresh, StalenessStatus::Fresh);

    // 2. 桩企接口超时 120 秒未更新，判定为陈旧（Stale）
    let stale_time = now - chrono::Duration::seconds(120);
    let s_stale = calculate_staleness(Some(stale_time), &ChargerStatus::Idle, now, 15);
    match s_stale {
        StalenessStatus::Stale(secs) => assert!(secs >= 120),
        other => panic!("expected Stale, got {:?}", other),
    }
}

#[test]
fn test_fault_4_hardware_strict_mode_vs_degraded_fallback() {
    let transport = MockTransport::new("bus-fault", "MOCK");
    transport.set_connected(false); // 模拟继电器总线物理断线
    let transport_arc: Arc<dyn charging_space_edge::HardwareTransport> = Arc::new(transport);

    // 场景 A: 生产严格模式 (strict = true)
    let strict_monitor = HardwareHealthMonitor::with_strict_mode(transport_arc.clone(), true);
    let report_strict = strict_monitor.report();
    assert_eq!(report_strict.status, "OFFLINE_CRITICAL");
    assert!(!report_strict.connected);
    assert!(report_strict.strict_mode);

    // 场景 B: 调试开发模式 (strict = false)
    let dev_monitor = HardwareHealthMonitor::with_strict_mode(transport_arc.clone(), false);
    let report_dev = dev_monitor.report();
    assert_eq!(report_dev.status, "DEGRADED_MOCK_FALLBACK");
    assert!(report_dev.is_simulated);
    assert!(!report_dev.strict_mode);
}

#[test]
fn test_fault_5_storage_quota_fifo_cleanup() {
    let temp_dir = std::env::temp_dir().join(format!("test_fault5_{}", Uuid::new_v4()));
    fs::create_dir_all(&temp_dir).unwrap();

    // 创建 3 个证据文件与其 sidecar json，每个文件 500KB，总共约 1.5MB
    let payload = vec![0xAAu8; 500 * 1024];
    for i in 1..=3 {
        let jpg = temp_dir.join(format!("evidence_{}.jpg", i));
        let sidecar = temp_dir.join(format!("evidence_{}.json", i));
        fs::write(&jpg, &payload).unwrap();
        fs::write(&sidecar, b"{\"sha256\":\"mock\"}").unwrap();
        // 制造修改时间差
        thread_sleep(50);
    }

    // 设置配额为 800KB（只能容纳 1 个 500KB 图片），最大保存 30 天
    let storage = EvidenceStorage::with_limits(&temp_dir, 800 * 1024, 30);
    let (deleted_count, freed_bytes) = storage.cleanup_local_quota().unwrap();
    assert!(deleted_count > 0);
    assert!(freed_bytes > 0);

    // 验证最早的 evidence_1.jpg 已被清理
    assert!(!temp_dir.join("evidence_1.jpg").exists());
    assert!(!temp_dir.join("evidence_1.json").exists());

    // 验证最新的 evidence_3.jpg 仍完整保留
    assert!(temp_dir.join("evidence_3.jpg").exists());

    let _ = fs::remove_dir_all(&temp_dir);
}

fn thread_sleep(ms: u64) {
    std::thread::sleep(std::time::Duration::from_millis(ms));
}
