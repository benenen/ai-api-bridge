# Stage 1: Build static musl binary
FROM rust:1.85-alpine AS builder

RUN apk add --no-cache musl-dev

WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src/ src/

RUN cargo build --release

# Stage 2: Minimal runtime image
FROM alpine:3.21

RUN apk add --no-cache ca-certificates

COPY --from=builder /app/target/release/ai-api-bridge /usr/local/bin/ai-api-bridge

EXPOSE 8282

ENTRYPOINT ["ai-api-bridge"]
CMD ["--config", "/etc/ai-api-bridge/bridge.toml"]
