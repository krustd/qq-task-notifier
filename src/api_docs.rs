#![allow(dead_code)]

use utoipa::{
    Modify, OpenApi, ToSchema,
    openapi::security::{Http, HttpAuthScheme, SecurityScheme},
};
use utoipa_swagger_ui::SwaggerUi;

#[derive(ToSchema)]
struct HealthResponse {
    ok: bool,
}

#[derive(ToSchema)]
struct StatusResponse {
    connected: bool,
    bound: bool,
}

#[derive(ToSchema)]
struct NotificationRequest {
    /// 通知正文；`summary` 或 `content` 必须提供其一。
    summary: Option<String>,
    /// 通知正文；`summary` 或 `content` 必须提供其一。
    content: Option<String>,
    /// 本次通知的目标用户；省略时使用已绑定的默认接收人。
    openid: Option<String>,
}

#[derive(ToSchema)]
struct MarkdownRequest {
    /// QQ C2C 自定义 Markdown 正文。
    content: String,
    /// 本次通知的目标用户；省略时使用已绑定的默认接收人。
    openid: Option<String>,
}

#[derive(ToSchema)]
struct MediaUploadRequest {
    /// 要上传的图片或文件，最大 200 MB。
    #[schema(value_type = String, format = Binary)]
    file: String,
    /// 可选值：`image` 或 `file`。省略时由服务自动识别。
    file_type: Option<String>,
    /// 本次通知的目标用户；省略时使用已绑定的默认接收人。
    openid: Option<String>,
}

#[derive(ToSchema)]
struct NotificationResponse {
    ok: bool,
    chunks: usize,
}

#[derive(ToSchema)]
struct SuccessResponse {
    ok: bool,
}

#[derive(ToSchema)]
struct MediaResponse {
    ok: bool,
    file_name: String,
    file_type: String,
}

struct SecurityAddon;

impl Modify for SecurityAddon {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        openapi.components.as_mut().unwrap().add_security_scheme(
            "bearer_auth",
            SecurityScheme::Http(Http::new(HttpAuthScheme::Bearer)),
        );
    }
}

#[derive(OpenApi)]
#[openapi(
    paths(
        healthz,
        status,
        send_message,
        task_completed_compatibility,
        send_markdown,
        upload_media,
    ),
    components(schemas(
        HealthResponse,
        StatusResponse,
        NotificationRequest,
        MarkdownRequest,
        MediaUploadRequest,
        NotificationResponse,
        SuccessResponse,
        MediaResponse,
    )),
    modifiers(&SecurityAddon),
    tags(
        (name = "健康检查", description = "服务存活与 QQ Gateway 连接状态。"),
        (name = "通知", description = "向绑定的 QQ C2C 接收人发送通知。"),
    )
)]
struct ApiDoc;

pub(crate) fn router() -> SwaggerUi {
    SwaggerUi::new("/docs").url("/openapi.json", ApiDoc::openapi())
}

/// 返回无需认证的服务存活状态。
#[utoipa::path(
    get,
    path = "/healthz",
    tag = "健康检查",
    responses((status = 200, description = "服务正常运行。", body = HealthResponse))
)]
fn healthz() {}

/// 返回 QQ Gateway 连接与默认接收人绑定状态。
#[utoipa::path(
    get,
    path = "/status",
    tag = "健康检查",
    security(("bearer_auth" = [])),
    responses(
        (status = 200, description = "当前状态。", body = StatusResponse),
        (status = 401, description = "Bearer Token 无效或缺失。")
    )
)]
fn status() {}

/// 发送文本通知。
#[utoipa::path(
    post,
    path = "/v1/messages",
    tag = "通知",
    security(("bearer_auth" = [])),
    request_body = NotificationRequest,
    responses(
        (status = 200, description = "通知已发送。", body = NotificationResponse),
        (status = 400, description = "请求体无效。"),
        (status = 401, description = "Bearer Token 无效或缺失。"),
        (status = 412, description = "尚未绑定默认接收人。")
    )
)]
fn send_message() {}

#[utoipa::path(
    post,
    path = "/task-completed",
    tag = "通知",
    security(("bearer_auth" = [])),
    request_body = NotificationRequest,
    responses(
        (status = 200, description = "通知已发送。", body = NotificationResponse),
        (status = 400, description = "请求体无效。"),
        (status = 401, description = "Bearer Token 无效或缺失。"),
        (status = 412, description = "尚未绑定默认接收人。")
    )
)]
fn task_completed_compatibility() {}

/// 发送 QQ C2C 自定义 Markdown 通知。
#[utoipa::path(
    post,
    path = "/v1/markdown",
    tag = "通知",
    security(("bearer_auth" = [])),
    request_body = MarkdownRequest,
    responses(
        (status = 200, description = "Markdown 通知已发送。", body = SuccessResponse),
        (status = 400, description = "请求体无效。"),
        (status = 401, description = "Bearer Token 无效或缺失。"),
        (status = 412, description = "尚未绑定默认接收人。")
    )
)]
fn send_markdown() {}

/// 上传并发送 QQ C2C 图片或文件。
#[utoipa::path(
    post,
    path = "/v1/media",
    tag = "通知",
    security(("bearer_auth" = [])),
    request_body(content = MediaUploadRequest, content_type = "multipart/form-data"),
    responses(
        (status = 200, description = "媒体已上传并发送。", body = MediaResponse),
        (status = 400, description = "上传表单无效。"),
        (status = 401, description = "Bearer Token 无效或缺失。"),
        (status = 412, description = "尚未绑定默认接收人。"),
        (status = 413, description = "上传文件超过 200 MB。")
    )
)]
fn upload_media() {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn documents_bearer_auth_and_public_health_check() {
        let document = serde_json::to_value(ApiDoc::openapi()).unwrap();

        assert_eq!(
            document.pointer("/components/securitySchemes/bearer_auth/scheme"),
            Some(&serde_json::json!("bearer"))
        );
        assert!(document.pointer("/paths/~1healthz/get/security").is_none());
        assert_eq!(
            document.pointer("/paths/~1status/get/security/0/bearer_auth"),
            Some(&serde_json::json!([]))
        );
    }
}
