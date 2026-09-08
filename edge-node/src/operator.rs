use crate::{ChargerStatus, DynamicChargerAdapter, ExternalChargerUpdate};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

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
        let station_id = std::env::var("OPERATOR_STATION_ID").unwrap_or_else(|_| "STATION_001".into());

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
pub struct ConnectorStatusItem {
    pub connector_id: String,
    pub space_id: Option<String>,
    pub status: u8, // 1:空闲, 2:占用未充, 3:充电中, 4:充完滞留, 255:故障, 0:离线
    pub current_a: Option<f64>,
    pub voltage_v: Option<f64>,
    pub soc: Option<u8>,
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

/// 充电运营商云平台客户端
pub struct OperatorCloudClient {
    config: OperatorApiConfig,
    http: reqwest::Client,
}

impl OperatorCloudClient {
    pub fn new(config: OperatorApiConfig) -> Self {
        Self {
            config,
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
            let space_id = item.space_id.clone().unwrap_or_else(|| format!("A-{}", &item.connector_id[..2.min(item.connector_id.len())]));
            let status = item.to_charger_status();
            let completed_at = if status == ChargerStatus::Completed {
                Some(now)
            } else {
                None
            };

            let update = ExternalChargerUpdate {
                charger_id: item.connector_id,
                space_id,
                status,
                session_id: None,
                started_at: None,
                completed_at,
                provider: Some("operator-cloud-api".into()),
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

    #[test]
    fn test_connector_status_mapping() {
        let item_charging = ConnectorStatusItem {
            connector_id: "chg_01".into(),
            space_id: Some("A-01".into()),
            status: 3,
            current_a: Some(32.0),
            voltage_v: Some(380.0),
            soc: Some(75),
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
}
