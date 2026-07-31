# qnserver-rs

将外部任务完成汇报通过 HTTP API 发送到 QQ 私聊的 Rust 通知服务。服务保持 QQ Bot WebSocket 连接，接收用户私聊并绑定默认通知接收人；配套 CLI 工具 [qn](https://github.com/krustd/qn) 可在任意 shell 中包装命令，完成后自动推送通知。

## 能力与限制

- 接收 HTTP 请求，将 `summary` 或 `content` 发送到 QQ C2C 私聊。
- 单条消息超过 4,000 个字符时自动拆分发送。
- 用户向机器人发送任意私聊消息后，会成为默认接收人。
- 支持在单次请求中显式传入 `openid`，向指定用户发送消息。
- 默认接收人存储在 `data/recipient_openid`，权限为 `0600`。每次新私聊都会覆盖此前绑定；不支持订阅列表和广播。

> API Token 持有者可使用已知的 `openid` 定向发送消息。请将 Token 视为高敏感凭据，并仅在受信任的网络中暴露 API。

## QQ 机器人接入

1. 在 [QQ 机器人管理后台](https://q.qq.com/qqbot/dashboard/) 创建机器人并取得 `AppID` 与 `AppSecret`。
2. 按 [QQ 机器人官方文档](https://bot.q.qq.com/wiki/develop/api-v2/) 开通私聊能力并发布机器人。
3. 服务启动且 WebSocket 已连接后，目标接收人必须先向机器人发送一条私聊消息，才能绑定通知接收人。

`AppSecret` 与 API Token 只能保存在部署主机，不得提交到仓库或构建进镜像。

## 配置

| 环境变量 | 必填 | 说明 |
| --- | --- | --- |
| `QQBOT_APP_ID` | 是 | QQ 机器人 `AppID`。 |
| `QQBOT_APP_SECRET` | 是 | QQ 机器人 `AppSecret`。 |
| `QQBOT_API_TOKEN` | 是 | 本服务 Bearer Token；建议至少 32 个随机字符。 |
| `QQBOT_HTTP_HOST` | 否 | 监听地址，默认 `0.0.0.0`。 |
| `QQBOT_HTTP_PORT` | 否 | 监听端口，默认 `8765`。 |
| `QQBOT_OPENID_FILE` | 否 | 默认接收人 OpenID 存储路径，默认 `data/recipient_openid`。 |
| `LOG_LEVEL` | 否 | 日志级别，默认 `info`。 |

## 本地运行

需要 Rust 1.85+：

```bash
export QQBOT_APP_ID='你的 App ID'
export QQBOT_APP_SECRET='你的 App Secret'
export QQBOT_API_TOKEN='至少 32 个字符的随机 Token'
cargo run --release
```

## Docker 运行

GitHub Action 会在推送到 `main` 时发布 `latest`，推送版本标签（例如 `v0.1.0`）时额外发布对应版本镜像。首次发布后，在 GitHub Packages 中将 `qnserver-rs` 设置为 Public，远程主机即可匿名拉取。

```bash
cp .env.example .env
cp docker-compose.env.example.yml docker-compose.env.yml
# 编辑 .env，填写 QQBOT_APP_ID、QQBOT_APP_SECRET 与 QQBOT_API_TOKEN
docker compose -f docker-compose.yml -f docker-compose.env.yml pull
docker compose -f docker-compose.yml -f docker-compose.env.yml up -d
```

`docker-compose.yml` 默认拉取 `ghcr.io/krustd/qq-task-notifier:latest`。生产环境建议将镜像标签固定为已发布版本，例如 `:0.1.0`。接收人 OpenID 继续由命名卷 `qqbot-data` 持久化，升级后不会丢失已有绑定。

查看日志：

```bash
docker compose logs -f qnserver-rs
```

## HTTP API

除健康检查外，接口均需请求头：

```text
Authorization: Bearer <QQBOT_API_TOKEN>
```

### 发送通知

`POST /v1/messages`，兼容别名：`POST /task-completed`。

```bash
curl -X POST http://127.0.0.1:8765/v1/messages \
  -H "Authorization: Bearer $QQBOT_API_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"summary":"任务完成：部署成功"}'
```

请求体字段：

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| `summary` 或 `content` | string | 必填其一，通知正文。 |
| `openid` | string | 可选；指定本次消息的目标用户；未提供时发送给默认接收人。 |

成功响应：

```json
{"ok":true,"chunks":1}
```

### 发送私聊 Markdown

`POST /v1/markdown` 使用 QQ C2C 自定义 Markdown 消息。

```bash
curl -X POST http://127.0.0.1:8765/v1/markdown \
  -H "Authorization: Bearer $QQBOT_API_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"content":"# 任务完成\n**部署成功**"}'
```

请求体字段：

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| `content` | string | 必填；Markdown 正文。 |
| `openid` | string | 可选；指定本次私聊的目标用户；未提供时发送给默认接收人。 |

Markdown 内嵌图片仍须使用 QQ 平台可以访问的公网 URL；若图片只在本机，请改用 `/v1/media` 上传发送。

### 上传并发送私聊图片或文件

`POST /v1/media` 接收 `multipart/form-data`。调用方直接上传文件内容；服务只在临时文件中保存上传内容，随后通过 QQ C2C 分片上传并发送，不需要公网 URL 或文件服务器。

```bash
curl -X POST http://127.0.0.1:8765/v1/media \
  -H "Authorization: Bearer $QQBOT_API_TOKEN" \
  -F 'file=@./report.png' \
  -F 'file_type=image'
```

表单字段：

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| `file` | file | 必填；单个文件，最大 200 MB。 |
| `file_type` | string | 可选；`image` 或 `file`。未提供时按图片 MIME 类型或常见图片扩展名自动识别，其余按文件发送。 |
| `openid` | string | 可选；指定本次私聊的目标用户；未提供时发送给默认接收人。 |

上传仅支持 QQ C2C 私聊；文件在请求处理结束时删除。

### 查看状态

```bash
curl http://127.0.0.1:8765/status \
  -H "Authorization: Bearer $QQBOT_API_TOKEN"
```

`connected` 表示 QQ WebSocket 是否连接，`bound` 表示是否已经绑定默认接收人。

### 健康检查

```bash
curl http://127.0.0.1:8765/healthz
```

## License

[MIT](LICENSE)
