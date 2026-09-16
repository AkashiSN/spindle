//! ハンドラとミドルウェアが共有する状態

use std::sync::Arc;

use crate::config::Config;
use crate::db::Db;

use super::auth;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub db: Arc<Db>,
    pub auth: Arc<auth::Shared>,
}

impl AppState {
    pub fn new(config: Arc<Config>, db: Arc<Db>, mode: auth::Mode) -> Self {
        Self {
            config,
            db,
            auth: Arc::new(auth::Shared::new(mode)),
        }
    }
}
