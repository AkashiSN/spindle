//! `GET /health`。認証の外に置く唯一のルート。
//! P0-3 でロックモード時に `{"status":"locked"}` を返すようにする

use axum::Json;
use serde::Serialize;

#[derive(Serialize)]
pub struct Health {
    pub status: &'static str,
}

pub async fn get() -> Json<Health> {
    Json(Health { status: "ok" })
}
