use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
    time::Instant,
};

/// 磁盘空间与水位指标
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiskWatermarkInfo {
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub available_bytes: u64,
    pub watermark_percent: f64,
    pub alert_threshold_percent: f64,
    pub is_critical: bool,
}

/// 边缘节点遥测与核心运行指标
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TelemetrySnapshot {
    pub node_id: String,
    pub uptime_seconds: u64,
    pub frames_processed_total: u64,
    pub violations_detected_total: u64,
    pub alarms_triggered_total: u64,
    pub sqlite_unuploaded_count: u64,
    pub disk: DiskWatermarkInfo,
    pub clock_skew_millis: i64,
    pub cpu_usage: f64,
    pub memory_usage: f64,
    pub temperature: f64,
    pub generated_at: DateTime<Utc>,
}

/// 工业边缘指标采集管理器
pub struct MetricsCollector {
    node_id: String,
    start_time: Instant,
    frames_processed: AtomicU64,
    violations_detected: AtomicU64,
    alarms_triggered: AtomicU64,
    sqlite_unuploaded: AtomicU64,
    clock_skew_millis: AtomicU64,
    data_dir_path: String,
    disk_total_capacity_bytes: u64,
    alert_threshold_percent: f64,
}

impl MetricsCollector {
    pub fn new(node_id: String, data_dir_path: String, disk_total_capacity_bytes: u64) -> Self {
        Self {
            node_id,
            start_time: Instant::now(),
            frames_processed: AtomicU64::new(0),
            violations_detected: AtomicU64::new(0),
            alarms_triggered: AtomicU64::new(0),
            sqlite_unuploaded: AtomicU64::new(0),
            clock_skew_millis: AtomicU64::new(0),
            data_dir_path,
            disk_total_capacity_bytes: if disk_total_capacity_bytes == 0 {
                10 * 1024 * 1024 * 1024 // 默认按 10GB 容量水位计算
            } else {
                disk_total_capacity_bytes
            },
            alert_threshold_percent: 90.0, // 90% 水位告警
        }
    }

