# Stage 1: Build static musl binary
FROM rust:1-alpine AS builder

# build-base (gcc + make + musl-dev) compiles the C sources pulled in by
# mlua (vendored Lua 5.4), libsqlite3-sys (bundled SQLite), and ring.
RUN apk add --no-cache build-base

WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src/ src/
# Compile-time inputs: sqlx::migrate!("./migrations") embeds the migrations and
# admin.rs include_str!("../web/admin.html") embeds the page — both must be
# present at build time or `cargo build` fails.
COPY migrations/ migrations/
COPY web/ web/

RUN cargo build --release

# Stage 2: Minimal runtime image
FROM alpine:3.21

RUN apk add --no-cache ca-certificates

COPY --from=builder /app/target/release/ai-api-bridge /usr/local/bin/ai-api-bridge

EXPOSE 8282

ENTRYPOINT ["ai-api-bridge"]
CMD ["--config", "/etc/ai-api-bridge/bridge.toml"]
