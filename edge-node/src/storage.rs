use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use uuid::Uuid;

type HmacSha256 = Hmac<Sha256>;

/// 计算数据的标准 SHA-256 小写 16 进制字符串（防篡改凭证与 V4 签名摘要）
pub fn compute_sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    let result = hasher.finalize();
    let mut s = String::with_capacity(64);
    for b in result {
        use std::fmt::Write;
        let _ = write!(s, "{:02x}", b);
    }
    s
}

/// HMAC-SHA256 基础运算
pub fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC-SHA256 accepts any key length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

/// 纯 Rust AWS/OSS V4 签名计算（零外部库依赖）
pub fn compute_v4_signature(
    secret_key: &str,
    date_stamp: &str, // 例如 "20260908"
    region: &str,     // 例如 "cn-hangzhou"
    service: &str,    // 例如 "s3"
    string_to_sign: &str,
) -> String {
    let k_secret = format!("AWS4{}", secret_key);
    let k_date = hmac_sha256(k_secret.as_bytes(), date_stamp.as_bytes());
    let k_region = hmac_sha256(&k_date, region.as_bytes());
    let k_service = hmac_sha256(&k_region, service.as_bytes());
    let k_signing = hmac_sha256(&k_service, b"aws4_request");
    let signature = hmac_sha256(&k_signing, string_to_sign.as_bytes());

    let mut sig_hex = String::with_capacity(64);
    for b in signature {
        use std::fmt::Write;
        let _ = write!(sig_hex, "{:02x}", b);
    }
    sig_hex
}

/// 违规抓拍证据 Sidecar 侧车元数据（同名 .json 归档，提供防篡改溯源证据链）
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EvidenceSidecarMetadata {
    pub file_name: String,
    pub space_id: String,
    pub plate_number: Option<String>,
    pub violation_type: Option<String>,
    pub captured_at: DateTime<Utc>,
    pub sha256_checksum: String,
    pub file_size_bytes: usize,
    pub storage_target: String,
    #[serde(default)]
    pub is_simulated: bool,
}

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

    /// 构造标准 S3 / OSS V4 HTTP PUT 认证请求头
    pub fn build_v4_headers(
        &self,
        key: &str,
        payload_bytes: &[u8],
        now: DateTime<Utc>,
    ) -> Vec<(String, String)> {
        let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();
        let date_stamp = now.format("%Y%m%d").to_string();
        let payload_hash = compute_sha256_hex(payload_bytes);

        let clean_endpoint = self
            .endpoint
            .trim_start_matches("https://")
            .trim_start_matches("http://");
        let host = format!("{}.{}", self.bucket, clean_endpoint);
        let canonical_uri = if key.starts_with('/') {
            key.to_string()
        } else {
            format!("/{}", key)
        };

        let canonical_headers = format!(
            "host:{}\nx-amz-content-sha256:{}\nx-amz-date:{}\n",
            host, payload_hash, amz_date
        );
        let signed_headers = "host;x-amz-content-sha256;x-amz-date";

        let canonical_request = format!(
            "PUT\n{}\n\n{}\n{}\n{}",
            canonical_uri, canonical_headers, signed_headers, payload_hash
        );
        let canonical_request_hash = compute_sha256_hex(canonical_request.as_bytes());

        let credential_scope = format!("{}/{}/s3/aws4_request", date_stamp, self.region);
        let string_to_sign = format!(
            "AWS4-HMAC-SHA256\n{}\n{}\n{}",
            amz_date, credential_scope, canonical_request_hash
        );

        let signature = compute_v4_signature(
            &self.secret_key,
            &date_stamp,
            &self.region,
            "s3",
            &string_to_sign,
        );

        let authorization = format!(
            "AWS4-HMAC-SHA256 Credential={}/{}, SignedHeaders={}, Signature={}",
            self.access_key, credential_scope, signed_headers, signature
        );

        vec![
            ("Host".into(), host),
            ("x-amz-date".into(), amz_date),
            ("x-amz-content-sha256".into(), payload_hash),
            ("Authorization".into(), authorization),
            ("Content-Type".into(), "image/jpeg".into()),
        ]
    }
}

/// 证据文件存储客户端（优先 S3/OSS 上传，无配置时本地磁盘回退，并支持侧车元数据与容量配额自动清理）
pub struct EvidenceStorage {
    config: Option<S3StorageConfig>,
    local_dir: PathBuf,
    max_storage_bytes: u64,
    retention_days: u32,
}

