use crate::storage::compute_sha256_hex;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{fs, path::PathBuf, sync::Mutex};

/// 远程 OTA 升级清单
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpgradeManifest {
    pub version: u64,
    pub version_name: String,
    pub package_url: String,
    pub sha256_hash: String,
    pub signature: String,
    pub min_compatible_version: u64,
    pub release_notes: String,
    pub created_at: DateTime<Utc>,
}

/// 升级状态机流转
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum UpgradeState {
    Idle,
    Downloading,
    Verifying,
    Staged,
    HealthChecking,
    Committed,
    RolledBack,
}

/// 升级执行进度报告
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpgradeReport {
    pub current_version: u64,
    pub target_version: Option<u64>,
    pub state: UpgradeState,
    pub message: String,
    pub last_error: Option<String>,
    pub updated_at: DateTime<Utc>,
}

/// 边缘节点远程升级与双版本自愈回滚管理器
pub struct RemoteUpgradeManager {
    current_version: Mutex<u64>,
    shared_secret: String,
    binary_path: PathBuf,
    report: Mutex<UpgradeReport>,
}

impl RemoteUpgradeManager {
    pub fn new(current_version: u64, shared_secret: String, binary_path: PathBuf) -> Self {
        let initial_report = UpgradeReport {
            current_version,
            target_version: None,
            state: UpgradeState::Idle,
            message: "系统当前运行在稳定基线版本".to_string(),
            last_error: None,
            updated_at: Utc::now(),
        };

        Self {
            current_version: Mutex::new(current_version),
            shared_secret,
            binary_path,
            report: Mutex::new(initial_report),
        }
    }

    /// 获取当前运行固件版本号
    pub fn get_current_version(&self) -> u64 {
        *self.current_version.lock().unwrap()
    }

    /// 获取当前升级报告
    pub fn get_report(&self) -> UpgradeReport {
        self.report.lock().unwrap().clone()
    }

    /// 校验升级包数字签名 (HMAC-SHA256) 与 SHA-256 哈希完整性
    pub fn verify_package(
        &self,
        manifest: &UpgradeManifest,
        package_data: &[u8],
    ) -> Result<(), String> {
        // 1. 版本兼容性检查
        let cur_ver = *self.current_version.lock().unwrap();
        if manifest.version <= cur_ver {
            return Err(format!(
                "目标版本 (v{}) 必须高于当前运行版本 (v{})",
                manifest.version, cur_ver
            ));
        }
        if cur_ver < manifest.min_compatible_version {
            return Err(format!(
                "当前版本 (v{}) 低于最低升级依赖版本 (v{})",
                cur_ver, manifest.min_compatible_version
            ));
        }

        // 2. SHA-256 数据完整性校验
        let computed_sha256 = compute_sha256_hex(package_data);
        if !computed_sha256.eq_ignore_ascii_case(&manifest.sha256_hash) {
            return Err(format!(
                "升级包 SHA-256 校验失败: 期望={}, 实际={}",
                manifest.sha256_hash, computed_sha256
            ));
        }

        // 3. 数字签名验签
        let sign_payload = format!(
            "{}:{}:{}",
            manifest.version, manifest.sha256_hash, manifest.package_url
        );
        if self.shared_secret.is_empty() {
            return Err("升级验签密钥未配置，拒绝执行升级".to_string());
        }
        if manifest.signature.is_empty()
            || !verify_hmac_sha256_hex(&self.shared_secret, &sign_payload, &manifest.signature)
        {
            return Err("升级包数字签名非法，拒绝应用不可信版本".to_string());
        }

        Ok(())
    }

    /// 执行就地升级、自检探针与失败自动原子回滚
    pub fn apply_upgrade<F>(
        &self,
        manifest: &UpgradeManifest,
        package_data: &[u8],
        health_check: F,
    ) -> Result<UpgradeReport, String>
    where
        F: Fn() -> Result<(), String>,
    {
        // 1. 验证
        self.update_status(
            Some(manifest.version),
            UpgradeState::Verifying,
            "正在校验固件签名与哈希...".into(),
            None,
        );
        if let Err(err) = self.verify_package(manifest, package_data) {
            self.update_status(
                Some(manifest.version),
                UpgradeState::RolledBack,
                "升级包校验失败，终止升级".into(),
                Some(err.clone()),
            );
            return Err(err);
        }

        // 2. 备份当前执行文件 (A/B 分区回滚安全保障)
        let backup_path = self.binary_path.with_extension("backup");
        if self.binary_path.exists() {
            if let Err(e) = fs::copy(&self.binary_path, &backup_path) {
                let err_msg = format!("备份当前版本二进制失败: {}", e);
                self.update_status(
                    Some(manifest.version),
                    UpgradeState::RolledBack,
                    "备份当前版本失败，升级放弃".into(),
                    Some(err_msg.clone()),
                );
                return Err(err_msg);
            }
        }

        // 3. 部署新固件 (Staging via atomic rename to avoid Linux ETXTBSY on running binary)
        self.update_status(
            Some(manifest.version),
            UpgradeState::Staged,
            "固件就绪，正在写入新程序文件...".into(),
            None,
        );
        let staged_path = self.binary_path.with_extension("new");
        let write_and_stage = || -> Result<(), std::io::Error> {
            fs::write(&staged_path, package_data)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = fs::set_permissions(&staged_path, fs::Permissions::from_mode(0o755));
            }
            // Linux 下通过 rename 原子替换正在运行的可执行文件，不会触发 ETXTBSY
            fs::rename(&staged_path, &self.binary_path)?;
            Ok(())
        };

