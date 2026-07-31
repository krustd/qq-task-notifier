use std::{ffi::OsStr, path::Path, sync::atomic::Ordering};

use axum::{
    Router,
    body::Bytes,
    extract::{DefaultBodyLimit, Multipart, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{get, post},
};
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use tracing::error;

use crate::{
    media::{MediaType, TemporaryFile, UploadedMedia},
    state::{ApiError, AppState, authenticate},
};

const MAX_MEDIA_SIZE: u64 = 200 * 1024 * 1024;
const MAX_MEDIA_REQUEST_SIZE: usize = MAX_MEDIA_SIZE as usize + 1024 * 1024;

pub(crate) fn router() -> Router<AppState> {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/status", get(status))
        .route("/v1/messages", post(task_completed))
        .route("/v1/markdown", post(send_markdown))
        .route(
            "/v1/media",
            post(upload_media).layer(DefaultBodyLimit::max(MAX_MEDIA_REQUEST_SIZE)),
        )
        .route("/task-completed", post(task_completed))
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

pub(crate) fn sanitized_file_name(value: Option<&str>) -> Option<String> {
    let file_name = Path::new(value?).file_name()?.to_str()?;
    (!file_name.is_empty() && file_name.len() <= 255 && !file_name.contains('\0'))
        .then(|| file_name.to_owned())
}

pub(crate) fn parse_media_type(
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
