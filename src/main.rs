use std::{
    env,
    ffi::OsStr,
    fs::OpenOptions,
    net::SocketAddr,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use axum::{
    Router,
    body::Bytes,
    extract::{DefaultBodyLimit, Multipart, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use futures_util::{SinkExt, StreamExt};
use md5::Md5;
use serde::Deserialize;
use serde_json::{Value, json};
use sha1::Sha1;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::{Mutex, RwLock},
    time::{MissedTickBehavior, interval_at, sleep},
};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{error, info, warn};

const MAX_CONTENT_LENGTH: usize = 4_000;
const MAX_MEDIA_SIZE: u64 = 200 * 1024 * 1024;
const MAX_MEDIA_REQUEST_SIZE: usize = MAX_MEDIA_SIZE as usize + 1024 * 1024;
const BINDING_REPLY: &str = "已绑定通知接收人。之后任务完成时会发送最后汇报。";
const QQ_API_BASE: &str = "https://api.sgroup.qq.com";
const QQ_TOKEN_URL: &str = "https://bots.qq.com/app/getAppAccessToken";
static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Clone)]
struct AppState {
    api_token_hash: [u8; 32],
    recipient: Arc<RwLock<Option<String>>>,
    recipient_file: PathBuf,
    qq: QQApi,
    connected: Arc<AtomicBool>,
}

#[derive(Clone)]
struct QQApi {
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

struct GatewaySession {
    id: Option<String>,
    sequence: Option<i64>,
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
struct GatewayEnvelope {
    op: u8,
    d: Value,
    s: Option<i64>,
    t: Option<String>,
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

#[derive(Clone, Copy)]
enum MediaType {
    Image,
    File,
}

impl MediaType {
    const fn code(self) -> u8 {
        match self {
            Self::Image => 1,
            Self::File => 4,
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Image => "image",
            Self::File => "file",
        }
    }
}

struct UploadedMedia {
    temporary_file: TemporaryFile,
    file_name: String,
    content_type: Option<String>,
    size: u64,
}

struct TemporaryFile {
    path: PathBuf,
}

impl TemporaryFile {
    async fn create() -> Result<(Self, tokio::fs::File)> {
        for _ in 0..16 {
            let sequence = TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
            let path =
                env::temp_dir().join(format!("qqbot-upload-{}-{sequence}", std::process::id()));
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => return Ok((Self { path }, tokio::fs::File::from_std(file))),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("无法创建临时上传文件 {}", path.display()));
                }
            }
        }
        bail!("无法创建唯一的临时上传文件")
    }
}

impl Drop for TemporaryFile {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_file(&self.path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            warn!("无法删除临时上传文件 {}: {error}", self.path.display());
        }
    }
}

struct ApiError {
    status: StatusCode,
    message: &'static str,
    bearer_challenge: bool,
}

impl ApiError {
    const fn new(status: StatusCode, message: &'static str) -> Self {
        Self {
            status,
            message,
            bearer_challenge: false,
        }
    }

    const fn unauthorized() -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            message: "需要 Authorization: Bearer <QQBOT_API_TOKEN>",
            bearer_challenge: true,
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        if self.bearer_challenge {
            (
                self.status,
                [(header::WWW_AUTHENTICATE, "Bearer")],
                self.message,
            )
                .into_response()
        } else {
            (self.status, self.message).into_response()
        }
    }
}

