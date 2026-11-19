use charging_space_edge::{
    evaluate, ChargerStatus, Decision, DeviceBinding, EvaluationInput, EventLifecycleRegistry,
    MemoryRelayController, PresencePhase, RawDetection, RelayController, RuleConfig,
    SpacePresenceTracker, SpaceStatusRegistry, SqliteStorage, TransitionEvent, VehicleType,
};
use chrono::Utc;
use std::{
    sync::{Arc, Mutex},
    thread,
};

#[test]
fn test_16_channel_concurrent_load_and_state_transitions() {
    let now = Utc::now();
    let rules = RuleConfig {
        uncharged_grace_seconds: 5,
        completed_grace_seconds: 5,
        vehicle_enter_confirm_seconds: 1,
        vehicle_leave_confirm_seconds: 1,
        alarm_repeat_seconds: 2,
        alarm_auto_stop_seconds: 10,
        ..Default::default()
    };

    let lifecycle = Arc::new(EventLifecycleRegistry::default());
    let relay = Arc::new(MemoryRelayController::new());
    let temp_dir = std::env::temp_dir().join(format!("test_16ch_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&temp_dir).unwrap();
    let sqlite = Arc::new(SqliteStorage::open(temp_dir.join("edge.db")).unwrap());

    // 建立 16 个车位配置绑定
    let mut bindings = Vec::new();
    for i in 0..16 {
        let space_id = format!("SPACE-{:02}", i + 1);
        bindings.push(DeviceBinding {
            site_id: "site-full-16ch".into(),
            space_id: space_id.clone(),
            camera_id: format!("cam-{:02}", i + 1),
            charger_id: format!("charger-{:02}", i + 1),
            relay_channel: i as u16,
            speaker_channel: Some(i as u16),
            enabled: true,
            ..Default::default()
        });
        relay.register_space(&space_id, i as u16).unwrap();
    }

    let status_registry = Arc::new(SpaceStatusRegistry::default());
    status_registry
        .apply_bindings("edge-16ch-node", &bindings)
        .unwrap();

    let completed_tasks = Arc::new(Mutex::new(0usize));
    let mut handles = Vec::new();

    // 并发启动 16 个线程分别独立处理 16 路车位生命周期
    for i in 0..16 {
        let space_id = format!("SPACE-{:02}", i + 1);
        let rules = rules.clone();
        let lifecycle = lifecycle.clone();
        let relay = relay.clone();
        let sqlite = sqlite.clone();
        let completed = completed_tasks.clone();

        let handle = thread::spawn(move || {
            let mut tracker = SpacePresenceTracker::new(space_id.clone());
            let is_ev = i % 2 == 0;
            let plate = if is_ev {
                format!("粤D{:05}", 10000 + i)
            } else {
                format!("粤B{:05}", 20000 + i)
            };
            let v_type = if is_ev {
                VehicleType::NewEnergy
            } else {
                VehicleType::Fuel
            };

            // 1. 车辆进入状态机
            let detected = RawDetection {
                space_id: space_id.clone(),
                has_vehicle: true,
                plate: Some(plate.clone()),
                vehicle_type: v_type.clone(),
                captured_at: now,
                evidence_image_path: None,
                is_simulated: false,
                data_source: "REAL_STREAM".to_string(),
                confidence: Some(0.95),
                plate_color: None,
                camera_id: None,
                image_base64: None,
            };

            let _ = tracker.update(&detected, &rules);
            assert_eq!(tracker.phase, PresencePhase::Entering);

            let confirmed_frame = RawDetection {
                captured_at: now + chrono::Duration::seconds(2),
                ..detected.clone()
            };
            let transition2 = tracker.update(&confirmed_frame, &rules);
            assert!(matches!(
                transition2,
                TransitionEvent::VehicleEntered { .. }
            ));
            assert_eq!(tracker.phase, PresencePhase::Occupied);

            // 2. 规则评估
            let input = EvaluationInput {
                space_id: space_id.clone(),
                plate: Some(plate),
                vehicle_type: v_type.clone(),
                charger_status: ChargerStatus::Idle,
                occupied_since: now,
                charge_completed_at: None,
                now,
                whitelisted: false,
            };

            let mut eval = evaluate(&input, &rules);

            if v_type == VehicleType::Fuel {
                // 燃油车应判定违规
                assert!(eval.should_alarm);
                assert_eq!(eval.decision, Decision::Violation);
                relay.set_alarm(&space_id, true).unwrap();
                let events = lifecycle
                    .process_evaluation(&input, &mut eval, None)
                    .unwrap();
                for ev in events {
                    sqlite.save_event(&ev).unwrap();
                }
            } else {
                // 新能源车初始宽限不报警
                assert!(!eval.should_alarm);
            }

            // 3. 车辆驶离
            let empty_det = RawDetection {
                space_id: space_id.clone(),
                has_vehicle: false,
                plate: None,
                vehicle_type: VehicleType::Unknown,
                captured_at: now + chrono::Duration::seconds(5),
                evidence_image_path: None,
                is_simulated: false,
                data_source: "REAL_STREAM".to_string(),
                confidence: Some(0.95),
                plate_color: None,
                camera_id: None,
                image_base64: None,
            };
            let _ = tracker.update(&empty_det, &rules);
            let empty_confirmed = RawDetection {
                captured_at: now + chrono::Duration::seconds(8),
                ..empty_det
            };
            let _ = tracker.update(&empty_confirmed, &rules);
            assert_eq!(tracker.phase, PresencePhase::Empty);

            if v_type == VehicleType::Fuel {
                relay.set_alarm(&space_id, false).unwrap();
                let cleared_events = lifecycle
                    .clear_space(&space_id, now + chrono::Duration::seconds(8))
                    .unwrap();
                for cleared in cleared_events {
                    sqlite.save_event(&cleared).unwrap();
                }
            }

            let mut count = completed.lock().unwrap();
            *count += 1;
        });

        handles.push(handle);
    }

    for h in handles {
        h.join().unwrap();
    }

    assert_eq!(*completed_tasks.lock().unwrap(), 16);
    let pending_cnt = sqlite.pending_count().unwrap();
    assert!(pending_cnt > 0);

    let _ = std::fs::remove_dir_all(&temp_dir);
}
