use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// 标准 S3 / OSS 对象存储连接配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct S3StorageConfig {
    pub endpoint: String,
    pub bucket: String,
    pub access_key: String,
    pub secret_key: String,
    pub region: String,
    pub public_base_url: Option<String>,
}

impl S3StorageConfig {
    pub fn from_env() -> Option<Self> {
        let endpoint = std::env::var("S3_ENDPOINT").ok()?;
        let bucket = std::env::var("S3_BUCKET").ok()?;
        let access_key = std::env::var("S3_ACCESS_KEY").unwrap_or_default();
        let secret_key = std::env::var("S3_SECRET_KEY").unwrap_or_default();
        let region = std::env::var("S3_REGION").unwrap_or_else(|_| "cn-hangzhou".into());
        let public_base_url = std::env::var("S3_PUBLIC_BASE_URL").ok();

        Some(Self {
            endpoint,
            bucket,
            access_key,
            secret_key,
            region,
            public_base_url,
        })
    }
}

/// 证据文件存储客户端（优先 S3/OSS 上传，无配置时本地磁盘回退）
pub struct EvidenceStorage {
    config: Option<S3StorageConfig>,
    local_dir: PathBuf,
}

impl EvidenceStorage {
    pub fn new(local_dir: impl AsRef<Path>) -> Self {
        let config = S3StorageConfig::from_env();
        let local_dir = local_dir.as_ref().to_path_buf();
        let _ = std::fs::create_dir_all(&local_dir);
        Self { config, local_dir }
    }

    /// 上传违规抓拍图片证据并返回访问 URL
    pub async fn upload_evidence(
        &self,
        space_id: &str,
        image_bytes: &[u8],
        extension: &str,
    ) -> Result<String, String> {
        let now = Utc::now();
        let file_name = format!(
            "{}_{}_{}.{}",
            space_id,
            now.format("%Y%m%d_%H%M%S"),
            &Uuid::new_v4().to_string()[..8],
            extension.trim_start_matches('.')
        );

        if let Some(cfg) = &self.config {
            // 真实公有云 S3 / OSS 上传
            let key = format!("evidences/{}/{}", now.format("%Y/%m/%d"), file_name);
            let target_url = format!(
                "https://{}.{}/{}",
                cfg.bucket,
                cfg.endpoint.trim_start_matches("https://").trim_start_matches("http://"),
                key
            );

            // 通过 HTTP PUT 上传图片数据
            let client = reqwest::Client::new();
            let res = client
                .put(&target_url)
                .header("Content-Type", "image/jpeg")
                .body(image_bytes.to_vec())
                .send()
                .await
                .map_err(|e| format!("S3 上传请求失败: {}", e))?;

            if res.status().is_success() {
                let public_url = cfg
                    .public_base_url
                    .as_ref()
                    .map(|base| format!("{}/{}", base.trim_end_matches('/'), key))
                    .unwrap_or(target_url);
                return Ok(public_url);
            }
        }

        // 本地磁盘保存回退
        let local_path = self.local_dir.join(&file_name);
        tokio::fs::write(&local_path, image_bytes)
            .await
            .map_err(|e| format!("写入本地证据文件失败: {}", e))?;

        Ok(format!("/evidences/{}", file_name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_local_evidence_fallback() {
        let tmp = std::env::temp_dir().join(format!("charging-evidence-{}", Uuid::new_v4()));
        let storage = EvidenceStorage::new(&tmp);
        let dummy_image = b"\xFF\xD8\xFF\xE0\x00\x10JFIF"; // 模拟 JPEG 文件头
        let url = storage.upload_evidence("A-01", dummy_image, "jpg").await.unwrap();
        assert!(url.starts_with("/evidences/A-01_"));
        let _ = std::fs::remove_dir_all(tmp);
    }
}
