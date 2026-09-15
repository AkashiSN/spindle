# ---- web ----
FROM node:22-alpine AS web
WORKDIR /web
COPY web/package*.json ./
RUN npm ci
COPY web/ ./
RUN npm run build

# ---- server ----
FROM rust:1-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src/ src/
# rust-embed / include_str! でバイナリへ埋め込むものは build context に入れる
COPY db/migrations/ db/migrations/
COPY --from=web /web/dist/ web/dist/
RUN cargo build --release --locked

# ---- runtime ----
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
      ffmpeg flac opus-tools cd-paranoia cdrdao ca-certificates \
 && rm -rf /var/lib/apt/lists/*
# yt-dlp は apt 版が古いため単体バイナリを取得する。version を固定し、更新は
# イメージの再ビルドで行う（P3 で使う。無くても起動は通り、起動時診断で警告する）
ARG YTDLP_VERSION=2026.08.19
ADD --chmod=755 https://github.com/yt-dlp/yt-dlp/releases/download/${YTDLP_VERSION}/yt-dlp_linux /usr/local/bin/yt-dlp
COPY --from=build /src/target/release/spindle /usr/local/bin/spindle
ENV SPINDLE_CONFIG=/data/config.toml
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/spindle"]
