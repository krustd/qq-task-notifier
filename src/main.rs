mod api_docs;
mod gateway;
mod http_api;
mod media;
mod qq_api;
mod state;

use std::{env, net::SocketAddr, path::PathBuf};

use anyhow::{Context, Result, anyhow};
use tokio::net::TcpListener;
use tracing::{error, info};

use crate::{gateway::run_gateway, state::AppState};

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
    let state = AppState::new(&api_token, app_id, app_secret, recipient_file).await?;
    let gateway_state = state.clone();
    tokio::spawn(async move {
        run_gateway(gateway_state).await;
    });

    let app = http_api::router()
        .merge(api_docs::router())
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

async fn shutdown_signal() {
    if let Err(error) = tokio::signal::ctrl_c().await {
        error!("无法监听关闭信号: {error}");
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use serde_json::json;
    use tokio::io::AsyncWriteExt;

    use crate::{
        gateway::{GatewayEnvelope, GatewaySession, gateway_identify_payload},
        http_api::{parse_media_type, sanitized_file_name},
        media::{MediaType, TemporaryFile, UploadedMedia, file_digests},
        qq_api::markdown_payload,
    };

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
    fn accepts_gateway_ack_without_payload() {
        let envelope: GatewayEnvelope = serde_json::from_str(r#"{"op":11}"#).unwrap();

        assert_eq!(envelope.op, 11);
        assert!(envelope.d.is_null());
        assert!(envelope.s.is_none());
        assert!(envelope.t.is_none());
    }

    #[test]
    fn subscribes_to_group_and_c2c_events() {
        let session = GatewaySession {
            id: None,
            sequence: None,
        };

        assert_eq!(
            gateway_identify_payload("access-token", &session),
            json!({
                "op": 2,
                "d": {
                    "token": "access-token",
                    "intents": 33_554_432,
                    "shard": [0, 1],
                },
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