    pub fn record_frame(&self) {
        self.frames_processed.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_violation(&self) {
        self.violations_detected.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_alarm(&self) {
        self.alarms_triggered.fetch_add(1, Ordering::Relaxed);
    }

    pub fn set_sqlite_unuploaded(&self, count: u64) {
        self.sqlite_unuploaded.store(count, Ordering::Relaxed);
    }

    pub fn set_clock_skew_millis(&self, millis: i64) {
        self.clock_skew_millis
            .store(millis.unsigned_abs(), Ordering::Relaxed);
    }

    /// 评估磁盘已用空间与水位百分比
    pub fn evaluate_disk(&self) -> DiskWatermarkInfo {
        let dir_path = Path::new(&self.data_dir_path);
        let used_bytes = calculate_dir_size(dir_path);
        let total = self.disk_total_capacity_bytes;
        let available = total.saturating_sub(used_bytes);
        let watermark = (used_bytes as f64 / total as f64) * 100.0;
        let is_critical = watermark >= self.alert_threshold_percent;

        DiskWatermarkInfo {
            total_bytes: total,
            used_bytes,
            available_bytes: available,
            watermark_percent: (watermark * 100.0).round() / 100.0,
            alert_threshold_percent: self.alert_threshold_percent,
            is_critical,
        }
    }

    /// 获取完整遥测快照
    pub fn snapshot(&self) -> TelemetrySnapshot {
        let disk = self.evaluate_disk();
        let (cpu_usage, memory_usage, temperature) = collect_system_metrics();
        TelemetrySnapshot {
            node_id: self.node_id.clone(),
            uptime_seconds: self.start_time.elapsed().as_secs(),
            frames_processed_total: self.frames_processed.load(Ordering::Relaxed),
            violations_detected_total: self.violations_detected.load(Ordering::Relaxed),
            alarms_triggered_total: self.alarms_triggered.load(Ordering::Relaxed),
            sqlite_unuploaded_count: self.sqlite_unuploaded.load(Ordering::Relaxed),
            disk,
            clock_skew_millis: self.clock_skew_millis.load(Ordering::Relaxed) as i64,
            cpu_usage,
            memory_usage,
            temperature,
            generated_at: Utc::now(),
        }
    }

    /// 导出 Prometheus 标准指标文本格式
    pub fn export_prometheus_text(&self) -> String {
        let snap = self.snapshot();
        format!(
            "# HELP chg_edge_uptime_seconds Process uptime in seconds\n\
             # TYPE chg_edge_uptime_seconds gauge\n\
             chg_edge_uptime_seconds{{node=\"{}\"}} {}\n\
             # HELP chg_edge_frames_processed_total Total video frames processed\n\
             # TYPE chg_edge_frames_processed_total counter\n\
             chg_edge_frames_processed_total{{node=\"{}\"}} {}\n\
             # HELP chg_edge_violations_detected_total Total violation events detected\n\
             # TYPE chg_edge_violations_detected_total counter\n\
             chg_edge_violations_detected_total{{node=\"{}\"}} {}\n\
             # HELP chg_edge_alarms_triggered_total Total relay alarms triggered\n\
             # TYPE chg_edge_alarms_triggered_total counter\n\
             chg_edge_alarms_triggered_total{{node=\"{}\"}} {}\n\
             # HELP chg_edge_sqlite_unuploaded_total SQLite pending events to upload\n\
             # TYPE chg_edge_sqlite_unuploaded_total gauge\n\
             chg_edge_sqlite_unuploaded_total{{node=\"{}\"}} {}\n\
             # HELP chg_edge_disk_watermark_ratio Disk usage watermark ratio\n\
             # TYPE chg_edge_disk_watermark_ratio gauge\n\
             chg_edge_disk_watermark_ratio{{node=\"{}\"}} {:.4}\n\
             # HELP chg_edge_disk_critical_alert Is disk watermark above threshold\n\
             # TYPE chg_edge_disk_critical_alert gauge\n\
             chg_edge_disk_critical_alert{{node=\"{}\"}} {}\n",
            snap.node_id,
            snap.uptime_seconds,
            snap.node_id,
            snap.frames_processed_total,
            snap.node_id,
            snap.violations_detected_total,
            snap.node_id,
            snap.alarms_triggered_total,
            snap.node_id,
            snap.sqlite_unuploaded_count,
            snap.node_id,
            (snap.disk.watermark_percent / 100.0),
            snap.node_id,
            if snap.disk.is_critical { 1 } else { 0 }
        )
    }
}

/// 真实物理系统资源探测（真实物理内存、真实CPU负载、真实核心温度）
pub fn collect_system_metrics() -> (f64, f64, f64) {
    // 1. 真实物理内存占用率（从 /proc/meminfo 读取）
    let mut mem_usage = 0.0;
    if let Ok(meminfo) = std::fs::read_to_string("/proc/meminfo") {
        let mut total = 0u64;
        let mut avail = 0u64;
        for line in meminfo.lines() {
            if line.starts_with("MemTotal:") {
                total = line.split_whitespace().nth(1).and_then(|v| v.parse().ok()).unwrap_or(0);
            } else if line.starts_with("MemAvailable:") {
                avail = line.split_whitespace().nth(1).and_then(|v| v.parse().ok()).unwrap_or(0);
            }
        }
        if total > 0 {
            mem_usage = ((total.saturating_sub(avail)) as f64 / total as f64) * 100.0;
            mem_usage = (mem_usage * 10.0).round() / 10.0;
        }
    }

    // 2. 真实 CPU 负载率（从 /proc/loadavg 读取，无法读取时保持 0.0）
    let mut cpu_usage = 0.0;
    if let Ok(loadavg) = std::fs::read_to_string("/proc/loadavg") {
        if let Some(load1) = loadavg
            .split_whitespace()
            .next()
            .and_then(|v| v.parse::<f64>().ok())
        {
            let cpus = std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1) as f64;
            cpu_usage = ((load1 / cpus) * 100.0).clamp(0.0, 100.0);
            cpu_usage = (cpu_usage * 10.0).round() / 10.0;
        }
    }

    // 3. 真实核心温度（优先扫描 Linux /sys/class/hwmon 与 /sys/class/thermal，无传感器时保持 0.0）
    let mut temp = 0.0;
    if let Ok(hwmons) = std::fs::read_dir("/sys/class/hwmon") {
        'hwmon_scan: for hw in hwmons.flatten() {
            let hw_path = hw.path();
            for idx in 1..=8 {
                let temp_path = hw_path.join(format!("temp{}_input", idx));
                if let Ok(temp_str) = std::fs::read_to_string(&temp_path) {
                    if let Ok(raw) = temp_str.trim().parse::<f64>() {
                        let deg = if raw > 1000.0 { raw / 1000.0 } else { raw };
                        if (10.0..120.0).contains(&deg) {
                            temp = (deg * 10.0).round() / 10.0;
                            break 'hwmon_scan;
                        }
                    }
                }
            }
        }
    }
    if temp <= 0.0 {
        for zone in 0..8 {
            let path = format!("/sys/class/thermal/thermal_zone{}/temp", zone);
            if let Ok(temp_str) = std::fs::read_to_string(&path) {
                if let Ok(raw) = temp_str.trim().parse::<f64>() {
                    let deg = if raw > 1000.0 { raw / 1000.0 } else { raw };
                    if (10.0..120.0).contains(&deg) {
                        temp = (deg * 10.0).round() / 10.0;
                        break;
                    }
                }
            }
        }
    }

