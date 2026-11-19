use crate::{ChargerStatus, DynamicChargerAdapter, ExternalChargerUpdate, StalenessStatus};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, sync::Arc};

/// 充电桩与车位映射字典（严禁推测映射）
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ConnectorSpaceMapping {
    /// 枪号 -> 车位号显式配置表
    pub mapping: HashMap<String, String>,
}

impl ConnectorSpaceMapping {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, connector_id: impl Into<String>, space_id: impl Into<String>) {
        self.mapping.insert(connector_id.into(), space_id.into());
    }

    /// 从环境变量 `OPERATOR_CONNECTOR_MAPPINGS` 加载映射（格式：chg1:A-01,chg2:A-02）
    pub fn from_env() -> Self {
        let mut mapping = HashMap::new();
        if let Ok(raw) = std::env::var("OPERATOR_CONNECTOR_MAPPINGS") {
            for pair in raw.split(',') {
                let parts: Vec<&str> = pair.split([':', '=']).collect();
                if parts.len() == 2 {
                    let k = parts[0].trim();
                    let v = parts[1].trim();
                    if !k.is_empty() && !v.is_empty() {
                        mapping.insert(k.to_string(), v.to_string());
                    }
                }
            }
        }
        Self { mapping }
    }

    /// 严格解析车位号：严禁通过枪号下标推测。若既无显式 space_id 且未配置映射字典，则返回 Err
    pub fn resolve_space_id(
        &self,
        connector_id: &str,
        explicit_space_id: Option<&str>,
    ) -> Result<String, String> {
        if let Some(space_id) = explicit_space_id {
            let trimmed = space_id.trim();
            if !trimmed.is_empty() {
                return Ok(trimmed.to_string());
            }
        }
        if let Some(space_id) = self.mapping.get(connector_id) {
            return Ok(space_id.clone());
        }
        Err(format!(
            "拒绝推测车位映射：枪号 {} 未在 ConnectorSpaceMapping 配置且无显式 spaceId",
            connector_id
        ))
    }
}

/// 充电运营商开放平台连接配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperatorApiConfig {
    pub base_url: String,
    pub operator_id: String,
    pub operator_secret: String,
    pub station_id: String,
}

impl OperatorApiConfig {
    pub fn from_env() -> Option<Self> {
        let base_url = std::env::var("OPERATOR_API_URL").ok()?;
        let operator_id = std::env::var("OPERATOR_ID").unwrap_or_else(|_| "DEMO_OPERATOR".into());
        let operator_secret = std::env::var("OPERATOR_SECRET").unwrap_or_default();
        let station_id =
            std::env::var("OPERATOR_STATION_ID").unwrap_or_else(|_| "STATION_001".into());

        Some(Self {
            base_url,
            operator_id,
            operator_secret,
            station_id,
        })
    }
}

/// 运营商开放接口返回的单枪状态标准模型
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectorStatusItem {
    pub connector_id: String,
    pub space_id: Option<String>,
    pub status: u8, // 1:空闲, 2:占用未充, 3:充电中, 4:充完滞留, 255:故障, 0:离线
    pub current_a: Option<f64>,
    pub voltage_v: Option<f64>,
    pub soc: Option<u8>,
    pub raw_timestamp: Option<DateTime<Utc>>,
    #[serde(default)]
    pub is_simulated: bool,
}

impl ConnectorStatusItem {
    pub fn to_charger_status(&self) -> ChargerStatus {
        match self.status {
            1 => ChargerStatus::Idle,
            2 => ChargerStatus::Preparing,
            3 => ChargerStatus::Charging,
            4 => ChargerStatus::Completed,
            255 => ChargerStatus::Fault,
            0 => ChargerStatus::Offline,
            _ => ChargerStatus::Unknown,
        }
    }
}

/// 状态与数据陈旧度诊断算法
pub fn calculate_staleness(
    raw_timestamp: Option<DateTime<Utc>>,
    status: &ChargerStatus,
    now: DateTime<Utc>,
    threshold_seconds: u64,
) -> StalenessStatus {
    match status {
        ChargerStatus::Offline => StalenessStatus::Disconnected,
        ChargerStatus::Fault => StalenessStatus::Fault,
        ChargerStatus::Completed => StalenessStatus::Completed,
        _ => {
            if let Some(raw_ts) = raw_timestamp {
                let diff = (now - raw_ts).num_seconds();
                if diff > threshold_seconds as i64 {
                    StalenessStatus::Stale(diff.max(0) as u64)
                } else {
                    StalenessStatus::Fresh
                }
            } else {
                StalenessStatus::Fresh
            }
        }
    }
}