impl QQApi {
    fn new(app_id: String, app_secret: String) -> Result<Self> {
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

    async fn authorization(&self) -> Result<String> {
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

    async fn gateway_url(&self) -> Result<String> {
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

    async fn send_c2c(&self, openid: &str, content: &str, msg_id: Option<&str>) -> Result<()> {
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

    async fn send_c2c_markdown(&self, openid: &str, content: &str) -> Result<()> {
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

    async fn upload_c2c_media(
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

impl AppState {
    async fn save_recipient(&self, openid: String) -> Result<()> {
        if let Some(parent) = self.recipient_file.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("无法创建 OpenID 目录 {}", parent.display()))?;
        }
        tokio::fs::write(&self.recipient_file, format!("{openid}\n"))
            .await
            .with_context(|| format!("无法保存 OpenID 文件 {}", self.recipient_file.display()))?;
        tokio::fs::set_permissions(&self.recipient_file, std::fs::Permissions::from_mode(0o600))
            .await
            .with_context(|| {
                format!("无法设置 OpenID 文件权限 {}", self.recipient_file.display())
            })?;
        *self.recipient.write().await = Some(openid);
        Ok(())
    }

    async fn recipient(&self) -> Option<String> {
        self.recipient.read().await.clone()
    }

    async fn recipient_for(&self, openid: Option<&str>) -> Result<String, ApiError> {
        match openid.filter(|value| !value.is_empty()) {
            Some(value) => Ok(value.to_owned()),
            None => self.recipient().await.ok_or(ApiError::new(
                StatusCode::PRECONDITION_FAILED,
                "尚未绑定 openid，请先向机器人发送一条消息。",
            )),
        }
    }

    async fn send_message(&self, content: &str, openid: Option<&str>) -> Result<usize, ApiError> {
        let recipient = self.recipient_for(openid).await?;
        if content.trim().is_empty() {
            return Err(ApiError::new(StatusCode::BAD_REQUEST, "content 不能为空。"));
        }

        let mut chunks = 0;
        let mut start = 0;
        let mut characters = 0;
        for (index, _) in content.char_indices() {
            if characters == MAX_CONTENT_LENGTH {
                self.qq
                    .send_c2c(&recipient, &content[start..index], None)
                    .await
                    .map_err(|error| {
                        error!("发送 QQ 消息失败: {error:#}");
                        ApiError::new(StatusCode::BAD_GATEWAY, "QQ 消息发送失败。")
                    })?;
                chunks += 1;
                start = index;
                characters = 0;
            }
            characters += 1;
        }
        self.qq
            .send_c2c(&recipient, &content[start..], None)
            .await
            .map_err(|error| {
                error!("发送 QQ 消息失败: {error:#}");
                ApiError::new(StatusCode::BAD_GATEWAY, "QQ 消息发送失败。")
            })?;
        Ok(chunks + 1)
    }

    async fn send_markdown(&self, content: &str, openid: Option<&str>) -> Result<(), ApiError> {
        if content.trim().is_empty() {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "Markdown content 不能为空。",
            ));
        }
        let recipient = self.recipient_for(openid).await?;
        self.qq
            .send_c2c_markdown(&recipient, content)
            .await
            .map_err(|error| {
                error!("发送 QQ 私聊 Markdown 消息失败: {error:#}");
                ApiError::new(StatusCode::BAD_GATEWAY, "QQ Markdown 消息发送失败。")
            })
    }

    async fn send_uploaded_media(
        &self,
        media: &UploadedMedia,
        media_type: MediaType,
        openid: Option<&str>,
    ) -> Result<(), ApiError> {
        let recipient = self.recipient_for(openid).await?;
        self.qq
            .upload_c2c_media(
                &recipient,
                &media.temporary_file.path,
                &media.file_name,
                media_type,
                media.size,
            )
            .await
            .map_err(|error| {
                error!("发送 QQ 私聊媒体失败: {error:#}");
                ApiError::new(StatusCode::BAD_GATEWAY, "QQ 媒体上传或发送失败。")
            })
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    init_logging();
    let app_id = read_required("QQBOT_APP_ID")?;
    let app_secret = read_required("QQBOT_APP_SECRET")?;
    let api_token = read_required("QQBOT_API_TOKEN")?;
    let host = env::var("QQBOT_HTTP_HOST").unwrap_or_else(|_| "0.0.0.0".to_owned());
    let port = env::var("QQBOT_HTTP_PORT")
        .unwrap_or_else(|_| "8765".to_owned())
        .parse::<u16>()
        .context("QQBOT_HTTP_PORT 必须是有效端口")?;
    let recipient_file = env::var_os("QQBOT_OPENID_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("data/recipient_openid"));
    let recipient = load_recipient(&recipient_file).await?;

    let state = AppState {
        api_token_hash: Sha256::digest(format!("Bearer {api_token}").as_bytes()).into(),
        recipient: Arc::new(RwLock::new(recipient)),
        recipient_file,
        qq: QQApi::new(app_id, app_secret)?,
        connected: Arc::new(AtomicBool::new(false)),
    };
    let gateway_state = state.clone();
    tokio::spawn(async move {
        run_gateway(gateway_state).await;
    });

    let app = Router::new()
        .route("/healthz", get(healthz))
        .route("/status", get(status))
        .route("/v1/messages", post(task_completed))
        .route("/v1/markdown", post(send_markdown))
        .route(
            "/v1/media",
            post(upload_media).layer(DefaultBodyLimit::max(MAX_MEDIA_REQUEST_SIZE)),
        )
        .route("/task-completed", post(task_completed))
        .with_state(state);
    let address: SocketAddr = format!("{host}:{port}")
        .parse()
        .context("QQBOT_HTTP_HOST 或 QQBOT_HTTP_PORT 无效")?;
    let listener = TcpListener::bind(address)
        .await
        .with_context(|| format!("无法监听 {address}"))?;
    info!("消息 API 监听于 http://{address}");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("HTTP 服务异常退出")
}

fn init_logging() {
    let filter = env::var("LOG_LEVEL").unwrap_or_else(|_| "info".to_owned());
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .compact()
        .init();
}

fn read_required(name: &str) -> Result<String> {
    env::var(name)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("缺少环境变量: {name}"))
}

async fn load_recipient(path: &Path) -> Result<Option<String>> {
    match tokio::fs::read_to_string(path).await {
        Ok(value) => Ok((!value.trim().is_empty()).then(|| value.trim().to_owned())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => {
            Err(error).with_context(|| format!("无法读取 OpenID 文件 {}", path.display()))
        }
    }
}

async fn file_digests(path: &Path) -> Result<(String, String, String)> {
    const FIRST_10_MB: usize = 10_002_432;

    let mut file = tokio::fs::File::open(path)
        .await
        .with_context(|| format!("无法读取临时上传文件 {}", path.display()))?;
    let mut full_md5 = Md5::new();
    let mut full_sha1 = Sha1::new();
    let mut first_10_mb_md5 = Md5::new();
    let mut first_10_mb_size = 0_usize;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file
            .read(&mut buffer)
            .await
            .context("读取上传文件校验数据失败")?;
        if count == 0 {
            break;
        }
        let bytes = &buffer[..count];
        full_md5.update(bytes);
        full_sha1.update(bytes);
        if first_10_mb_size < FIRST_10_MB {
            let length = (FIRST_10_MB - first_10_mb_size).min(bytes.len());
            first_10_mb_md5.update(&bytes[..length]);
            first_10_mb_size += length;
        }
    }
    Ok((
        format!("{:x}", full_md5.finalize()),
        format!("{:x}", full_sha1.finalize()),
        format!("{:x}", first_10_mb_md5.finalize()),
    ))
}

async fn healthz() -> impl IntoResponse {
    axum::Json(json!({ "ok": true }))
}

async fn status(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    authenticate(&state, &headers)?;
    Ok(axum::Json(json!({
        "connected": state.connected.load(Ordering::Acquire),
        "bound": state.recipient().await.is_some(),
    })))
}

async fn task_completed(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<impl IntoResponse, ApiError> {
    authenticate(&state, &headers)?;
    let payload: Value = serde_json::from_slice(&body)
        .map_err(|_| ApiError::new(StatusCode::BAD_REQUEST, "请求体必须是 JSON。"))?;
    let content = payload
        .get("summary")
        .or_else(|| payload.get("content"))
        .and_then(Value::as_str)
        .ok_or(ApiError::new(
            StatusCode::BAD_REQUEST,
            "summary 或 content 必须是字符串。",
        ))?;
    let openid = match payload.get("openid") {
        None | Some(Value::Null) => None,
        Some(Value::String(value)) => Some(value.as_str()),
        Some(_) => {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "openid 必须是字符串。",
            ));
        }
    };
    let chunks = state.send_message(content, openid).await?;
    Ok(axum::Json(json!({ "ok": true, "chunks": chunks })))
}

async fn send_markdown(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<impl IntoResponse, ApiError> {
    authenticate(&state, &headers)?;
    let payload: Value = serde_json::from_slice(&body)
        .map_err(|_| ApiError::new(StatusCode::BAD_REQUEST, "请求体必须是 JSON。"))?;
    let content = payload
        .get("content")
        .and_then(Value::as_str)
        .ok_or(ApiError::new(
            StatusCode::BAD_REQUEST,
            "content 必须是 Markdown 字符串。",
        ))?;
    let openid = match payload.get("openid") {
        None | Some(Value::Null) => None,
        Some(Value::String(value)) => Some(value.as_str()),
        Some(_) => {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "openid 必须是字符串。",
            ));
        }
    };
    state.send_markdown(content, openid).await?;
    Ok(axum::Json(json!({ "ok": true })))
}

fn markdown_payload(content: &str) -> Value {
    json!({
        "msg_type": 2,
        "markdown": { "content": content },
    })
}

async fn upload_media(
    State(state): State<AppState>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> Result<impl IntoResponse, ApiError> {
    authenticate(&state, &headers)?;

    let mut openid = None;
    let mut requested_type = None;
    let mut uploaded = None;
    while let Some(mut field) = multipart
        .next_field()
        .await
        .map_err(|_| ApiError::new(StatusCode::BAD_REQUEST, "multipart 请求体无效。"))?
    {
        match field.name() {
            Some("openid") => {
                if openid.is_some() {
                    return Err(ApiError::new(
                        StatusCode::BAD_REQUEST,
                        "openid 只能提供一次。",
                    ));
                }
                openid = Some(
                    field
                        .text()
                        .await
                        .map_err(|_| ApiError::new(StatusCode::BAD_REQUEST, "openid 无效。"))?,
                );
            }
            Some("file_type") => {
                if requested_type.is_some() {
                    return Err(ApiError::new(
                        StatusCode::BAD_REQUEST,
                        "file_type 只能提供一次。",
                    ));
                }
                requested_type = Some(
                    field
                        .text()
                        .await
                        .map_err(|_| ApiError::new(StatusCode::BAD_REQUEST, "file_type 无效。"))?,
                );
            }
            Some("file") => {
                if uploaded.is_some() {
                    return Err(ApiError::new(StatusCode::BAD_REQUEST, "只能上传一个文件。"));
                }
                let file_name = sanitized_file_name(field.file_name()).ok_or(ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "file 必须包含有效的文件名。",
                ))?;
                let content_type = field.content_type().map(str::to_owned);
                let (temporary_file, mut output) =
                    TemporaryFile::create().await.map_err(|error| {
                        error!("创建临时上传文件失败: {error:#}");
                        ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "无法暂存上传文件。")
                    })?;
                let mut size = 0_u64;
                while let Some(chunk) = field
                    .chunk()
                    .await
                    .map_err(|_| ApiError::new(StatusCode::BAD_REQUEST, "读取上传文件失败。"))?
                {
                    size = size.checked_add(chunk.len() as u64).ok_or(ApiError::new(
                        StatusCode::PAYLOAD_TOO_LARGE,
                        "上传文件超过 200 MB 限制。",
                    ))?;
                    if size > MAX_MEDIA_SIZE {
                        return Err(ApiError::new(
                            StatusCode::PAYLOAD_TOO_LARGE,
                            "上传文件超过 200 MB 限制。",
                        ));
                    }
                    output.write_all(&chunk).await.map_err(|error| {
                        error!("写入临时上传文件失败: {error:#}");
                        ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "无法暂存上传文件。")
                    })?;
                }
                if size == 0 {
                    return Err(ApiError::new(StatusCode::BAD_REQUEST, "上传文件不能为空。"));
                }
                output.flush().await.map_err(|error| {
                    error!("刷新临时上传文件失败: {error:#}");
                    ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "无法暂存上传文件。")
                })?;
                drop(output);
                uploaded = Some(UploadedMedia {
                    temporary_file,
                    file_name,
                    content_type,
                    size,
                });
            }
            _ => {
                return Err(ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "仅支持 openid、file_type 和 file 字段。",
                ));
            }
        }
    }

    let uploaded = uploaded.ok_or(ApiError::new(
        StatusCode::BAD_REQUEST,
        "请求缺少 file 文件字段。",
    ))?;
    let media_type = parse_media_type(requested_type.as_deref(), &uploaded)?;
    state
        .send_uploaded_media(&uploaded, media_type, openid.as_deref())
        .await?;
    Ok(axum::Json(json!({
        "ok": true,
        "file_name": uploaded.file_name,
        "file_type": media_type.label(),
    })))
}