    (cpu_usage, mem_usage, temp)
}

/// 递归计算目录下全部文件字节大小
fn calculate_dir_size(path: &Path) -> u64 {
    if !path.exists() {
        return 0;
    }
    if path.is_file() {
        return path.metadata().map(|m| m.len()).unwrap_or(0);
    }
    let mut total = 0;
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                total += calculate_dir_size(&p);
            } else if p.is_file() {
                total += entry.metadata().map(|m| m.len()).unwrap_or(0);
            }
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_metrics_collection_and_prometheus_export() {
        let temp_dir = std::env::temp_dir().join(format!("metrics_test_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&temp_dir).unwrap();

        // 写入 1MB 测试文件
        let test_file = temp_dir.join("test.bin");
        std::fs::write(&test_file, vec![0u8; 1024 * 1024]).unwrap();

        // 设定容量为 2MB，则 1MB 占用率为 50%
        let collector = MetricsCollector::new(
            "edge-node-test-01".to_string(),
            temp_dir.to_str().unwrap().to_string(),
            2 * 1024 * 1024,
        );

        collector.record_frame();
        collector.record_frame();
        collector.record_violation();
        collector.record_alarm();
        collector.set_sqlite_unuploaded(3);

        let snap = collector.snapshot();
        assert_eq!(snap.frames_processed_total, 2);
        assert_eq!(snap.violations_detected_total, 1);
        assert_eq!(snap.alarms_triggered_total, 1);
        assert_eq!(snap.sqlite_unuploaded_count, 3);
        assert!((snap.disk.watermark_percent - 50.0).abs() < 1.0);
        assert!(!snap.disk.is_critical);

        let prom = collector.export_prometheus_text();
        assert!(prom.contains("chg_edge_uptime_seconds"));
        assert!(prom.contains("chg_edge_frames_processed_total{node=\"edge-node-test-01\"} 2"));
        assert!(prom.contains("chg_edge_sqlite_unuploaded_total{node=\"edge-node-test-01\"} 3"));

        let _ = std::fs::remove_dir_all(&temp_dir);
    }
}
