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
COPY Cargo.toml Cargo.lock build.rs ./
COPY src/ src/
# rust-embed / include_str! でバイナリへ埋め込むものは build context に入れる
COPY db/migrations/ db/migrations/
COPY --from=web /web/dist/ web/dist/
# 版（`git describe --tags --always`）。build context に .git を入れないので外から渡す（CI と build.sh）。
# 無ければ build.rs が `dev` にする。`spindle --version` と `GET /health` の version が返す（P4-12）
ARG SPINDLE_VERSION
RUN SPINDLE_VERSION="$SPINDLE_VERSION" cargo build --release --locked

# ---- runtime ----
# yt-dlp は YouTube の抽出に JS ランタイムを要求する（deno 推奨）。公式の bin イメージから
# バイナリだけ取る。version を固定し、更新はイメージの再ビルドで行う（D-70）
FROM denoland/deno:bin-2.9.7 AS deno

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
      ffmpeg flac opus-tools cd-paranoia cdrdao ca-certificates \
 && rm -rf /var/lib/apt/lists/*
# yt-dlp は apt 版が古いため単体バイナリを取得する。version を固定し、更新は
# イメージの再ビルドで行う（P3-3 の ytdl ジョブが使う。無くても起動は通り、起動時診断で警告する）
# renovate: datasource=github-releases depName=yt-dlp/yt-dlp
ARG YTDLP_VERSION=2026.08.19
ADD --chmod=755 https://github.com/yt-dlp/yt-dlp/releases/download/${YTDLP_VERSION}/yt-dlp_linux /usr/local/bin/yt-dlp
COPY --from=deno /deno /usr/local/bin/deno
# メタデータプラグイン（spindle-ytmusic-meta）はイメージに入れない。static バイナリをホストに置き、
# compose で /usr/local/bin/spindle-ytmusic-meta にマウントする（D-70、deploy/compose.yaml）
COPY --from=build /src/target/release/spindle /usr/local/bin/spindle
ENV SPINDLE_CONFIG=/data/config.toml
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/spindle"]
