use std::{
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use anyhow::{Context, Result};
use axum::{
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tokio::sync::RwLock;
use tracing::error;

use crate::{
    media::{MediaType, UploadedMedia},
    qq_api::QQApi,
};

const MAX_CONTENT_LENGTH: usize = 4_000;
pub(crate) const BINDING_REPLY: &str = "已绑定通知接收人。之后任务完成时会发送最后汇报。";
#[derive(Clone)]
pub(crate) struct AppState {
    api_token_hash: [u8; 32],
    recipient: Arc<RwLock<Option<String>>>,
    recipient_file: PathBuf,
    pub(crate) qq: QQApi,
    pub(crate) connected: Arc<AtomicBool>,
}
pub(crate) struct ApiError {
    status: StatusCode,
    message: &'static str,
    bearer_challenge: bool,
}

impl ApiError {
    pub(crate) const fn new(status: StatusCode, message: &'static str) -> Self {
        Self {
            status,
            message,
            bearer_challenge: false,
        }
    }

    pub(crate) const fn unauthorized() -> Self {
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
impl AppState {
    pub(crate) async fn new(
        api_token: &str,
        app_id: String,
        app_secret: String,
        recipient_file: PathBuf,
    ) -> Result<Self> {
        let recipient = load_recipient(&recipient_file).await?;
        Ok(Self {
            api_token_hash: Sha256::digest(format!("Bearer {api_token}").as_bytes()).into(),
            recipient: Arc::new(RwLock::new(recipient)),
            recipient_file,
            qq: QQApi::new(app_id, app_secret)?,
            connected: Arc::new(AtomicBool::new(false)),
        })
    }
}

impl AppState {
    pub(crate) async fn save_recipient(&self, openid: String) -> Result<()> {
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

    pub(crate) async fn recipient(&self) -> Option<String> {
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

    pub(crate) async fn send_message(
        &self,
        content: &str,
        openid: Option<&str>,
    ) -> Result<usize, ApiError> {
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

    pub(crate) async fn send_markdown(
        &self,
        content: &str,
        openid: Option<&str>,
    ) -> Result<(), ApiError> {
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

    pub(crate) async fn send_uploaded_media(
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
async fn load_recipient(path: &Path) -> Result<Option<String>> {
    match tokio::fs::read_to_string(path).await {
        Ok(value) => Ok((!value.trim().is_empty()).then(|| value.trim().to_owned())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => {
            Err(error).with_context(|| format!("无法读取 OpenID 文件 {}", path.display()))
        }
    }
}
pub(crate) fn authenticate(state: &AppState, headers: &HeaderMap) -> Result<(), ApiError> {
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
