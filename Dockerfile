# ---- builder: 与目标平台同架构编译 musl 静态二进制 ----
# build-base 提供 gcc/musl-dev, 供 rusqlite(bundled) 编译 sqlite3.c。
FROM rust:1.89-alpine3.20 AS builder
RUN apk add --no-cache build-base
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked --bins \
    && cp target/release/hanabi /hanabi \
    && cp target/release/gallery_repair /gallery_repair \
    && cp target/release/restore_pending /restore_pending

# ---- runtime: alpine + gallery-dl + douyin-downloader 作者发现桥 ----
# gallery-dl 走 alpine community 仓库(自带, 依赖 python3 一并拉入), 免 pip 外部环境坑。
FROM alpine:3.20
ARG DOUYIN_DOWNLOADER_COMMIT=ad7338fdc474c14e2063370540a362cd8a953b43
ARG VCS_REF=unknown
LABEL org.opencontainers.image.source="https://github.com/Furinelle/hanabi" \
      org.opencontainers.image.revision="${VCS_REF}"
RUN apk add --no-cache gallery-dl ca-certificates py3-pip py3-aiohttp py3-yaml \
    && apk add --no-cache --virtual .douyin-build-deps git py3-setuptools py3-wheel \
    && pip install --no-cache-dir --break-system-packages gmssl==3.2.2 \
    && pip install --no-cache-dir --break-system-packages --no-build-isolation --no-deps \
       "git+https://github.com/jiji262/douyin-downloader.git@${DOUYIN_DOWNLOADER_COMMIT}" \
    && apk del .douyin-build-deps \
    && addgroup -S -g 10001 hanabi \
    && adduser -S -D -h /home/hanabi -u 10001 -G hanabi hanabi \
    && install -d -o hanabi -g hanabi -m 0700 /data
COPY --from=builder /hanabi /usr/local/bin/hanabi
COPY --from=builder /gallery_repair /usr/local/bin/gallery_repair
COPY --from=builder /restore_pending /usr/local/bin/restore_pending
COPY tools/douyin_user_feed.py /usr/local/lib/hanabi/douyin_user_feed.py
WORKDIR /data
# config.toml / gallery-dl.conf 经 volume 挂到 /data; token 经 -e 注入。
ENV HANABI_CONFIG=/data/config.toml
ENV HANABI_DOUYIN_HELPER=/usr/local/lib/hanabi/douyin_user_feed.py
ENV HOME=/home/hanabi
USER hanabi:hanabi
STOPSIGNAL SIGTERM
ENTRYPOINT ["hanabi"]
