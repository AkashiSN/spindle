//! ハンドラとミドルウェアが共有する状態

use std::sync::Arc;

use crate::config::Config;
use crate::db::Db;
use crate::domain::selection::SelectionStore;
use crate::edit::Editor;
use crate::jobs::Jobs;
use crate::media::artwork::ArtworkStore;
use tokio_util::sync::CancellationToken;

use super::auth;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub db: Arc<Db>,
    pub auth: Arc<auth::Shared>,
    pub jobs: Arc<Jobs>,
    /// preview が固定した selection（token → スナップショット。D-33）
    pub selection: Arc<SelectionStore>,
    /// プロセス停止の合図。シグナルハンドラが倒し、HTTP サーバ・ワーカー・SSE が同時に見る
    pub shutdown: CancellationToken,
    /// 編集バッチの coordinator（P0-9）。ライブラリ root を要するので `with_editor` で後から載せる。
    /// 無いと一括編集の API は 503
    pub editor: Option<Arc<Editor>>,
    /// アートワークのキャッシュ（P1-3）。無いと `/api/artwork` は 503
    pub artwork: Option<Arc<ArtworkStore>>,
}

impl AppState {
    pub fn new(config: Arc<Config>, db: Arc<Db>, mode: auth::Mode) -> Self {
        let jobs = Jobs::new(Arc::clone(&db));
        Self {
            config,
            db,
            auth: Arc::new(auth::Shared::new(mode)),
            jobs,
            selection: Arc::new(SelectionStore::default()),
            shutdown: CancellationToken::new(),
            editor: None,
            artwork: None,
        }
    }

    pub fn with_artwork(mut self, store: Arc<ArtworkStore>) -> Self {
        self.artwork = Some(store);
        self
    }

    pub fn with_editor(mut self, editor: Arc<Editor>) -> Self {
        self.editor = Some(editor);
        self
    }
}
