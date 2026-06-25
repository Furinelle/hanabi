# ---- builder: alpine(musl) 上编静态二进制 ----
# rust:alpine 默认 target 即 x86_64-unknown-linux-musl, 产物静态链接。
# build-base 提供 gcc/musl-dev, 供 rusqlite(bundled) 编译 sqlite3.c。
FROM rust:alpine AS builder
RUN apk add --no-cache build-base
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY tests ./tests
RUN cargo build --release && cp target/release/hanabi /hanabi

# ---- runtime: alpine + gallery-dl ----
# gallery-dl 走 alpine community 仓库(自带, 依赖 python3 一并拉入), 免 pip 外部环境坑。
FROM alpine:3.20
RUN apk add --no-cache gallery-dl ca-certificates
COPY --from=builder /hanabi /usr/local/bin/hanabi
WORKDIR /data
# config.toml / gallery-dl.conf 经 volume 挂到 /data; token 经 -e 注入。
ENV HANABI_CONFIG=/data/config.toml
ENTRYPOINT ["hanabi"]
