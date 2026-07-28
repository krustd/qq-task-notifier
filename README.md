# QQ Task Notifier

将外部任务的完成汇报通过 HTTP API 发送到 QQ 私聊的轻量通知服务。服务同时维持 QQ Bot WebSocket 连接，用于接收用户私聊并绑定默认通知接收人。配套 CLI 工具 [qn](https://github.com/krustd/qn) 可在任意 shell 中包装命令，完成后自动推送通知。

## 能力与限制

- 接收 HTTP 请求，将 `summary` 或 `content` 发送到 QQ C2C 私聊。
- 单条消息超过 4,000 个字符时自动拆分发送。
- 用户向机器人发送任意私聊消息后，会成为默认接收人。
- 支持在单次请求中显式传入 `openid`，向指定用户发送消息。
- **不支持多用户绑定管理。** 持久化存储仅有一个 `data/recipient_openid`；任何新私聊都会覆盖此前的默认接收人。显式 `openid` 只是定向发送能力，不会保存用户或维护订阅关系。

> API Token 持有者可使用已知的 `openid` 定向发送消息。请将 Token 视为高敏感凭据，并仅在受信任的网络中暴露 API。

## QQ 机器人接入

1. 在 [QQ 机器人管理后台](https://q.qq.com/qqbot/dashboard/) 登录并创建机器人。
2. 在机器人的开发配置中取得 `AppID` 和 `AppSecret`。两者是 QQ 开放平台凭据；`AppSecret` 必须仅保存在部署主机，不能提交到仓库或写入镜像。
3. 按 [QQ 机器人官方开发文档](https://bot.q.qq.com/wiki/develop/api-v2/) 完成机器人配置、发布及私聊能力的开通。平台侧的审核、权限和发布要求以该文档为准。
4. 服务启动并显示已连接后，**目标接收人**必须先向机器人发送一条任意私聊消息，才能绑定通知接收人。

本服务只保存一个默认接收人的 OpenID：每次收到私聊，都会以该发送者的 OpenID 覆盖此前绑定。因此它不支持订阅列表、多用户绑定或向所有已私聊用户广播。要切换默认接收人，让新的目标接收人向机器人再发一条私聊消息即可。

## 配置

| 环境变量 | 必填 | 说明 |
| --- | --- | --- |
| `QQBOT_APP_ID` | 是 | 从 QQ 机器人管理后台取得的 `AppID`。 |
| `QQBOT_APP_SECRET` | 是 | 从 QQ 机器人管理后台取得的 `AppSecret`；高敏感凭据。 |
| `QQBOT_API_TOKEN` | 是 | 此服务自行校验 HTTP API 的 Bearer Token，**不是** QQ 平台的 Token；使用至少 32 个随机字符，例如 `openssl rand -hex 32`。 |
| `QQBOT_HTTP_HOST` | 否 | 监听地址，默认 `0.0.0.0`。 |
| `QQBOT_HTTP_PORT` | 否 | 监听端口，默认 `8765`。 |
| `QQBOT_OPENID_FILE` | 否 | 默认接收人 OpenID 的存储路径，默认 `data/recipient_openid`。 |
| `LOG_LEVEL` | 否 | 日志级别，默认 `INFO`。 |

凭据不属于镜像。推荐将它们写入仅留在部署主机的 `.env`，不要提交真实凭据、不要将其写入镜像。

## 本地运行

需要 Python 3.11+ 和 [uv](https://docs.astral.sh/uv/)。

```bash
uv sync --locked
export QQBOT_APP_ID='你的 App ID'
export QQBOT_APP_SECRET='你的 App Secret'
export QQBOT_API_TOKEN='至少 32 个字符的随机 Token'
uv run python main.py
```

服务建立 WebSocket 连接后，由目标接收人向机器人发送一条私聊消息。服务会保存该用户的 OpenID；任何后续私聊都会替换这个默认接收人。

## Docker 运行

凭据不属于镜像，而是由 Docker Compose 在创建容器时注入。不要将包含真实凭据的 Compose 文件提交到版本库。

### 默认：从 GitHub Container Registry 拉取

`docker-compose.yml` 默认拉取公开镜像 `ghcr.io/krustd/qq-task-notifier:latest`，无需在目标机器构建、导出或导入镜像。公开镜像可匿名拉取，不需要执行 `docker login`。

推送到远程 `main` 会自动构建并发布 `latest`；推送版本标签（例如 `v0.1.0`）会额外发布对应版本镜像。首次发布后，在 GitHub 仓库的 **Packages** 中打开 `qq-task-notifier`，依次选择 **Package settings** → **Change visibility** → **Public**。

### 推荐：使用 `.env` 保存凭据

在部署主机执行：

```bash
cp .env.example .env
cp docker-compose.env.example.yml docker-compose.env.yml
# 编辑 .env，填写 QQBOT_APP_ID、QQBOT_APP_SECRET 与 QQBOT_API_TOKEN
docker compose -f docker-compose.yml -f docker-compose.env.yml pull
docker compose -f docker-compose.yml -f docker-compose.env.yml up -d
```

`.env` 与 `docker-compose.env.yml` 均已被 Git 忽略。`docker-compose.env.yml` 会以 `.env` 中的真实值覆盖基础 Compose 文件里的凭据占位符。

更新到最新镜像时，重复执行 `pull` 和 `up -d`。生产环境需要固定版本时，将 `docker-compose.yml` 中的 `:latest` 改为已发布的版本标签，例如 `:0.1.0`。

### 可选：覆盖特定部署的配置

在完成上述 `.env` 配置后，复制覆写模板并填写所需变量：

```bash
cp docker-compose.override.example.yml docker-compose.override.yml
docker compose -f docker-compose.yml -f docker-compose.env.yml -f docker-compose.override.yml up -d
```

最后加载的 `docker-compose.override.yml` 优先级最高，可覆盖基础 Compose 文件和环境模式文件中的配置。

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
