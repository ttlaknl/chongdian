use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fs::OpenOptions,
    io::Write,
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

/// 看门狗配置项
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WatchdogConfig {
    pub device_path: String,
    pub feed_interval_seconds: u64,
    pub subsystem_timeout_seconds: i64,
    pub enabled: bool,
}

impl Default for WatchdogConfig {
    fn default() -> Self {
        Self {
            device_path: "/dev/watchdog".to_string(),
            feed_interval_seconds: 5,
            subsystem_timeout_seconds: 30,
            enabled: true,
        }
    }
}

/// 子系统监控项
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubsystemHealth {
    pub name: String,
    pub last_feed_at: DateTime<Utc>,
    pub is_alive: bool,
    pub latency_seconds: i64,
}

/// 看门狗总监控报告
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WatchdogReport {
    pub hardware_watchdog_active: bool,
    pub device_path: String,
    pub all_subsystems_healthy: bool,
    pub subsystems: Vec<SubsystemHealth>,
    pub last_tick_at: DateTime<Utc>,
}

/// 工业边缘工控机高可用看门狗守护管理器
pub struct WatchdogMonitor {
    config: WatchdogConfig,
    subsystems: Mutex<HashMap<String, DateTime<Utc>>>,
    hardware_active: AtomicBool,
    running: AtomicBool,
}

impl WatchdogMonitor {
    pub fn new(config: WatchdogConfig) -> Self {
        let mut map = HashMap::new();
        let now = Utc::now();
        map.insert("pipeline".to_string(), now);
        map.insert("hardware".to_string(), now);
        map.insert("operator".to_string(), now);
        map.insert("sqlite".to_string(), now);

        Self {
            config,
            subsystems: Mutex::new(map),
            hardware_active: AtomicBool::new(false),
            running: AtomicBool::new(false),
        }
    }

    /// 子系统心跳喂狗
    pub fn feed(&self, subsystem_name: &str) {
        if let Ok(mut map) = self.subsystems.lock() {
            map.insert(subsystem_name.to_string(), Utc::now());
        }
    }

    /// 执行一次硬件喂狗并检查所有子系统健康状态
    pub fn tick(&self) -> WatchdogReport {
        let now = Utc::now();
        let mut healthy = true;
        let mut list = Vec::new();

        if let Ok(map) = self.subsystems.lock() {
            for (name, &last) in map.iter() {
                let diff = (now - last).num_seconds();
                let is_alive = diff <= self.config.subsystem_timeout_seconds;
                if !is_alive {
                    healthy = false;
                }
                list.push(SubsystemHealth {
                    name: name.clone(),
                    last_feed_at: last,
                    is_alive,
                    latency_seconds: diff,
                });
            }
        }

        // 仅在全部核心子系统心跳正常时，才向硬件看门狗设备喂狗
        let mut hw_success = false;
        if healthy && self.config.enabled {
            if Path::new(&self.config.device_path).exists() {
                if let Ok(mut file) = OpenOptions::new().write(true).open(&self.config.device_path) {
                    if file.write_all(b"1").is_ok() {
                        hw_success = true;
                    }
                }
            }
        }
        self.hardware_active.store(hw_success, Ordering::SeqCst);

        WatchdogReport {
            hardware_watchdog_active: hw_success,
            device_path: self.config.device_path.clone(),
            all_subsystems_healthy: healthy,
            subsystems: list,
            last_tick_at: now,
        }
    }

    /// 启动后台周期性监控与喂狗协程
    pub fn start_loop(self: Arc<Self>) -> tokio::task::JoinHandle<()> {
        self.running.store(true, Ordering::SeqCst);
        let interval = Duration::from_secs(self.config.feed_interval_seconds);

        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            while self.running.load(Ordering::SeqCst) {
                ticker.tick().await;
                let _ = self.tick();
            }
        })
    }

    pub fn stop(&self) {
        self.running.store(false, Ordering::SeqCst);
    }
}

impl Default for WatchdogMonitor {
    fn default() -> Self {
        Self::new(WatchdogConfig::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_watchdog_feed_and_health_check() {
        let config = WatchdogConfig {
            device_path: "/dev/null".to_string(),
            feed_interval_seconds: 1,
            subsystem_timeout_seconds: 2,
            enabled: false,
        };
        let watchdog = WatchdogMonitor::new(config);

        // 初始化时心跳正常
        let report = watchdog.tick();
        assert!(report.all_subsystems_healthy);
        assert_eq!(report.subsystems.len(), 4);

        // 模拟喂狗
        watchdog.feed("pipeline");
        watchdog.feed("hardware");
        let report2 = watchdog.tick();
        assert!(report2.all_subsystems_healthy);
    }
}