impl EvidenceStorage {
    pub fn new(local_dir: impl AsRef<Path>) -> Self {
        let max_storage_bytes = std::env::var("EDGE_EVIDENCE_MAX_BYTES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(5 * 1024 * 1024 * 1024); // 默认 5GB
        let retention_days = std::env::var("EDGE_EVIDENCE_RETENTION_DAYS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(30); // 默认 30 天

        Self::with_limits(local_dir, max_storage_bytes, retention_days)
    }

    pub fn with_limits(
        local_dir: impl AsRef<Path>,
        max_storage_bytes: u64,
        retention_days: u32,
    ) -> Self {
        let config = S3StorageConfig::from_env();
        let local_dir = local_dir.as_ref().to_path_buf();
        let _ = std::fs::create_dir_all(&local_dir);
        Self {
            config,
            local_dir,
            max_storage_bytes,
            retention_days,
        }
    }

    /// 执行本地证据目录配额自动清理（超期淘汰 + FIFO 容量淘汰）
    pub fn cleanup_local_quota(&self) -> Result<(usize, u64), String> {
        let read_dir = std::fs::read_dir(&self.local_dir)
            .map_err(|e| format!("读取本地证据目录失败: {}", e))?;

        let now = std::time::SystemTime::now();
        let max_age = std::time::Duration::from_secs(self.retention_days as u64 * 86400);

        let mut entries = Vec::new();
        let mut total_bytes: u64 = 0;
        let mut deleted_count = 0;
        let mut freed_bytes: u64 = 0;

        for entry in read_dir.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            if let Ok(meta) = entry.metadata() {
                let size = meta.len();
                let modified = meta.modified().unwrap_or(now);

                // 1. 超期留存按天自动清理
                if let Ok(age) = now.duration_since(modified) {
                    if age > max_age {
                        let _ = std::fs::remove_file(&path);
                        deleted_count += 1;
                        freed_bytes += size;
                        continue;
                    }
                }

                total_bytes += size;
                entries.push((path, size, modified));
            }
        }

        // 2. FIFO 容量上限淘汰
        if total_bytes > self.max_storage_bytes {
            // 按修改时间升序排列（最旧的排在前面）
            entries.sort_by_key(|(_, _, m)| *m);

            for (path, size, _) in entries {
                if total_bytes <= self.max_storage_bytes {
                    break;
                }
                if std::fs::remove_file(&path).is_ok() {
                    total_bytes = total_bytes.saturating_sub(size);
                    deleted_count += 1;
                    freed_bytes += size;
                }
            }
        }

        Ok((deleted_count, freed_bytes))
    }

    /// 上传违规抓拍图片证据（带上下文元数据并自动落盘 Sidecar JSON 侧车文件）
    pub async fn upload_evidence_with_context(
        &self,
        space_id: &str,
        image_bytes: &[u8],
        extension: &str,
        plate_number: Option<&str>,
        violation_type: Option<&str>,
        is_simulated: bool,
    ) -> Result<(String, EvidenceSidecarMetadata), String> {
        let now = Utc::now();
        let ext = extension.trim_start_matches('.');
        let file_stem = format!(
            "{}_{}_{}",
            space_id,
            now.format("%Y%m%d_%H%M%S"),
            &Uuid::new_v4().to_string()[..8]
        );
        let file_name = format!("{}.{}", file_stem, ext);
        let sha256_checksum = compute_sha256_hex(image_bytes);

        let mut target_url = format!("/evidences/{}", file_name);
        let mut storage_target = "LOCAL_DISK".to_string();

        if let Some(cfg) = &self.config {
            let key = format!("evidences/{}/{}", now.format("%Y/%m/%d"), file_name);
            let s3_url = format!(
                "https://{}.{}/{}",
                cfg.bucket,
                cfg.endpoint
                    .trim_start_matches("https://")
                    .trim_start_matches("http://"),
                key
            );

            // 纯 Rust 构造标准 S3/OSS V4 签名认证头
            let headers = cfg.build_v4_headers(&key, image_bytes, now);
            let client = reqwest::Client::new();
            let mut req = client.put(&s3_url).body(image_bytes.to_vec());
            for (k, v) in headers {
                req = req.header(k, v);
            }

            if let Ok(res) = req.send().await {
                if res.status().is_success() {
                    target_url = cfg
                        .public_base_url
                        .as_ref()
                        .map(|base| format!("{}/{}", base.trim_end_matches('/'), key))
                        .unwrap_or(s3_url);
                    storage_target = format!("S3:{}/{}", cfg.bucket, key);
                }
            }
        }

        // 同步在本地保存图片备份
        let local_image_path = self.local_dir.join(&file_name);
        tokio::fs::write(&local_image_path, image_bytes)
            .await
            .map_err(|e| format!("写入本地证据图片失败: {}", e))?;

        // 写入同名 Sidecar .json 元数据文件（完整取证上下文与 SHA256 防篡改凭证）
        let metadata = EvidenceSidecarMetadata {
            file_name: file_name.clone(),
            space_id: space_id.to_string(),
            plate_number: plate_number.map(|s| s.to_string()),
            violation_type: violation_type.map(|s| s.to_string()),
            captured_at: now,
            sha256_checksum,
            file_size_bytes: image_bytes.len(),
            storage_target,
            is_simulated,
        };

        let sidecar_path = self.local_dir.join(format!("{}.json", file_stem));
        if let Ok(json_str) = serde_json::to_string_pretty(&metadata) {
            let _ = tokio::fs::write(&sidecar_path, json_str).await;
        }

        // 执行本地目录配额与留存清理
        let _ = self.cleanup_local_quota();

        Ok((target_url, metadata))
    }

    /// 上传违规抓拍图片证据并返回访问 URL（兼容原有接口）
    pub async fn upload_evidence(
        &self,
        space_id: &str,
        image_bytes: &[u8],
        extension: &str,
    ) -> Result<String, String> {
        self.upload_evidence_with_context(space_id, image_bytes, extension, None, None, false)
            .await
            .map(|(url, _)| url)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_local_evidence_fallback_and_sidecar() {
        let tmp = std::env::temp_dir().join(format!("charging-evidence-{}", Uuid::new_v4()));
        let storage = EvidenceStorage::new(&tmp);
        let dummy_image = b"\xFF\xD8\xFF\xE0\x00\x10JFIF_SAMPLE_IMAGE_PAYLOAD";
        let (url, meta) = storage
            .upload_evidence_with_context(
                "A-01",
                dummy_image,
                "jpg",
                Some("粤B12345"),
                Some("FUEL_VEHICLE"),
                false,
            )
            .await
            .unwrap();

        assert!(url.starts_with("/evidences/A-01_"));
        assert_eq!(meta.space_id, "A-01");
        assert_eq!(meta.plate_number.as_deref(), Some("粤B12345"));
        assert_eq!(meta.file_size_bytes, dummy_image.len());
        assert!(!meta.sha256_checksum.is_empty());

        // 验证同名 Sidecar JSON 文件已创建
        let sidecar_name = format!("{}.json", meta.file_name.trim_end_matches(".jpg"));
        let sidecar_path = tmp.join(sidecar_name);
        assert!(sidecar_path.exists());
        let content = std::fs::read_to_string(sidecar_path).unwrap();
        assert!(content.contains("粤B12345"));
        assert!(content.contains(&meta.sha256_checksum));

        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn test_v4_signature_generation() {
        let config = S3StorageConfig {
            endpoint: "oss-cn-hangzhou.aliyuncs.com".into(),
            bucket: "my-charging-bucket".into(),
            access_key: "TEST_AK".into(),
            secret_key: "TEST_SK".into(),
            region: "cn-hangzhou".into(),
            public_base_url: None,
        };

        let now = Utc::now();
        let payload = b"TEST_IMAGE_BYTES";
        let headers = config.build_v4_headers("evidences/test.jpg", payload, now);

        let auth = headers.iter().find(|(k, _)| k == "Authorization").unwrap();
        assert!(auth.1.starts_with("AWS4-HMAC-SHA256 Credential=TEST_AK/"));
        assert!(auth.1.contains("/cn-hangzhou/s3/aws4_request"));
        assert!(auth.1.contains("Signature="));
    }

    #[test]
    fn test_storage_quota_cleanup() {
        let tmp = std::env::temp_dir().join(format!("charging-quota-{}", Uuid::new_v4()));
        let _ = std::fs::create_dir_all(&tmp);

        // 设置配额为 150 字节，生成 3 个 100 字节的文件
        let storage = EvidenceStorage::with_limits(&tmp, 150, 30);
        let f1 = tmp.join("file1.dat");
        let f2 = tmp.join("file2.dat");
        let f3 = tmp.join("file3.dat");

        std::fs::write(&f1, vec![0u8; 100]).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        std::fs::write(&f2, vec![0u8; 100]).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        std::fs::write(&f3, vec![0u8; 100]).unwrap();

        // 300 字节 > 150 字节限额，必须 FIFO 淘汰最旧的文件
        let (deleted, freed) = storage.cleanup_local_quota().unwrap();
        assert!(deleted >= 1);
        assert!(freed >= 100);

        let _ = std::fs::remove_dir_all(tmp);
    }
}
