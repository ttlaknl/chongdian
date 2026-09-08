use charging_space_edge::{
    evaluate, ChargerStatus, EvaluationInput, EvaluationResult, MemoryRelayController,
    RelayController, RuleConfig, VehicleType,
};
use chrono::{Duration, Utc};

#[tokio::main]
async fn main() {
    let now = Utc::now();
    let rules = RuleConfig::default();
    let relay = MemoryRelayController::default();
    let cases = [
        (
            "A-01",
            "燃油车立即违规",
            VehicleType::Fuel,
            ChargerStatus::Idle,
            1,
            false,
        ),
        (
            "A-02",
            "新能源车正常充电",
            VehicleType::NewEnergy,
            ChargerStatus::Charging,
            900,
            false,
        ),
        (
            "A-03",
            "新能源车未充电超时",
            VehicleType::NewEnergy,
            ChargerStatus::Idle,
            601,
            false,
        ),
        (
            "A-04",
            "充电完成后滞留超时",
            VehicleType::NewEnergy,
            ChargerStatus::Completed,
            1200,
            false,
        ),
        (
            "A-05",
            "白名单车辆",
            VehicleType::Fuel,
            ChargerStatus::Idle,
            1200,
            true,
        ),
        (
            "A-06",
            "故障允许停留",
            VehicleType::NewEnergy,
            ChargerStatus::Fault,
            1200,
            false,
        ),
    ];
    let edge_url = std::env::var("SIM_EDGE_URL")
        .ok()
        .filter(|value| !value.is_empty());
    let client = reqwest::Client::new();
    for (space_id, name, vehicle_type, charger_status, age, whitelisted) in cases {
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
        let result = if let Some(base) = &edge_url {
            client
                .post(format!(
                    "{}/api/v1/spaces/{}/evaluate",
                    base.trim_end_matches('/'),
                    space_id
                ))
                .json(&input)
                .send()
                .await
                .expect("call edge node")
                .error_for_status()
                .expect("edge response")
                .json::<EvaluationResult>()
                .await
                .expect("decode edge result")
        } else {
            let result = evaluate(&input, &rules);
            relay
                .set_alarm(&input.space_id, result.should_alarm)
                .expect("memory relay");
            result
        };
        println!(
            "{} => {:?}, alarm={}, type={:?}",
            name, result.decision, result.should_alarm, result.violation_type
        );
    }
}