fn sanitized_file_name(value: Option<&str>) -> Option<String> {
    let file_name = Path::new(value?).file_name()?.to_str()?;
    (!file_name.is_empty() && file_name.len() <= 255 && !file_name.contains('\0'))
        .then(|| file_name.to_owned())
}

fn parse_media_type(
    requested_type: Option<&str>,
    uploaded: &UploadedMedia,
) -> Result<MediaType, ApiError> {
    match requested_type
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        Some("image") | Some("1") => Ok(MediaType::Image),
        Some("file") | Some("4") => Ok(MediaType::File),
        Some(_) => Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "file_type 只能是 image 或 file。",
        )),
        None if uploaded
            .content_type
            .as_deref()
            .is_some_and(|value| value.starts_with("image/")) =>
        {
            Ok(MediaType::Image)
        }
        None if is_image_file_name(&uploaded.file_name) => Ok(MediaType::Image),
        None => Ok(MediaType::File),
    }
}

fn is_image_file_name(file_name: &str) -> bool {
    matches!(
        Path::new(file_name)
            .extension()
            .and_then(OsStr::to_str)
            .map(|value| value.to_ascii_lowercase())
            .as_deref(),
        Some("png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp")
    )
}

fn authenticate(state: &AppState, headers: &HeaderMap) -> Result<(), ApiError> {
    let supplied = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    let supplied_hash: [u8; 32] = Sha256::digest(supplied.as_bytes()).into();
    if bool::from(supplied_hash.ct_eq(&state.api_token_hash)) {
        Ok(())
    } else {
        Err(ApiError::unauthorized())
    }
}

