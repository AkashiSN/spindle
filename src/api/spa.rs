//! `web/dist` を `rust-embed` で同梱して配信する。
//! ルートに一致しないパスは SPA のクライアントルーティングのため `index.html` を返す。
//! `web/dist` が無いビルド（サーバ単体の開発時）では 404 を返す

use axum::body::Body;
use axum::http::{header, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use rust_embed::Embed;

#[derive(Embed)]
#[folder = "web/dist/"]
struct Assets;

pub async fn serve(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    if let Some(file) = Assets::get(path) {
        return file_response(path, file);
    }
    // 拡張子つきのパス（`/assets/x.js` 等）は静的ファイル要求なので index.html に倒さない
    let looks_like_asset = path
        .rsplit('/')
        .next()
        .is_some_and(|last| last.contains('.'));
    if looks_like_asset {
        return StatusCode::NOT_FOUND.into_response();
    }
    match Assets::get("index.html") {
        Some(index) => file_response("index.html", index),
        None => (
            StatusCode::NOT_FOUND,
            "SPA が同梱されていません（`cd web && npm run build` してから再ビルド）",
        )
            .into_response(),
    }
}

fn file_response(path: &str, file: rust_embed::EmbeddedFile) -> Response {
    let mime = mime_guess::from_path(path).first_or_octet_stream();
    (
        [(header::CONTENT_TYPE, mime.as_ref().to_string())],
        Body::from(file.data.into_owned()),
    )
        .into_response()
}
