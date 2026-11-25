# Relais WebSocket E2EE autonome pour Zaimule (distroless nonroot, ~15 Mo RAM)
# Build : docker build -t zaimule-relay:latest .

FROM rust:1.90-alpine AS builder
WORKDIR /app
RUN apk add --no-cache musl-dev
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked --target x86_64-unknown-linux-musl

FROM gcr.io/distroless/static-debian12:nonroot
WORKDIR /
COPY --from=builder /app/target/x86_64-unknown-linux-musl/release/zaimule-relay /usr/local/bin/zaimule-relay

USER nonroot:nonroot
EXPOSE 3000

ENV RUST_LOG=info
ENTRYPOINT ["/usr/local/bin/zaimule-relay"]
