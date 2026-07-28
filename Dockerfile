FROM ghcr.io/astral-sh/uv:python3.11-bookworm

ENV PYTHONDONTWRITEBYTECODE=1 \
    PYTHONUNBUFFERED=1 \
    UV_COMPILE_BYTECODE=1 \
    UV_LINK_MODE=copy \
    UV_CACHE_DIR=/tmp/uv-cache

WORKDIR /app

COPY pyproject.toml uv.lock ./
RUN uv sync --locked --no-dev

COPY main.py ./
RUN mkdir -p /app/data && chmod 700 /app/data

EXPOSE 8765

CMD ["/app/.venv/bin/python", "main.py"]
