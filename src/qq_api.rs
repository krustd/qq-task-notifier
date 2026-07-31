use std::{
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use md5::Md5;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::Digest;
use tokio::{
    io::AsyncReadExt,
    sync::{Mutex, RwLock},
};

use crate::media::{MediaType, file_digests};

const QQ_API_BASE: &str = "https://api.sgroup.qq.com";
const QQ_TOKEN_URL: &str = "https://bots.qq.com/app/getAppAccessToken";
#[derive(Clone)]
pub(crate) struct QQApi {
    app_id: String,
    app_secret: String,
    client: reqwest::Client,
    token: Arc<RwLock<Option<AccessToken>>>,
    refresh_lock: Arc<Mutex<()>>,
}

struct AccessToken {
    value: String,
    expires_at: Instant,
}
#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    expires_in: Value,
}

#[derive(Deserialize)]
struct GatewayResponse {
    url: String,
}
#[derive(Deserialize)]
struct UploadPrepareResponse {
    upload_id: String,
    parts: Vec<UploadPart>,
}

#[derive(Deserialize)]
struct UploadPart {
    index: u32,
    presigned_url: String,
    block_size: String,
}

#[derive(Deserialize)]
struct MediaUploadResponse {
    file_info: String,
}
impl QQApi {
    pub(crate) fn new(app_id: String, app_secret: String) -> Result<Self> {
        let client = reqwest::Client::builder()
            .https_only(true)
            .timeout(Duration::from_secs(20))
            .build()
            .context("无法创建 QQ HTTP 客户端")?;

        Ok(Self {
            app_id,
            app_secret,
            client,
            token: Arc::new(RwLock::new(None)),
            refresh_lock: Arc::new(Mutex::new(())),
        })
    }

    pub(crate) async fn authorization(&self) -> Result<String> {
        if let Some(value) = self.valid_token().await {
            return Ok(format!("QQBot {value}"));
        }

        let _refresh_guard = self.refresh_lock.lock().await;
        if let Some(value) = self.valid_token().await {
            return Ok(format!("QQBot {value}"));
        }

        let response = self
            .client
            .post(QQ_TOKEN_URL)
            .json(&json!({
                "appId": self.app_id,
                "clientSecret": self.app_secret,
            }))
            .send()
            .await
            .context("获取 QQ AppAccessToken 失败")?
            .error_for_status()
            .context("QQ AppAccessToken 请求返回错误")?
            .json::<TokenResponse>()
            .await
            .context("QQ AppAccessToken 响应格式无效")?;

        let expires_in = response
            .expires_in
            .as_u64()
            .or_else(|| {
                response
                    .expires_in
                    .as_str()
                    .and_then(|value| value.parse::<u64>().ok())
            })
            .ok_or_else(|| anyhow!("QQ AppAccessToken 响应缺少有效 expires_in"))?;
        let cache_for = Duration::from_secs(expires_in.saturating_sub(60).max(1));

        *self.token.write().await = Some(AccessToken {
            value: response.access_token,
            expires_at: Instant::now() + cache_for,
        });

        self.valid_token()
            .await
            .map(|value| format!("QQBot {value}"))
            .ok_or_else(|| anyhow!("QQ AppAccessToken 缓存失败"))
    }

    async fn valid_token(&self) -> Option<String> {
        self.token
            .read()
            .await
            .as_ref()
            .filter(|token| token.expires_at > Instant::now())
            .map(|token| token.value.clone())
    }

    pub(crate) async fn gateway_url(&self) -> Result<String> {
        let response = self
            .client
            .get(format!("{QQ_API_BASE}/gateway/bot"))
            .headers(self.headers().await?)
            .send()
            .await
            .context("查询 QQ Gateway 地址失败")?
            .error_for_status()
            .context("QQ Gateway 地址请求返回错误")?
            .json::<GatewayResponse>()
            .await
            .context("QQ Gateway 地址响应格式无效")?;
        Ok(response.url)
    }