/// 充电运营商云平台客户端（真实网络接口）
pub struct OperatorCloudClient {
    config: OperatorApiConfig,
    mapping: ConnectorSpaceMapping,
    staleness_threshold_seconds: u64,
    http: reqwest::Client,
}

impl OperatorCloudClient {
    pub fn new(config: OperatorApiConfig) -> Self {
        let mapping = ConnectorSpaceMapping::from_env();
        Self::with_mapping(config, mapping, 60)
    }

    pub fn with_mapping(
        config: OperatorApiConfig,
        mapping: ConnectorSpaceMapping,
        staleness_threshold_seconds: u64,
    ) -> Self {
        Self {
            config,
            mapping,
            staleness_threshold_seconds,
            http: reqwest::Client::new(),
        }
    }

    /// 拉取充电站各车位/桩的实时状态并同步到 DynamicChargerAdapter
    pub async fn sync_to_adapter(
        &self,
        adapter: &Arc<DynamicChargerAdapter>,
    ) -> Result<usize, String> {
        let url = format!(
            "{}/api/v1/stations/{}/connectors/status",
            self.config.base_url.trim_end_matches('/'),
            self.config.station_id
        );

        let response = self
            .http
            .get(&url)
            .header("X-Operator-ID", &self.config.operator_id)
            .header("X-Operator-Secret", &self.config.operator_secret)
            .send()
            .await
            .map_err(|e| format!("运营商 API 网络请求异常: {}", e))?;

        if !response.status().is_success() {
            return Err(format!("运营商 API 返回状态码: {}", response.status()));
        }

        let items = response
            .json::<Vec<ConnectorStatusItem>>()
            .await
            .map_err(|e| format!("解析运营商桩位状态响应失败: {}", e))?;

        let mut updated = 0;
        let now = Utc::now();
        for item in items {
            let space_id = match self
                .mapping
                .resolve_space_id(&item.connector_id, item.space_id.as_deref())
            {
                Ok(s) => s,
                Err(err) => {
                    eprintln!("[WARN] [OPERATOR_SYNC] {}", err);
                    continue;
                }
            };
            let status = item.to_charger_status();
            let completed_at = if status == ChargerStatus::Completed {
                Some(now)
            } else {
                None
            };

            let staleness_status = calculate_staleness(
                item.raw_timestamp,
                &status,
                now,
                self.staleness_threshold_seconds,
            );

            let update = ExternalChargerUpdate {
                charger_id: item.connector_id,
                space_id,
                status,
                session_id: None,
                started_at: None,
                completed_at,
                provider: Some("operator-cloud-api".into()),
                raw_timestamp: item.raw_timestamp,
                staleness_status: Some(staleness_status),
                is_simulated: item.is_simulated,
            };

            if adapter.update(update).is_ok() {
                updated += 1;
            }
        }

        Ok(updated)
    }
}

/// 仿真虚拟运营商云平台客户端（用于本地测试回放，显式标注模拟数据）
pub struct MockOperatorCloudClient {
    mapping: ConnectorSpaceMapping,
    staleness_threshold_seconds: u64,
    mock_items: std::sync::Mutex<Vec<ConnectorStatusItem>>,
}

impl MockOperatorCloudClient {
    pub fn new(mapping: ConnectorSpaceMapping) -> Self {
        Self {
            mapping,
            staleness_threshold_seconds: 60,
            mock_items: std::sync::Mutex::new(Vec::new()),
        }
    }

    pub fn set_mock_items(&self, items: Vec<ConnectorStatusItem>) {
        if let Ok(mut list) = self.mock_items.lock() {
            *list = items;
        }
    }

