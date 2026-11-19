use charging_space_edge::{
    calculate_crc16, compute_v4_signature, evaluate, feed_hardware_watchdog,
    magic_close_hardware_watchdog, sd_notify, ChargerStatus, CloudEvent, Decision, EvaluationInput,
    EventStatus, MetricsCollector, RemoteUpgradeManager, RuleConfig, SqliteStorage,
    UpgradeManifest, VehicleType,
};
use chrono::Utc;
use std::fs;
use uuid::Uuid;

fn main() {
    println!("==================================================");
    println!("   CHARGING SPACE EDGE - SYSTEM SMOKE TEST RUNNER");
    println!("   Industrial Cloud-Edge Anti-Occupancy Platform");
    println!("==================================================");

    let mut passed = 0;
    let total = 7;

    // 1. SQLite Storage & Event Spool Persistence
    print!("[TEST 1/7] SQLite Storage & Event Spool... ");
    let temp_dir = std::env::temp_dir().join(format!("smoke_test_{}", Uuid::new_v4()));
    fs::create_dir_all(&temp_dir).expect("create temp dir");
    let db_path = temp_dir.join("edge.db");
    let storage = SqliteStorage::open(&db_path).expect("open sqlite");
    let test_id = Uuid::new_v4();
    let now = Utc::now();
    let event = CloudEvent {
        event_id: test_id,
        session_id: "smoke-s1".into(),
        space_id: "A-01".into(),
        plate: Some("粤B88888".into()),
        vehicle_type: VehicleType::Fuel,
        violation_type: Some("FUEL_VEHICLE".into()),
        occurred_at: now,
        evidence_url: None,
        status: EventStatus::Alarming,
        cleared_at: None,
        updated_at: now,
    };
    storage.save_event(&event).expect("save event");
    assert_eq!(storage.pending_count().expect("pending count"), 1);
    storage.mark_event_synced(&test_id).expect("mark synced");
    assert_eq!(storage.pending_count().expect("pending count"), 0);
    println!("PASSED");
    passed += 1;

    // 2. Anti-Occupancy Rule Engine & State Machine
    print!("[TEST 2/7] Anti-Occupancy Rule Engine... ");
    let rules = RuleConfig::default();
    let input = EvaluationInput {
        space_id: "A-01".into(),
        plate: Some("粤B88888".into()),
        vehicle_type: VehicleType::Fuel,
        charger_status: ChargerStatus::Idle,
        occupied_since: now,
        charge_completed_at: None,
        now,
        whitelisted: false,
    };
    let eval = evaluate(&input, &rules);
    assert_eq!(eval.decision, Decision::Violation);
    assert!(eval.should_alarm);
    assert_eq!(eval.violation_type, Some("FUEL_VEHICLE".into()));
    println!("PASSED");
    passed += 1;

    // 3. Modbus RTU Protocol & Standard CRC16
    print!("[TEST 3/7] Modbus RTU Protocol & CRC16... ");
    let rtu_frame = [0x01, 0x05, 0x00, 0x00, 0xFF, 0x00];
    let crc = calculate_crc16(&rtu_frame);
    assert_eq!(crc, 0x3A8C);
    println!("PASSED");
    passed += 1;

    // 4. Hardware Watchdog, Magic Close & systemd sd_notify
    print!("[TEST 4/7] Watchdog, Magic Close & sd_notify... ");
    let wd_file = temp_dir.join("smoke_watchdog");
    fs::write(&wd_file, b"").unwrap();
    assert!(feed_hardware_watchdog(wd_file.to_str().unwrap()));
    assert!(magic_close_hardware_watchdog(wd_file.to_str().unwrap()));
    assert_eq!(fs::read(&wd_file).unwrap(), b"V");
    std::env::set_var("NOTIFY_SOCKET", "/run/systemd/notify-test");
    assert!(sd_notify("READY=1"));
    assert!(sd_notify("WATCHDOG=1"));
    std::env::remove_var("NOTIFY_SOCKET");
    println!("PASSED");
    passed += 1;

    // 5. Pure Rust AWS/OSS V4 Signatures
    print!("[TEST 5/7] Pure Rust AWS/OSS V4 Signature... ");
    let sig = compute_v4_signature(
        "mock_secret_key",
        "20260908",
        "cn-hangzhou",
        "oss",
        "AWS4-HMAC-SHA256\n20260908T000000Z\n20260908/cn-hangzhou/oss/aws4_request\n123456",
    );
    assert_eq!(sig.len(), 64);
    println!("PASSED");
    passed += 1;

    // 6. OTA Remote Upgrade, Signature & Rollback Probe
    print!("[TEST 6/7] OTA Remote Upgrade & Auto-Rollback... ");
    let bin_path = temp_dir.join("smoke_app.bin");
    fs::write(&bin_path, b"BIN_V1").unwrap();
    let upgrader = RemoteUpgradeManager::new(1, "secret_key".into(), bin_path.clone());
    let pkg = b"NEW_BIN_V2";
    let pkg_hash = charging_space_edge::storage::compute_sha256_hex(pkg);
    let sig = charging_space_edge::upgrade::compute_hmac_sha256_hex(
        "secret_key",
        &format!("2:{}:http://pkg/v2", pkg_hash),
    );
    let manifest = UpgradeManifest {
        version: 2,
        version_name: "2.0".into(),
        package_url: "http://pkg/v2".into(),
        sha256_hash: pkg_hash,
        signature: sig,
        min_compatible_version: 1,
        release_notes: "smoke test upgrade".into(),
        created_at: now,
    };
    // 验证探测失败回滚
    let rollback_res = upgrader.apply_upgrade(&manifest, pkg, || Err("Probe failed".into()));
    assert!(rollback_res.is_err());
    assert_eq!(fs::read(&bin_path).unwrap(), b"BIN_V1");
    // 验证探测成功提交
    let success_res = upgrader.apply_upgrade(&manifest, pkg, || Ok(()));
    assert!(success_res.is_ok());
    assert_eq!(fs::read(&bin_path).unwrap(), b"NEW_BIN_V2");
    println!("PASSED");
    passed += 1;

    // 7. System Telemetry & Prometheus Metrics
    print!("[TEST 7/7] Telemetry & Prometheus Exporter... ");
    let metrics = MetricsCollector::new(
        "smoke-node".into(),
        temp_dir.to_str().unwrap().into(),
        100 * 1024 * 1024,
    );
    metrics.record_frame();
    metrics.record_violation();
    let prom = metrics.export_prometheus_text();
    assert!(prom.contains("chg_edge_frames_processed_total{node=\"smoke-node\"} 1"));
    assert!(prom.contains("chg_edge_violations_detected_total{node=\"smoke-node\"} 1"));
    println!("PASSED");
    passed += 1;

    let _ = fs::remove_dir_all(&temp_dir);

    println!("==================================================");
    println!(
        "ALL {}/{} SUB-SYSTEM SMOKE CHECKS PASSED (100% HEALTHY)",
        passed, total
    );
    println!("==================================================");
}
