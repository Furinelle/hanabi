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

# ---- runtime: alpine + gallery-dl + douyin-downloader 作者发现桥 ----
# gallery-dl 走 alpine community 仓库(自带, 依赖 python3 一并拉入), 免 pip 外部环境坑。
FROM alpine:3.20
ARG DOUYIN_DOWNLOADER_COMMIT=ad7338fdc474c14e2063370540a362cd8a953b43
RUN apk add --no-cache gallery-dl ca-certificates py3-pip py3-aiohttp py3-yaml \
    && apk add --no-cache --virtual .douyin-build-deps git py3-setuptools py3-wheel \
    && pip install --no-cache-dir --break-system-packages gmssl==3.2.2 \
    && pip install --no-cache-dir --break-system-packages --no-build-isolation --no-deps \
       "git+https://github.com/jiji262/douyin-downloader.git@${DOUYIN_DOWNLOADER_COMMIT}" \
    && apk del .douyin-build-deps
COPY --from=builder /hanabi /usr/local/bin/hanabi
COPY tools/douyin_user_feed.py /usr/local/lib/hanabi/douyin_user_feed.py
WORKDIR /data
# config.toml / gallery-dl.conf 经 volume 挂到 /data; token 经 -e 注入。
ENV HANABI_CONFIG=/data/config.toml
ENV HANABI_DOUYIN_HELPER=/usr/local/lib/hanabi/douyin_user_feed.py
ENTRYPOINT ["hanabi"]