async fn run_gateway(state: AppState) {
    let mut session = GatewaySession {
        id: None,
        sequence: None,
    };
    loop {
        state.connected.store(false, Ordering::Release);
        if let Err(error) = gateway_session(&state, &mut session).await {
            warn!("QQ Gateway 已断开: {error:#}");
        }
        sleep(Duration::from_secs(5)).await;
    }
}

async fn gateway_session(state: &AppState, session: &mut GatewaySession) -> Result<()> {
    let gateway_url = state.qq.gateway_url().await?;
    let (mut socket, _) = connect_async(&gateway_url)
        .await
        .context("连接 QQ Gateway 失败")?;
    let mut heartbeat = interval_at(
        tokio::time::Instant::now() + Duration::from_secs(24 * 60 * 60),
        Duration::from_secs(24 * 60 * 60),
    );
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut heartbeat_ready = false;

    loop {
        tokio::select! {
            _ = heartbeat.tick(), if heartbeat_ready => {
                send_gateway(&mut socket, json!({ "op": 1, "d": session.sequence })).await?;
            }
            message = socket.next() => {
                match message {
                    Some(Ok(Message::Text(text))) => {
                        let envelope: GatewayEnvelope = serde_json::from_str(text.as_str())
                            .context("QQ Gateway 消息格式无效")?;
                        if let Some(sequence) = envelope.s {
                            session.sequence = Some(sequence);
                        }
                        match envelope.op {
                            0 => handle_dispatch(state, session, envelope).await?,
                            1 => send_gateway(&mut socket, json!({ "op": 1, "d": session.sequence })).await?,
                            7 => bail!("QQ Gateway 要求重新连接"),
                            9 => {
                                session.id = None;
                                session.sequence = None;
                                bail!("QQ Gateway 会话无效");
                            }
                            10 => {
                                let interval_ms = envelope.d
                                    .get("heartbeat_interval")
                                    .and_then(Value::as_u64)
                                    .ok_or_else(|| anyhow!("QQ Gateway Hello 缺少 heartbeat_interval"))?;
                                heartbeat = interval_at(
                                    tokio::time::Instant::now() + Duration::from_millis(interval_ms),
                                    Duration::from_millis(interval_ms),
                                );
                                heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);
                                heartbeat_ready = true;
                                let authorization = state.qq.authorization().await?;
                                let identify = match &session.id {
                                    Some(id) => json!({
                                        "op": 6,
                                        "d": { "token": authorization, "session_id": id, "seq": session.sequence },
                                    }),
                                    None => json!({
                                        "op": 2,
                                        "d": { "token": authorization, "intents": 1, "shard": [0, 1] },
                                    }),
                                };
                                send_gateway(&mut socket, identify).await?;
                            }
                            11 => {}
                            _ => {}
                        }
                    }
                    Some(Ok(Message::Ping(payload))) => socket.send(Message::Pong(payload)).await.context("回复 QQ Gateway Ping 失败")?,
                    Some(Ok(Message::Close(frame))) => bail!("QQ Gateway 关闭连接: {frame:?}"),
                    Some(Ok(_)) => {}
                    Some(Err(error)) => return Err(error).context("读取 QQ Gateway 消息失败"),
                    None => bail!("QQ Gateway 连接已结束"),
                }
            }
        }
    }
}

