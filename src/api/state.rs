//! ハンドラとミドルウェアが共有する状態

use std::sync::Arc;

use crate::config::Config;
use crate::db::Db;
use crate::jobs::Jobs;
use tokio_util::sync::CancellationToken;

use super::auth;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub db: Arc<Db>,
    pub auth: Arc<auth::Shared>,
    pub jobs: Arc<Jobs>,
    /// プロセス停止の合図。シグナルハンドラが倒し、HTTP サーバ・ワーカー・SSE が同時に見る
    pub shutdown: CancellationToken,
}

impl AppState {
    pub fn new(config: Arc<Config>, db: Arc<Db>, mode: auth::Mode) -> Self {
        let jobs = Jobs::new(Arc::clone(&db));
        Self {
            config,
            db,
            auth: Arc::new(auth::Shared::new(mode)),
            jobs,
            shutdown: CancellationToken::new(),
        }
    }
}
