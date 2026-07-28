import asyncio
import hashlib
import hmac
import json
import logging
import os
from pathlib import Path

from aiohttp import web
import botpy
from botpy.message import C2CMessage


BASE_DIR = Path(__file__).resolve().parent
OPENID_FILE = Path(os.environ.get("QQBOT_OPENID_FILE", BASE_DIR / "data" / "recipient_openid"))
MAX_CONTENT_LENGTH = 4000

logging.basicConfig(level=os.environ.get("LOG_LEVEL", "INFO"), format="%(asctime)s %(levelname)s %(message)s")
log = logging.getLogger("qq_task_notifier")


def read_required(name: str) -> str:
    value = os.environ.get(name, "").strip()
    if not value:
        raise RuntimeError(f"缺少环境变量: {name}")
    return value


def save_openid(openid: str) -> None:
    OPENID_FILE.parent.mkdir(parents=True, exist_ok=True)
    OPENID_FILE.write_text(openid + "\n", encoding="utf-8")
    OPENID_FILE.chmod(0o600)


def load_openid() -> str | None:
    if not OPENID_FILE.exists():
        return None
    value = OPENID_FILE.read_text(encoding="utf-8").strip()
    return value or None


def split_content(content: str) -> list[str]:
    return [content[i : i + MAX_CONTENT_LENGTH] for i in range(0, len(content), MAX_CONTENT_LENGTH)]


class QQClient(botpy.Client):
    async def on_ready(self):
        log.info("QQ Bot WebSocket 已连接，机器人=%s", self.robot.name)
        if load_openid():
            log.info("已加载保存的 recipient openid")

    async def on_c2c_message_create(self, message: C2CMessage):
        openid = message.author.user_openid
        save_openid(openid)
        log.info("已捕获并保存 C2C recipient openid")
        await message.reply(content="已绑定通知接收人。之后任务完成时会发送最后汇报。")


class Connector:
    def __init__(self, client: QQClient, api_token: str):
        self.client = client
        self.api_token = api_token

    def authenticate(self, request: web.Request) -> None:
        supplied = request.headers.get("Authorization", "")
        expected = f"Bearer {self.api_token}"
        if not hmac.compare_digest(
            hashlib.sha256(supplied.encode()).digest(),
            hashlib.sha256(expected.encode()).digest(),
        ):
            raise web.HTTPUnauthorized(
                text="需要 Authorization: Bearer <QQBOT_API_TOKEN>",
                headers={"WWW-Authenticate": "Bearer"},
            )

    async def send_message(self, content: str, openid: str | None = None) -> int:
        target = openid or load_openid()
        if not target:
            raise web.HTTPPreconditionFailed(text="尚未绑定 openid，请先向机器人发送一条消息。")
        if not content.strip():
            raise web.HTTPBadRequest(text="content 不能为空。")
        chunks = split_content(content)
        for chunk in chunks:
            await self.client.api.post_c2c_message(openid=target, content=chunk)
        return len(chunks)

    async def task_completed(self, request: web.Request) -> web.Response:
        self.authenticate(request)
        try:
            payload = await request.json()
        except json.JSONDecodeError as exc:
            raise web.HTTPBadRequest(text="请求体必须是 JSON。") from exc
        content = payload.get("summary", payload.get("content"))
        openid = payload.get("openid")
        if not isinstance(content, str):
            raise web.HTTPBadRequest(text="summary 或 content 必须是字符串。")
        if openid is not None and not isinstance(openid, str):
            raise web.HTTPBadRequest(text="openid 必须是字符串。")
        chunks = await self.send_message(content, openid)
        return web.json_response({"ok": True, "chunks": chunks})

    async def status(self, request: web.Request) -> web.Response:
        self.authenticate(request)
        return web.json_response({"connected": not self.client.is_closed(), "bound": load_openid() is not None})

    async def healthz(self, _: web.Request) -> web.Response:
        return web.json_response({"ok": True})


async def run() -> None:
    appid = read_required("QQBOT_APP_ID")
    secret = read_required("QQBOT_APP_SECRET")
    api_token = read_required("QQBOT_API_TOKEN")
    host = os.environ.get("QQBOT_HTTP_HOST", "0.0.0.0")
    port = int(os.environ.get("QQBOT_HTTP_PORT", "8765"))

    client = QQClient(
        intents=botpy.Intents(public_messages=True),
        bot_log=False,
        ext_handlers=False,
    )
    connector = Connector(client, api_token)
    server = web.Application(client_max_size=2 * 1024 * 1024)
    server.add_routes([
        web.get("/healthz", connector.healthz),
        web.get("/status", connector.status),
        web.post("/v1/messages", connector.task_completed),
        web.post("/task-completed", connector.task_completed),
    ])
    runner = web.AppRunner(server)
    await runner.setup()
    await web.TCPSite(runner, host, port).start()
    log.info("消息 API 监听于 http://%s:%s", host, port)

    try:
        await client.start(appid=appid, secret=secret)
    finally:
        await runner.cleanup()
        await client.close()


if __name__ == "__main__":
    try:
        asyncio.run(run())
    except KeyboardInterrupt:
        pass
