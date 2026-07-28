# QQ Task Notifier

将外部任务的完成汇报通过 HTTP API 发送到 QQ 私聊的轻量通知服务。服务同时维持 QQ Bot WebSocket 连接，用于接收用户私聊并绑定默认通知接收人。配套 CLI 工具 [qn](https://github.com/krustd/qn) 可在任意 shell 中包装命令，完成后自动推送通知。

## 能力与限制

- 接收 HTTP 请求，将 `summary` 或 `content` 发送到 QQ C2C 私聊。
- 单条消息超过 4,000 个字符时自动拆分发送。
- 用户向机器人发送任意私聊消息后，会成为默认接收人。
- 支持在单次请求中显式传入 `openid`，向指定用户发送消息。
- **不支持多用户绑定管理。** 持久化存储仅有一个 `data/recipient_openid`；任何新私聊都会覆盖此前的默认接收人。显式 `openid` 只是定向发送能力，不会保存用户或维护订阅关系。

> API Token 持有者可使用已知的 `openid` 定向发送消息。请将 Token 视为高敏感凭据，并仅在受信任的网络中暴露 API。

## 配置

| 环境变量 | 必填 | 说明 |
| --- | --- | --- |
| `QQBOT_APP_ID` | 是 | QQ 机器人 App ID |
| `QQBOT_APP_SECRET` | 是 | QQ 机器人 App Secret |
| `QQBOT_API_TOKEN` | 是 | 调用 HTTP API 的 Bearer Token；使用至少 32 个随机字符 |
| `QQBOT_HTTP_HOST` | 否 | 监听地址，默认 `0.0.0.0` |
| `QQBOT_HTTP_PORT` | 否 | 监听端口，默认 `8765` |
| `QQBOT_OPENID_FILE` | 否 | 默认接收人 OpenID 的存储路径，默认 `data/recipient_openid` |
| `LOG_LEVEL` | 否 | 日志级别，默认 `INFO` |

默认部署直接在 `docker-compose.yml` 填写凭据。`.env` 是可选的高级方式，适合不希望修改基础 Compose 文件的部署；它不会被复制进镜像。

## 本地运行

需要 Python 3.11+ 和 [uv](https://docs.astral.sh/uv/)。

```bash
uv sync --locked
export QQBOT_APP_ID='你的 App ID'
export QQBOT_APP_SECRET='你的 App Secret'
export QQBOT_API_TOKEN='至少 32 个字符的随机 Token'
uv run python main.py
```

机器人建立 WebSocket 连接后，先从 QQ 向机器人发送一条私聊消息，以绑定默认接收人。

## Docker 运行

凭据不属于镜像，而是由 Docker Compose 在创建容器时注入。不要将包含真实凭据的 Compose 文件提交到版本库。

### 默认：直接编辑 Compose 文件

这是离线镜像和普通 Docker Compose 部署的推荐方式。先在构建机导出镜像：

```bash
docker build -t qq-task-notifier:latest .
docker save -o qq-task-notifier.tar qq-task-notifier:latest
```

将 `qq-task-notifier.tar` 与 `docker-compose.yml` 复制到目标机器。导入镜像后，编辑 `docker-compose.yml` 的 `environment`，填写 `QQBOT_APP_ID`、`QQBOT_APP_SECRET` 和 `QQBOT_API_TOKEN`：
将以下三项替换为真实值，保留其余配置：

```yaml
environment:
  QQBOT_APP_ID: "你的 App ID"
  QQBOT_APP_SECRET: "你的 App Secret"
  QQBOT_API_TOKEN: "至少 32 个字符的随机 Token"
```

```bash
docker load -i qq-task-notifier.tar
docker compose up -d
```

无需 `.env`、覆写文件或进入容器创建文件。

### 可选：使用 `.env`

如果不希望修改基础 Compose 文件，复制环境文件和环境模式模板：

```bash
cp .env.example .env
cp docker-compose.env.example.yml docker-compose.env.yml
# 编辑 .env，填写 QQBOT_APP_ID、QQBOT_APP_SECRET 与 QQBOT_API_TOKEN
docker compose -f docker-compose.yml -f docker-compose.env.yml up -d
```

`docker-compose.env.yml` 会用 `.env` 的值覆盖基础 Compose 文件中的占位符。

### 可选：覆盖特定部署的配置

复制覆写模板并填写所需变量：

```bash
cp docker-compose.override.example.yml docker-compose.override.yml
docker compose -f docker-compose.yml -f docker-compose.override.yml up -d
```

最后加载的 `docker-compose.override.yml` 优先级最高，可覆盖基础 Compose 文件。若同时使用 `.env`，将环境模式文件置于覆写文件之前：

```bash
docker compose -f docker-compose.yml -f docker-compose.env.yml -f docker-compose.override.yml up -d
```

`docker-compose.env.yml`、`docker-compose.override.yml` 均已被 Git 忽略。

Compose 使用命名卷 `qqbot-data` 保存默认接收人的 OpenID；重建容器不会丢失该绑定。查看日志：

```bash
docker compose logs -f qq-task-notifier
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
| `summary` 或 `content` | string | 必填其一，通知正文 |
| `openid` | string | 可选；指定本次消息的目标用户，未提供时发送给默认接收人 |

成功响应示例：

```json
{"ok": true, "chunks": 1}
```

### 查看状态

```bash
curl http://127.0.0.1:8765/status \
  -H "Authorization: Bearer $QQBOT_API_TOKEN"
```

响应中的 `connected` 表示 QQ WebSocket 是否仍连接，`bound` 表示是否已绑定默认接收人。

### 健康检查

`GET /healthz` 无需认证，适合容器健康探针：

```bash
curl http://127.0.0.1:8765/healthz
```

## License

[MIT](LICENSE)