        if let Err(e) = write_and_stage() {
            let _ = fs::remove_file(&staged_path);
            // 写入或重命名失败恢复备份
            if backup_path.exists() {
                let _ = fs::copy(&backup_path, &self.binary_path);
                let _ = fs::remove_file(&backup_path);
            }
            let err_msg = format!("写入新版本固件失败: {}", e);
            self.update_status(
                Some(manifest.version),
                UpgradeState::RolledBack,
                "写入失败，已回退至原版本".into(),
                Some(err_msg.clone()),
            );
            return Err(err_msg);
        }

        // 4. 执行新版本自检探针 (Health Check)
        self.update_status(
            Some(manifest.version),
            UpgradeState::HealthChecking,
            "新版本部署成功，正在执行系统自检...".into(),
            None,
        );
        match health_check() {
            Ok(()) => {
                // 自检通过，提交生效并清理备份文件
                if backup_path.exists() {
                    let _ = fs::remove_file(&backup_path);
                }
                *self.current_version.lock().unwrap() = manifest.version;
                // 持久化当前生效版本号至同目录或数据目录
                let version_file = self.binary_path.with_extension("version");
                let _ = fs::write(&version_file, manifest.version.to_string());
                let report = self.update_status(
                    Some(manifest.version),
                    UpgradeState::Committed,
                    format!("版本 v{} 升级成功并已提交生效", manifest.version),
                    None,
                );
                Ok(report)
            }
            Err(reason) => {
                // 自检失败！立即原子回滚至备份版本
                if backup_path.exists() {
                    let _ = fs::copy(&backup_path, &self.binary_path);
                    let _ = fs::remove_file(&backup_path);
                }
                let err_msg = format!(
                    "自检失败 ({}): 已自动触发版本回滚，系统恢复上一稳定版本",
                    reason
                );
                let _report = self.update_status(
                    Some(manifest.version),
                    UpgradeState::RolledBack,
                    "新版本自检未通过，已自动执行安全回滚".into(),
                    Some(err_msg.clone()),
                );
                Err(err_msg)
            }
        }
    }

    fn update_status(
        &self,
        target_version: Option<u64>,
        state: UpgradeState,
        message: String,
        last_error: Option<String>,
    ) -> UpgradeReport {
        let cur = *self.current_version.lock().unwrap();
        let mut rep = self.report.lock().unwrap();
        rep.current_version = cur;
        rep.target_version = target_version;
        rep.state = state;
        rep.message = message;
        rep.last_error = last_error;
        rep.updated_at = Utc::now();
        rep.clone()
    }
}

/// 计算 HMAC-SHA256 签名
pub fn compute_hmac_sha256_hex(secret: &str, data: &str) -> String {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    type HmacSha256 = Hmac<Sha256>;

    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC can take any key size");
    mac.update(data.as_bytes());
    let result = mac.finalize().into_bytes();
    let mut hex = String::with_capacity(64);
    for b in result {
        use std::fmt::Write;
        let _ = write!(hex, "{:02x}", b);
    }
    hex
}

fn verify_hmac_sha256_hex(secret: &str, data: &str, signature: &str) -> bool {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    type HmacSha256 = Hmac<Sha256>;

    let bytes = match (0..signature.len())
        .step_by(2)
        .map(|index| {
            signature
                .get(index..index + 2)
                .ok_or(())
                .and_then(|pair| u8::from_str_radix(pair, 16).map_err(|_| ()))
        })
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(bytes) if bytes.len() == 32 => bytes,
        _ => return false,
    };
    let mut mac = match HmacSha256::new_from_slice(secret.as_bytes()) {
        Ok(mac) => mac,
        Err(_) => return false,
    };
    mac.update(data.as_bytes());
    mac.verify_slice(&bytes).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_remote_upgrade_and_rollback_flow() {
        let temp_dir = std::env::temp_dir();
        let bin_path = temp_dir.join(format!("test_app_{}.bin", uuid::Uuid::new_v4()));
        fs::write(&bin_path, b"ORIGINAL_BINARY_V1").unwrap();

        let secret = "test_upgrade_key_8888".to_string();
        let manager = RemoteUpgradeManager::new(1, secret.clone(), bin_path.clone());

        let new_package = b"NEW_BINARY_V2_FIRMWARE";
        let pkg_sha256 = compute_sha256_hex(new_package);
        let sign_payload = format!("2:{}:https://firmware/v2.bin", pkg_sha256);
        let signature = compute_hmac_sha256_hex(&secret, &sign_payload);

        let manifest = UpgradeManifest {
            version: 2,
            version_name: "2.0.0-release".to_string(),
            package_url: "https://firmware/v2.bin".to_string(),
            sha256_hash: pkg_sha256,
            signature,
            min_compatible_version: 1,
            release_notes: "新增16路并发自愈支持".to_string(),
            created_at: Utc::now(),
        };

        // 场景 1: 自检探针失败，验证自动回滚并保持原内容
        let rollback_res = manager.apply_upgrade(&manifest, new_package, || {
            Err("SQLite 数据库完整性校验失败".to_string())
        });
        assert!(rollback_res.is_err());
        assert_eq!(manager.get_report().state, UpgradeState::RolledBack);
        // 原文件内容未被破坏
        assert_eq!(fs::read(&bin_path).unwrap(), b"ORIGINAL_BINARY_V1");

        // 场景 2: 自检通过，升级提交生效
        let success_res = manager.apply_upgrade(&manifest, new_package, || Ok(()));
        assert!(success_res.is_ok());
        assert_eq!(manager.get_report().state, UpgradeState::Committed);
        assert_eq!(manager.get_report().current_version, 2);
        assert_eq!(fs::read(&bin_path).unwrap(), b"NEW_BINARY_V2_FIRMWARE");

        let _ = fs::remove_file(&bin_path);
    }
}
