//! `GET /health` が 200 を返すこと（P0-1 の受け入れ条件）。

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use spindle::api;

#[tokio::test]
async fn health_returns_200_with_status_ok() {
    let app = api::router();
    let res = app
        .oneshot(
            Request::builder()
                .uri("/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let ct = res
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert!(ct.starts_with("application/json"), "content-type: {ct}");
    let body = res.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json, serde_json::json!({ "status": "ok" }));
}

#[tokio::test]
async fn unknown_api_route_returns_404() {
    let app = api::router();
    let res = app
        .oneshot(
            Request::builder()
                .uri("/api/nope")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn unknown_static_asset_returns_404() {
    let app = api::router();
    let res = app
        .oneshot(
            Request::builder()
                .uri("/assets/nope.js")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}
