FROM rust:1.97-bookworm AS builder

WORKDIR /src
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && printf 'fn main() {}\n' > src/main.rs && cargo build --release --locked && rm -rf src
COPY src ./src
RUN touch src/main.rs && cargo build --release --locked
RUN mkdir -p /runtime/data && chown -R 65532:65532 /runtime

FROM gcr.io/distroless/cc-debian12:nonroot

WORKDIR /app
COPY --from=builder --chown=65532:65532 /src/target/release/qq-task-notifier /app/qq-task-notifier
COPY --from=builder --chown=65532:65532 /runtime/data /app/data
USER nonroot:nonroot
EXPOSE 8765
ENTRYPOINT ["/app/qq-task-notifier"]
