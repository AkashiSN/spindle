//! `web/dist` を `rust-embed` で同梱して配信する。
//! ルートに一致しないパスは SPA のクライアントルーティングのため `index.html` を返す。
//! `web/dist` が無いビルド（サーバ単体の開発時）では 404 を返す

use axum::body::Body;
use axum::http::{header, HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use rust_embed::Embed;

#[derive(Embed)]
#[folder = "web/dist/"]
struct Assets;

/// `path` に応じて返す index.html を選ぶ。`m` 段（`m` / `m/...`）だけスマホ版
fn index_for(path: &str) -> &'static str {
    if path.split('/').next() == Some("m") {
        "m/index.html"
    } else {
        "index.html"
    }
}

/// `/` をスマホの UA から開いたとき `/m/` へ振り分けるか。
/// `spindle_view=pc` の Cookie があれば PC 版を選んだ人なので振り分けない。
/// iPad は UA が Mac と同じで判別できないため PC 版のまま
pub fn should_redirect_to_mobile(path: &str, headers: &HeaderMap) -> bool {
    if !path.is_empty() {
        return false;
    }
    let is_mobile_ua = headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ua| ["Mobi", "Android", "iPhone"].iter().any(|k| ua.contains(k)));
    if !is_mobile_ua {
        return false;
    }
    let prefers_pc = headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .any(|pair| pair.trim() == "spindle_view=pc");
    !prefers_pc
}

pub async fn serve(uri: Uri, headers: HeaderMap) -> Response {
    let path = uri.path().trim_start_matches('/');
    if should_redirect_to_mobile(path, &headers) {
        return redirect(StatusCode::FOUND, "/m/");
    }
    // `/m` は相対の assets を解決させるため `/m/` へ寄せる
    if path == "m" {
        return redirect(StatusCode::MOVED_PERMANENTLY, "/m/");
    }
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
    let index_path = index_for(path);
    match Assets::get(index_path) {
        Some(index) => file_response(index_path, index),
        None => (
            StatusCode::NOT_FOUND,
            "SPA が同梱されていません（`cd web && npm run build` してから再ビルド）",
        )
            .into_response(),
    }
}

fn redirect(status: StatusCode, to: &'static str) -> Response {
    (status, [(header::LOCATION, to)]).into_response()
}

fn file_response(path: &str, file: rust_embed::EmbeddedFile) -> Response {
    let mime = mime_guess::from_path(path).first_or_octet_stream();
    (
        [(header::CONTENT_TYPE, mime.as_ref().to_string())],
        Body::from(file.data.into_owned()),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderMap, HeaderValue};

    fn ua(s: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(header::USER_AGENT, HeaderValue::from_str(s).unwrap());
        h
    }

    fn ua_cookie(s: &str, cookie: &str) -> HeaderMap {
        let mut h = ua(s);
        h.insert(header::COOKIE, HeaderValue::from_str(cookie).unwrap());
        h
    }

    #[test]
    fn redirects_only_root_for_mobile_ua_without_pc_cookie() {
        let iphone = "Mozilla/5.0 (iPhone; CPU iPhone OS 18_0 like Mac OS X) AppleWebKit/605.1.15 Mobile/15E148";
        let android = "Mozilla/5.0 (Linux; Android 15; XQ-EC54) AppleWebKit/537.36 Chrome/130 Mobile Safari/537.36";
        let mac =
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 Safari/605.1.15";
        assert!(should_redirect_to_mobile("", &ua(iphone)));
        assert!(should_redirect_to_mobile("", &ua(android)));
        assert!(!should_redirect_to_mobile("", &ua(mac))); // iPad もここ
        assert!(!should_redirect_to_mobile("index.html", &ua(iphone))); // `/` だけ
        assert!(!should_redirect_to_mobile("m/", &ua(iphone)));
        assert!(!should_redirect_to_mobile(
            "",
            &ua_cookie(iphone, "a=1; spindle_view=pc")
        ));
        assert!(should_redirect_to_mobile(
            "",
            &ua_cookie(iphone, "spindle_view=mobile")
        ));
        assert!(should_redirect_to_mobile(
            "",
            &ua_cookie(iphone, "xspindle_view=pc")
        ));
        assert!(!should_redirect_to_mobile("", &HeaderMap::new()));
    }

    #[test]
    fn index_for_picks_the_mobile_html_under_m() {
        assert_eq!(index_for("m"), "m/index.html");
        assert_eq!(index_for("m/"), "m/index.html");
        assert_eq!(index_for("m/playlists/3"), "m/index.html");
        assert_eq!(index_for(""), "index.html");
        assert_eq!(index_for("mobile"), "index.html"); // 前方一致ではなく段で見る
        assert_eq!(index_for("library"), "index.html");
    }
}