    pub fn sync_to_adapter(&self, adapter: &Arc<DynamicChargerAdapter>) -> Result<usize, String> {
        let items = self.mock_items.lock().map_err(|e| e.to_string())?.clone();
        let now = Utc::now();
        let mut updated = 0;
        for mut item in items {
            // 显式打标为模拟数据
            item.is_simulated = true;
            let space_id = match self
                .mapping
                .resolve_space_id(&item.connector_id, item.space_id.as_deref())
            {
                Ok(s) => s,
                Err(err) => {
                    eprintln!("[WARN] [MOCK_OPERATOR_SYNC] {}", err);
                    continue;
                }
            };
            let status = item.to_charger_status();
            let completed_at = if status == ChargerStatus::Completed {
                Some(now)
            } else {
                None
            };
            let staleness_status = calculate_staleness(
                item.raw_timestamp,
                &status,
                now,
                self.staleness_threshold_seconds,
            );

            let update = ExternalChargerUpdate {
                charger_id: item.connector_id,
                space_id,
                status,
                session_id: None,
                started_at: None,
                completed_at,
                provider: Some("[MOCK_SIMULATED] 虚拟运营商云平台".into()),
                raw_timestamp: item.raw_timestamp,
                staleness_status: Some(staleness_status),
                is_simulated: true,
            };

            if adapter.update(update).is_ok() {
                updated += 1;
            }
        }
        Ok(updated)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ChargerAdapter;
    use chrono::Duration;

    #[test]
    fn test_connector_status_mapping() {
        let item_charging = ConnectorStatusItem {
            connector_id: "chg_01".into(),
            space_id: Some("A-01".into()),
            status: 3,
            current_a: Some(32.0),
            voltage_v: Some(380.0),
            soc: Some(75),
            raw_timestamp: Some(Utc::now()),
            is_simulated: false,
        };
        assert_eq!(item_charging.to_charger_status(), ChargerStatus::Charging);

        let item_completed = ConnectorStatusItem {
            status: 4,
            ..item_charging.clone()
        };
        assert_eq!(item_completed.to_charger_status(), ChargerStatus::Completed);

        let item_fault = ConnectorStatusItem {
            status: 255,
            ..item_charging.clone()
        };
        assert_eq!(item_fault.to_charger_status(), ChargerStatus::Fault);
    }

    #[test]
    fn test_connector_space_mapping_strict_resolution() {
        let mut mapping = ConnectorSpaceMapping::new();
        mapping.insert("gun-01", "A-01");
        mapping.insert("gun-02", "A-02");

        // 1. 显式配置的映射能够成功解析
        assert_eq!(mapping.resolve_space_id("gun-01", None).unwrap(), "A-01");

        // 2. 带显式 space_id 的优先使用
        assert_eq!(
            mapping.resolve_space_id("gun-01", Some("B-99")).unwrap(),
            "B-99"
        );

        // 3. 未配置映射且无显式 space_id 的，必须严格拒绝，严禁通过下标等启发式猜测
        let err = mapping.resolve_space_id("gun-99", None);
        assert!(err.is_err());
        assert!(err.unwrap_err().contains("拒绝推测车位映射"));
    }

    #[test]
    fn test_staleness_status_calculation() {
        let now = Utc::now();

        // 实时新鲜数据
        let fresh_st = calculate_staleness(
            Some(now - Duration::seconds(10)),
            &ChargerStatus::Charging,
            now,
            60,
        );
        assert_eq!(fresh_st, StalenessStatus::Fresh);

        // 超过 60 秒的陈旧数据
        let stale_st = calculate_staleness(
            Some(now - Duration::seconds(95)),
            &ChargerStatus::Charging,
            now,
            60,
        );
        assert_eq!(stale_st, StalenessStatus::Stale(95));

        // 离线状态
        let offline_st = calculate_staleness(Some(now), &ChargerStatus::Offline, now, 60);
        assert_eq!(offline_st, StalenessStatus::Disconnected);
    }

    #[test]
    fn test_mock_operator_cloud_client_sync() {
        let mut mapping = ConnectorSpaceMapping::new();
        mapping.insert("mock-gun-1", "A-01");

        let mock_client = MockOperatorCloudClient::new(mapping);
        let now = Utc::now();
        mock_client.set_mock_items(vec![ConnectorStatusItem {
            connector_id: "mock-gun-1".into(),
            space_id: None,
            status: 3,
            current_a: Some(25.0),
            voltage_v: Some(380.0),
            soc: Some(60),
            raw_timestamp: Some(now),
            is_simulated: true,
        }]);

        let adapter = Arc::new(DynamicChargerAdapter::new(60));
        let count = mock_client.sync_to_adapter(&adapter).unwrap();
        assert_eq!(count, 1);

        let snap = adapter.snapshot("A-01").unwrap();
        assert_eq!(snap.status, ChargerStatus::Charging);
        assert!(snap.is_simulated);
        assert!(snap.provider.contains("[MOCK_SIMULATED]"));
    }
}
