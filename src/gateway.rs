use std::{sync::atomic::Ordering, time::Duration};

use anyhow::{Context, Result, anyhow, bail};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::time::{MissedTickBehavior, interval_at, sleep};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{info, warn};

use crate::state::{AppState, BINDING_REPLY};

const QQ_GROUP_AND_C2C_EVENT_INTENT: u32 = 1 << 25;
pub(crate) struct GatewaySession {
    pub(crate) id: Option<String>,
    pub(crate) sequence: Option<i64>,
}
#[derive(Deserialize)]
pub(crate) struct GatewayEnvelope {
    pub(crate) op: u8,
    #[serde(default)]
    pub(crate) d: Value,
    pub(crate) s: Option<i64>,
    pub(crate) t: Option<String>,
}
pub(crate) async fn run_gateway(state: AppState) {
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
                                send_gateway(
                                    &mut socket,
                                    gateway_identify_payload(&authorization, session),
                                )
                                .await?;
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

pub(crate) fn gateway_identify_payload(authorization: &str, session: &GatewaySession) -> Value {
    match &session.id {
        Some(id) => json!({
            "op": 6,
            "d": { "token": authorization, "session_id": id, "seq": session.sequence },
        }),
        None => json!({
            "op": 2,
            "d": {
                "token": authorization,
                "intents": QQ_GROUP_AND_C2C_EVENT_INTENT,
                "shard": [0, 1],
            },
        }),
    }
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
