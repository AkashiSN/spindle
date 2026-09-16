//! `GET /api/events`。単一チャネルの SSE。`event:` にイベント種別（job / batch / library）、
//! `data:` に JSON（SPEC §9）。セッション必須（trusted_cidrs の allowlist に含めない。D-27）。
//!
//! 購読者が遅れて broadcast の容量を超えたときは、読み飛ばした後続イベントより前に
//! `event: resync` を流す。クライアントはこれを受けたら一覧を再取得する（D-36）。
//! サーバ停止（shutdown token）でストリームを閉じる。閉じないと axum の graceful shutdown が
//! 接続の終了を待ち続ける

use std::convert::Infallible;
use std::time::Duration;

use axum::extract::State;
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use futures_core::Stream;
use futures_util::StreamExt;
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;
use tokio_stream::wrappers::BroadcastStream;

use super::AppState;

/// クライアントとの間の proxy に切られないための keep-alive 間隔
const KEEP_ALIVE: Duration = Duration::from_secs(15);

pub async fn stream(
    State(state): State<AppState>,
) -> Sse<impl Stream<Item = Result<SseEvent, Infallible>>> {
    let rx = state.jobs.subscribe();
    let shutdown = state.shutdown.clone();
    let stream = BroadcastStream::new(rx)
        .map(|item| match item {
            Ok(ev) => Ok(SseEvent::default()
                .event(ev.name())
                .data(ev.data().to_string())),
            Err(BroadcastStreamRecvError::Lagged(skipped)) => {
                tracing::debug!(skipped, "SSE 購読者が遅れてイベントを読み飛ばした");
                Ok(SseEvent::default()
                    .event("resync")
                    .data(serde_json::json!({ "skipped": skipped }).to_string()))
            }
        })
        .take_until(shutdown.cancelled_owned());
    Sse::new(stream).keep_alive(KeepAlive::new().interval(KEEP_ALIVE))
}
