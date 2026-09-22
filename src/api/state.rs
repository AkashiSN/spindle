//! ハンドラとミドルウェアが共有する状態

use std::sync::Arc;

use crate::config::Config;
use crate::db::Db;
use crate::domain::selection::SelectionStore;
use crate::edit::Editor;
use crate::fsroot::RootDir;
use crate::gc::GcRoots;
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
    /// 起動時診断で取った yt-dlp の版（`/health` が出す。P4-12）。取れなければ None
    pub ytdlp_version: Option<Arc<str>>,
    /// 再生用の root（P1-9）。無いと `/api/stream` は 503
    pub library: Option<Arc<RootDir>>,
    pub derived: Option<Arc<RootDir>>,
    /// プレイリストの書き出し先・取り込み元（P1-6）。無いと export の POST と import は 503
    pub playlists: Option<Arc<RootDir>>,
    /// GC が触る root（P1-11）。無いと `/api/gc/preview` は 503
    pub gc: Option<Arc<GcRoots>>,
    /// Inbox の root（P2-10）。無いと `/api/inbox` は 503
    pub inbox: Option<Arc<RootDir>>,
    /// Inbox の周期監視の状態（最後に確認した時刻。P4-18）。監視が無ければ None
    pub inbox_watch: Option<Arc<crate::jobs::handlers::inbox::WatchStatus>>,
    /// オンザフライ変換の上限に足す猶予（トラック長 + これ。P1-9）
    pub transcode_grace: std::time::Duration,
    /// MusicBrainz の照会（P2-3）。無いと `/api/cd/lookup` は 503
    pub musicbrainz: Option<Arc<crate::cd::musicbrainz::MusicBrainzClient>>,
    /// CD ドライブ（P2-1）。ポーラが `DriveMonitor` を更新し、`/api/cd/status` が読む。
    /// eject はドライブを直接叩く。無いと `/api/cd/status` / `eject` は 503
    pub cd: Option<CdDrive>,
}

/// ドライブとその監視状態の組
#[derive(Clone)]
pub struct CdDrive {
    pub drive: Arc<dyn crate::cd::device::Drive>,
    pub monitor: Arc<crate::cd::device::DriveMonitor>,
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
            ytdlp_version: None,
            library: None,
            derived: None,
            playlists: None,
            gc: None,
            inbox: None,
            inbox_watch: None,
            transcode_grace: super::stream::TRANSCODE_GRACE,
            musicbrainz: None,
            cd: None,
        }
    }

    pub fn with_cd(
        mut self,
        drive: Arc<dyn crate::cd::device::Drive>,
        monitor: Arc<crate::cd::device::DriveMonitor>,
    ) -> Self {
        self.cd = Some(CdDrive { drive, monitor });
        self
    }

    pub fn with_musicbrainz(
        mut self,
        client: Arc<crate::cd::musicbrainz::MusicBrainzClient>,
    ) -> Self {
        self.musicbrainz = Some(client);
        self
    }

    #[doc(hidden)]
    pub fn with_transcode_grace(mut self, grace: std::time::Duration) -> Self {
        self.transcode_grace = grace;
        self
    }

    pub fn with_roots(mut self, library: Arc<RootDir>, derived: Arc<RootDir>) -> Self {
        self.library = Some(library);
        self.derived = Some(derived);
        self
    }

    pub fn with_playlists(mut self, playlists: Arc<RootDir>) -> Self {
        self.playlists = Some(playlists);
        self
    }

    pub fn with_gc(mut self, roots: Arc<GcRoots>) -> Self {
        self.gc = Some(roots);
        self
    }

    pub fn with_inbox(mut self, inbox: Arc<RootDir>) -> Self {
        self.inbox = Some(inbox);
        self
    }

    pub fn with_inbox_watch(
        mut self,
        watch: Arc<crate::jobs::handlers::inbox::WatchStatus>,
    ) -> Self {
        self.inbox_watch = Some(watch);
        self
    }

    pub fn with_artwork(mut self, store: Arc<ArtworkStore>) -> Self {
        self.artwork = Some(store);
        self
    }

    pub fn with_ytdlp_version(mut self, version: Option<String>) -> Self {
        self.ytdlp_version = version.map(Arc::from);
        self
    }

    pub fn with_editor(mut self, editor: Arc<Editor>) -> Self {
        self.editor = Some(editor);
        self
    }
}