async fn send_gateway(
    socket: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    payload: Value,
) -> Result<()> {
    socket
        .send(Message::Text(payload.to_string().into()))
        .await
        .context("发送 QQ Gateway 消息失败")
}

async fn handle_dispatch(
    state: &AppState,
    session: &mut GatewaySession,
    envelope: GatewayEnvelope,
) -> Result<()> {
    match envelope.t.as_deref() {
        Some("READY") => {
            session.id = envelope
                .d
                .get("session_id")
                .and_then(Value::as_str)
                .map(str::to_owned);
            state.connected.store(true, Ordering::Release);
            info!("QQ Bot WebSocket 已连接");
        }
        Some("RESUMED") => {
            state.connected.store(true, Ordering::Release);
            info!("QQ Bot WebSocket 已恢复连接");
        }
        Some("C2C_MESSAGE_CREATE") => {
            let Some(openid) = envelope
                .d
                .pointer("/author/user_openid")
                .and_then(Value::as_str)
                .map(str::to_owned)
            else {
                warn!("忽略缺少 user_openid 的 C2C 消息事件");
                return Ok(());
            };
            let message_id = envelope.d.get("id").and_then(Value::as_str);
            state.save_recipient(openid.clone()).await?;
            info!("已捕获并保存 C2C recipient openid");
            if let Err(error) = state.qq.send_c2c(&openid, BINDING_REPLY, message_id).await {
                warn!("发送绑定确认失败: {error:#}");
            }
        }
        _ => {}
    }
    Ok(())
}