    pub(crate) async fn send_c2c(
        &self,
        openid: &str,
        content: &str,
        msg_id: Option<&str>,
    ) -> Result<()> {
        let mut payload = json!({
            "msg_type": 0,
            "content": content,
        });
        if let Some(msg_id) = msg_id {
            payload["msg_id"] = Value::String(msg_id.to_owned());
            payload["msg_seq"] = Value::from(1);
        }

        let response = self
            .client
            .post(format!("{QQ_API_BASE}/v2/users/{openid}/messages"))
            .headers(self.headers().await?)
            .json(&payload)
            .send()
            .await
            .context("发送 QQ 私聊消息失败")?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            bail!("QQ 私聊消息接口返回 {status}: {body}");
        }
        Ok(())
    }

    pub(crate) async fn send_c2c_markdown(&self, openid: &str, content: &str) -> Result<()> {
        let response = self
            .client
            .post(format!("{QQ_API_BASE}/v2/users/{openid}/messages"))
            .headers(self.headers().await?)
            .json(&markdown_payload(content))
            .send()
            .await
            .context("发送 QQ 私聊 Markdown 消息失败")?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            bail!("QQ 私聊 Markdown 消息接口返回 {status}: {body}");
        }
        Ok(())
    }

    async fn send_c2c_media(&self, openid: &str, file_info: &str) -> Result<()> {
        let response = self
            .client
            .post(format!("{QQ_API_BASE}/v2/users/{openid}/messages"))
            .headers(self.headers().await?)
            .json(&json!({
                "msg_type": 7,
                "media": { "file_info": file_info },
            }))
            .send()
            .await
            .context("发送 QQ 私聊富媒体消息失败")?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            bail!("QQ 私聊富媒体消息接口返回 {status}: {body}");
        }
        Ok(())
    }

    pub(crate) async fn upload_c2c_media(
        &self,
        openid: &str,
        file: &Path,
        file_name: &str,
        file_type: MediaType,
        file_size: u64,
    ) -> Result<()> {
        let (md5, sha1, md5_10m) = file_digests(file).await?;
        let prepared = self
            .client
            .post(format!("{QQ_API_BASE}/v2/users/{openid}/upload_prepare"))
            .headers(self.headers().await?)
            .json(&json!({
                "file_type": file_type.code(),
                "file_size": file_size.to_string(),
                "file_name": file_name,
                "md5": md5,
                "sha1": sha1,
                "md5_10m": md5_10m,
            }))
            .send()
            .await
            .context("准备 QQ 私聊媒体上传失败")?
            .error_for_status()
            .context("QQ 私聊媒体预上传接口返回错误")?
            .json::<UploadPrepareResponse>()
            .await
            .context("QQ 私聊媒体预上传响应格式无效")?;
        let UploadPrepareResponse { upload_id, parts } = prepared;
        if parts.is_empty() {
            bail!("QQ 私聊媒体预上传未返回分片");
        }

        let mut source = tokio::fs::File::open(file)
            .await
            .with_context(|| format!("无法读取临时上传文件 {}", file.display()))?;
        let mut remaining = file_size;
        for part in parts {
            let block_size = part
                .block_size
                .parse::<u64>()
                .context("QQ 私聊媒体预上传返回的分片大小无效")?;
            if block_size == 0 || remaining == 0 {
                bail!("QQ 私聊媒体预上传返回了无效分片");
            }
            let size = block_size.min(remaining);
            let mut bytes =
                vec![0; usize::try_from(size).context("QQ 分片大小超出本机限制")?];
            source
                .read_exact(&mut bytes)
                .await
                .context("读取上传文件分片失败")?;
            let part_md5 = format!("{:x}", Md5::digest(&bytes));

            self.client
                .put(&part.presigned_url)
                .body(bytes)
                .send()
                .await
                .context("上传 QQ 私聊媒体分片失败")?
                .error_for_status()
                .context("QQ 私聊媒体分片上传接口返回错误")?;

            self.client
                .post(format!(
                    "{QQ_API_BASE}/v2/users/{openid}/upload_part_finish"
                ))
                .headers(self.headers().await?)
                .json(&json!({
                    "upload_id": &upload_id,
                    "part_index": part.index,
                    "block_size": size.to_string(),
                    "md5": part_md5,
                }))
                .send()
                .await
                .context("确认 QQ 私聊媒体分片失败")?
                .error_for_status()
                .context("QQ 私聊媒体分片确认接口返回错误")?;
            remaining -= size;
        }
        if remaining != 0 {
            bail!("QQ 私聊媒体预上传返回的分片不足以容纳文件");
        }

        let uploaded = self
            .client
            .post(format!("{QQ_API_BASE}/v2/users/{openid}/files"))
            .headers(self.headers().await?)
            .json(&json!({
                "file_type": file_type.code(),
                "file_name": file_name,
                "upload_id": &upload_id,
            }))
            .send()
            .await
            .context("完成 QQ 私聊媒体上传失败")?
            .error_for_status()
            .context("QQ 私聊媒体上传接口返回错误")?
            .json::<MediaUploadResponse>()
            .await
            .context("QQ 私聊媒体上传响应格式无效")?;
        self.send_c2c_media(openid, &uploaded.file_info).await
    }

    async fn headers(&self) -> Result<reqwest::header::HeaderMap> {
        let authorization = self.authorization().await?;
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::AUTHORIZATION,
            authorization.parse().context("QQ 授权头无效")?,
        );
        headers.insert(
            "X-Union-Appid",
            self.app_id.parse().context("QQ App ID 无效")?,
        );
        Ok(headers)
    }
}
pub(crate) fn markdown_payload(content: &str) -> Value {
    json!({
        "msg_type": 2,
        "markdown": { "content": content },
    })
}