async fn shutdown_signal() {
    if let Err(error) = tokio::signal::ctrl_c().await {
        error!("无法监听关闭信号: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uploaded(file_name: &str, content_type: Option<&str>) -> UploadedMedia {
        UploadedMedia {
            temporary_file: TemporaryFile {
                path: PathBuf::from("/nonexistent/qqbot-test-upload"),
            },
            file_name: file_name.to_owned(),
            content_type: content_type.map(str::to_owned),
            size: 1,
        }
    }

    #[test]
    fn detects_images_and_keeps_other_uploads_as_files() {
        assert!(matches!(
            parse_media_type(None, &uploaded("report.png", None)),
            Ok(MediaType::Image)
        ));
        assert!(matches!(
            parse_media_type(None, &uploaded("archive.zip", Some("application/zip"))),
            Ok(MediaType::File)
        ));
        assert!(matches!(
            parse_media_type(Some("image"), &uploaded("archive.zip", None)),
            Ok(MediaType::Image)
        ));
    }

    #[test]
    fn builds_c2c_custom_markdown_payload() {
        assert_eq!(
            markdown_payload("# 标题"),
            json!({
                "msg_type": 2,
                "markdown": { "content": "# 标题" },
            })
        );
    }

    #[test]
    fn rejects_unsupported_media_type() {
        assert!(parse_media_type(Some("video"), &uploaded("clip.mp4", None)).is_err());
    }

    #[test]
    fn strips_directory_components_from_upload_file_name() {
        assert_eq!(
            sanitized_file_name(Some("../../private/report.pdf")).as_deref(),
            Some("report.pdf")
        );
        assert!(sanitized_file_name(None).is_none());
    }

    #[tokio::test]
    async fn calculates_qq_required_file_digests() {
        let (temporary_file, mut output) = TemporaryFile::create().await.unwrap();
        output.write_all(b"abc").await.unwrap();
        output.flush().await.unwrap();
        drop(output);

        let (md5, sha1, md5_10m) = file_digests(&temporary_file.path).await.unwrap();
        assert_eq!(md5, "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(sha1, "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(md5_10m, md5);
    }
}
