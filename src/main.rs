use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use tracing::info;

use spindle::api::{self, auth, AppState};
use spindle::cd::accuraterip::AccurateRipClient;
use spindle::cd::ctdb::CtdbClient;
use spindle::cd::musicbrainz::MusicBrainzClient;
use spindle::db::{migrations, Db};
use spindle::edit::{Editor, NormalizeEnv};
use spindle::fsroot::Roots;
use spindle::gc::GcRoots;
use spindle::import::inbox::PlaceItemEnv;
use spindle::import::scanner::Scanner;
use spindle::import::ytmusic::downloader::DownloaderEnv;
use spindle::import::ytmusic::MetadataProvider;
use spindle::jobs::handlers::backup::{self, BackupHandler};
use spindle::jobs::handlers::flaccheck::FlaccheckHandler;
use spindle::jobs::handlers::gc::{self as gc_job, GcHandler};
use spindle::jobs::handlers::hirescheck::HirescheckHandler;
use spindle::jobs::handlers::inbox::{self as inbox_job, InboxHandler};
use spindle::jobs::handlers::normalize::NormalizeHandler;
use spindle::jobs::handlers::playlist_sync::{self, PlaylistSyncHandler, SyncEnv};
use spindle::jobs::handlers::rename::RenameHandler;
use spindle::jobs::handlers::rg::RgHandler;
use spindle::jobs::handlers::scan::{self, ScanHandler};
use spindle::jobs::handlers::tagwrite::TagwriteHandler;
use spindle::jobs::handlers::thumbnail::ThumbnailHandler;
use spindle::jobs::handlers::transcode::{self, TranscodeHandler};
use spindle::jobs::handlers::verify::VerifyHandler;
use spindle::jobs::handlers::ytdl::YtdlHandler;
use spindle::jobs::{self, EnqueueResult, JobType, Registry};
use spindle::media::artwork::ArtworkStore;
use spindle::media::decode::Decoder;
use spindle::media::encode::{AacEncoder, FlacEncoder, OpusEncoder};
use spindle::playlist::autoexport::AutoExport;
use spindle::{config::Config, logging};

/// `SPINDLE_CONFIG` 未設定時の設定ファイルパス（SPEC §14 環境変数）
const DEFAULT_CONFIG_PATH: &str = "/data/config.toml";
/// `[paths].data` 直下の DB ファイル名（SPEC §5）
const DB_FILE_NAME: &str = "spindle.db";
/// `[paths].data` 直下の変換作業領域（SPEC §5）
const TMP_DIR_NAME: &str = "tmp";
/// `[paths].data` 直下のアートワークキャッシュ（SPEC §5、P1-3）
const THUMBS_DIR_NAME: &str = "thumbs";
/// 初回起動時の管理パスワード（SPEC §14 環境変数、D-28）
const INITIAL_PASSWORD_ENV: &str = "SPINDLE_INITIAL_PASSWORD";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // `spindle --version` は版だけ出して終わる（設定を読まない。P4-12）。他の引数は取らない
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [] => {}
        [flag] if flag == "--version" || flag == "-V" => {
            println!("spindle {}", spindle::version::VERSION);
            return Ok(());
        }
        _ => {
            eprintln!(
                "不明な引数: {}（受けるのは --version だけ。設定は SPINDLE_CONFIG）",
                args.join(" ")
            );
            std::process::exit(2);
        }
    }
    logging::init();
    info!(version = spindle::version::VERSION, "spindle を起動する");

    let config_path = std::env::var_os("SPINDLE_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_CONFIG_PATH));
    let config = Config::load(&config_path)
        .with_context(|| format!("設定の読み込みに失敗: {}", config_path.display()))?;
    info!(config = %config_path.display(), library = %config.paths.library.display(), "設定を読み込んだ");
    // 外部プログラムの実行可否（無くても起動は通す。使うジョブが失敗する。D-70）
    for missing in config.probe_executables() {
        tracing::warn!("{missing}");
    }
    // yt-dlp の版（/health が出す。古いと YouTube の抽出が壊れるので実機で確認できるように。P4-12）
    let ytdlp_version = probe_ytdlp_version(&config.bin.ytdlp).await;
    match &ytdlp_version {
        Some(v) => info!(ytdlp = %v, "yt-dlp の版"),
        None => tracing::warn!("yt-dlp の版を取れない（無いか --version が失敗）"),
    }
    // 全 root を dirfd で開く。openat2 が無い（Linux 5.6 未満）ならここで止まる（D-31）
    let roots = Roots::open(&config.paths).context("ライブラリの root を開けない")?;
    let library_root = Arc::new(roots.library);
    let archive_root = Arc::new(roots.archive);
    let derived_root = Arc::new(roots.derived);
    let playlists_root = Arc::new(roots.playlists);
    let inbox_root = Arc::new(roots.inbox);

    let db_path = config.paths.data.join(DB_FILE_NAME);
    let db = {
        let path = db_path.clone();
        tokio::task::spawn_blocking(move || Db::open(&path))
            .await
            .context("DB を開くタスクが異常終了")?
            .with_context(|| format!("DB を開けない: {}", db_path.display()))?
    };
    let version = db
        .read(|conn| Ok(migrations::current_version(conn)?))
        .await
        .context("スキーマ版の読み取りに失敗")?;
    info!(db = %db_path.display(), schema_version = ?version, "DB を開いた");
    let db = Arc::new(db);

    // 初期パスワードは DB に無い初回起動だけ読む（D-28）。読んだ後は環境から消す
    let initial_password = std::env::var(INITIAL_PASSWORD_ENV).ok();
    std::env::remove_var(INITIAL_PASSWORD_ENV);
    let mode = auth::bootstrap(&db, initial_password)
        .await
        .context("認証の初期化に失敗")?;

    // 起動時リカバリ: running → queued、track_locks 全削除（SPEC §8）。ワーカー起動より前
    let recovered = jobs::recovery::run(&db)
        .await
        .context("ジョブのリカバリに失敗")?;
    info!(
        requeued = recovered.requeued,
        locks_cleared = recovered.locks_cleared,
        "ジョブをリカバリした"
    );

    // Derived の系統設定は config を正とし、起動時に derived_variants へ写す（D-75）
    {
        let derived_cfg = config.encode.derived.clone();
        db.write(move |c| {
            spindle::db::derived::sync_variants(c, &derived_cfg, spindle::db::now_epoch())
        })
        .await
        .context("derived_variants の更新に失敗")?;
        info!(
            opus_enabled = config.encode.derived.opus.enabled,
            opus_bitrate = config.encode.derived.opus.bitrate,
            aac_enabled = config.encode.derived.aac.enabled,
            aac_bitrate = config.encode.derived.aac.bitrate,
            "Derived の系統設定を揃えた"
        );
    }

    // foobar プロファイルの UNC prefix は config を正とする（D-55）
    let fb2k_prefix = config.export.fb2k_prefix.clone();
    if db
        .write(move |c| spindle::db::playlists::sync_foobar_prefix(c, &fb2k_prefix))
        .await
        .context("export_profiles の更新に失敗")?
    {
        info!(prefix = %config.export.fb2k_prefix, "foobar プロファイルの prefix を config に揃えた");
    }

    let listen = config.server.listen;
    let mut state = AppState::new(Arc::new(config), db, mode).with_ytdlp_version(ytdlp_version);

    // 停止シグナルは共有 token を倒す。HTTP サーバ・ワーカー・SSE ストリームが同時に止まる
    // （SSE を先に閉じないと axum の graceful shutdown が接続の終了を待ち続ける）
    let shutdown = state.shutdown.clone();
    tokio::spawn({
        let shutdown = shutdown.clone();
        async move {
            shutdown_signal().await;
            shutdown.cancel();
        }
    });
    // アートワークのキャッシュ（P1-3）。スキャナが原画像を置き、thumbnail ジョブが WebP を作る。
    // 書き側（D-60）はアップロードと退避に使う
    let artwork = Arc::new(ArtworkStore::new(
        state.config.paths.data.join(THUMBS_DIR_NAME),
    ));
    state = state.with_artwork(Arc::clone(&artwork));
    // 編集バッチの coordinator。起動時リカバリ（pending op の track ジョブ再投入）は
    // ジョブのリカバリの後・ワーカー起動の前（SPEC §7.5）
    let editor = Arc::new(
        Editor::new(
            Arc::clone(&state.db),
            Arc::clone(&library_root),
            Arc::clone(&state.jobs),
        )
        // ロスレス正規化（P1-4）。作業領域は data/tmp、退避先は Archive root
        .with_normalize(NormalizeEnv {
            archive: Arc::clone(&archive_root),
            encoder: FlacEncoder::new(
                &state.config.bin.ffmpeg,
                &state.config.bin.flac,
                state.config.encode.flac_compression,
                state.config.paths.data.join(TMP_DIR_NAME),
            ),
            retention_days: state.config.gc.retention_days,
        })
        // ReplayGain のタグ変換と rg_written_at の判定の基準（P1-2）
        .with_replaygain_reference(state.config.replaygain.reference_lufs)
        // 埋め込み画像の差し替え（P1-3 書き側、D-60）
        .with_artwork(Arc::clone(&artwork)),
    );
    state = state.with_editor(Arc::clone(&editor));
    // 再生（P1-9）。原本と Derived を Range で直送する
    state = state.with_roots(Arc::clone(&library_root), Arc::clone(&derived_root));
    // プレイリストの書き出し先・取り込み元（P1-6）
    state = state.with_playlists(Arc::clone(&playlists_root));
    let edit_recovered = editor
        .recover()
        .await
        .context("編集バッチのリカバリに失敗")?;
    info!(
        requeued = edit_recovered.requeued,
        cancelled_ops = edit_recovered.cancelled_ops,
        "編集バッチをリカバリした"
    );

    // ハンドラは各タスクで登録する
    let cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2);
    let scanner = Arc::new(
        Scanner::new(Arc::clone(&state.db), Arc::clone(&library_root), cpus)
            .with_artwork(Arc::clone(&artwork))
            .with_replaygain_reference(state.config.replaygain.reference_lufs),
    );
    let mut registry = Registry::new();
    registry.register(
        JobType::Scan,
        Arc::new(
            ScanHandler::new(scanner, state.config.scan.deep_interval_days)
                .with_flac_verify(state.config.normalize.flac_verify_on_import)
                .with_hires_check(state.config.hires.check_on_import),
        ),
    );
    // FLAC 健全性チェック（P1-5、D-57）。読むだけ
    registry.register(
        JobType::Flaccheck,
        Arc::new(FlaccheckHandler::new(
            Arc::clone(&library_root),
            &state.config.bin.flac,
        )),
    );
    // 偽ハイレゾ検出（P3-5、D-71）。読むだけ。デコーダは rg と同じもの
    registry.register(
        JobType::Hirescheck,
        Arc::new(HirescheckHandler::new(
            Arc::clone(&library_root),
            Decoder::new(&state.config.bin.ffmpeg),
            spindle::media::hires::Thresholds {
                cutoff_hz: state.config.hires.cutoff_hz,
                cliff_db: state.config.hires.cliff_db,
                hard_cutoff_hz: state.config.hires.hard_cutoff_hz,
            },
        )),
    );
    // 遡及照合（P2-9、D-13 / D-63）。読むだけ。ログは data/verify/
    let ua = &state.config.musicbrainz.user_agent;
    let ar_client = AccurateRipClient::new(&state.config.verify.accuraterip_url, ua)
        .context("AccurateRip クライアントの初期化に失敗")?;
    let ctdb_client = CtdbClient::new(&state.config.verify.ctdb_url, ua)
        .context("CTDB クライアントの初期化に失敗")?;
    registry.register(
        JobType::Verify,
        Arc::new(VerifyHandler::new(
            Arc::clone(&library_root),
            &state.config.paths.data,
            ar_client,
            ctdb_client,
        )),
    );
    registry.register(
        JobType::Tagwrite,
        Arc::new(TagwriteHandler::new(Arc::clone(&editor))),
    );
    registry.register(
        JobType::Rename,
        Arc::new(RenameHandler::new(Arc::clone(&editor))),
    );
    registry.register(
        JobType::Normalize,
        Arc::new(NormalizeHandler::new(Arc::clone(&editor))),
    );
    // ReplayGain 解析（P1-1）。Opus は ffmpeg でデコードする
    registry.register(
        JobType::Rg,
        Arc::new(RgHandler::new(
            Arc::clone(&library_root),
            Decoder::new(&state.config.bin.ffmpeg),
            state.config.replaygain.reference_lufs,
        )),
    );
    registry.register(
        JobType::Thumbnail,
        Arc::new(ThumbnailHandler::new(
            Arc::clone(&artwork),
            &state.config.bin.ffmpeg,
        )),
    );
    // Derived の生成と追随（P1-10、D-51）。作業領域は data/tmp、画像は thumbs のキャッシュ。
    // 前回の強制終了で残った作業ファイルはワーカーを起こす前に回収する
    let swept = transcode::sweep_tmp(&state.config.paths.data.join(TMP_DIR_NAME), &derived_root);
    if swept != transcode::SweepReport::default() {
        info!(?swept, "取り残された作業ファイルを回収した");
    }
    // GC（P1-11、D-56）。物理削除を行う唯一の経路。dry-run は GET /api/gc/preview
    let gc_roots = Arc::new(GcRoots {
        library: Arc::clone(&library_root),
        archive: Arc::clone(&archive_root),
        derived: Arc::clone(&derived_root),
        artwork: Arc::clone(&artwork),
    });
    state = state.with_gc(Arc::clone(&gc_roots));
    // MusicBrainz の照会（P2-3）。UA 必須・1 req/s
    let mb = MusicBrainzClient::new(
        &state.config.musicbrainz.url,
        &state.config.musicbrainz.user_agent,
        std::time::Duration::from_secs(1) / state.config.musicbrainz.rate_limit_per_sec.max(1),
    )
    .context("MusicBrainz クライアントの初期化に失敗")?;
    state = state.with_musicbrainz(Arc::new(mb));
    registry.register(
        JobType::Gc,
        Arc::new(GcHandler::new(
            Arc::clone(&state.db),
            gc_roots,
            i64::from(state.config.gc.retention_days) * 86_400,
        )),
    );
    registry.register(
        JobType::Transcode,
        Arc::new(TranscodeHandler::new(
            Arc::clone(&library_root),
            derived_root,
            OpusEncoder::new(
                &state.config.bin.ffmpeg,
                &state.config.bin.opusenc,
                state.config.encode.derived.opus.bitrate,
                state.config.paths.data.join(TMP_DIR_NAME),
            ),
            AacEncoder::new(
                &state.config.bin.ffmpeg,
                state.config.encode.derived.aac.bitrate,
                state.config.paths.data.join(TMP_DIR_NAME),
            ),
            Arc::clone(&artwork),
            &state.config.bin.ffmpeg,
            state.config.replaygain.reference_lufs,
        )),
    );
    registry.register(
        JobType::Backup,
        Arc::new(BackupHandler::new(
            state.config.paths.data.join(backup::BACKUP_DIR_NAME),
            state.config.backup.retention_generations,
        )),
    );
    // Inbox 取り込み（P2-10、D-68）。走査と承認済みの配置を 1 本のジョブで
    state = state.with_inbox(Arc::clone(&inbox_root));
    registry.register(
        JobType::Inbox,
        Arc::new(InboxHandler::new(PlaceItemEnv {
            db: Arc::clone(&state.db),
            library: Arc::clone(&library_root),
            inbox: Arc::clone(&inbox_root),
            jobs: Arc::clone(&state.jobs),
            layout: state.config.layout.clone(),
            editor: Some(Arc::clone(&editor)),
            wav_to_flac: state.config.normalize.wav_to_flac,
            before_place: None,
            artwork: Some(Arc::clone(&artwork)),
            before_artwork: None,
        })),
    );
    // YouTube のダウンロード（P3-3、D-70）。Inbox に置くところまで。無効なら登録しない（API は 404）
    if state.config.ytmusic.enabled {
        let ytdl_tmp = state.config.paths.data.join(TMP_DIR_NAME).join("ytdl");
        let swept = spindle::import::ytmusic::downloader::sweep_tmp(&ytdl_tmp);
        if swept > 0 {
            info!(swept, "ytdl の作業領域の残りを消した");
        }
        let provider = MetadataProvider::new(
            &state.config.ytmusic.metadata_command,
            std::time::Duration::from_secs(u64::from(state.config.ytmusic.metadata_timeout_secs)),
        )
        .context("ytmusic.metadata_command が空")?;
        registry.register(
            JobType::Ytdl,
            Arc::new(YtdlHandler::new(DownloaderEnv {
                db: Arc::clone(&state.db),
                inbox: Arc::clone(&inbox_root),
                archive: Arc::clone(&archive_root),
                jobs: Arc::clone(&state.jobs),
                provider,
                ytdlp: state.config.ytdlp_command(),
                ffmpeg: PathBuf::from(&state.config.bin.ffmpeg),
                tmp_root: ytdl_tmp,
                download_timeout: std::time::Duration::from_secs(u64::from(
                    state.config.ytmusic.download_timeout_secs,
                )),
            })),
        );
        // 再生リストの購読の同期（P4-16、D-78）。列挙 → 番号揃え → ytdl 投入
        let (pending_wait, pending_poll) = SyncEnv::default_waits();
        registry.register(
            JobType::PlaylistSync,
            Arc::new(PlaylistSyncHandler::new(SyncEnv {
                db: Arc::clone(&state.db),
                jobs: Arc::clone(&state.jobs),
                editor: Arc::clone(&editor),
                layout: state.config.layout.clone(),
                ytdlp: state.config.ytdlp_command(),
                pending_wait,
                pending_poll,
            })),
        );
    }
    let worker = state.jobs.start(registry, shutdown.clone());
    // 購読の dispatcher（latch の回収と定期同期）。ytmusic が無効なら回さない
    let subscription_dispatcher = state.config.ytmusic.enabled.then(|| {
        playlist_sync::spawn_dispatcher(
            Arc::clone(&state.jobs),
            state.config.ytmusic.sync_interval_hours,
            std::time::Duration::from_secs(30),
            shutdown.clone(),
        )
    });
    // Inbox の周期検出（0 で無し）
    let inbox_scheduler = inbox_job::spawn_scheduler(
        Arc::clone(&state.jobs),
        i64::from(state.config.inbox.poll_interval_secs),
        shutdown.clone(),
    );
    // 定期バックアップ（SPEC §14）。最後の終端 backup から interval_hours 経っていれば投入する
    let backup_scheduler = backup::spawn_scheduler(
        Arc::clone(&state.jobs),
        state.config.backup.interval_hours,
        shutdown.clone(),
    );
    // 定期 GC（D-56）。最後の終端 gc から 24 時間経っていれば投入する
    let gc_scheduler = gc_job::spawn_scheduler(Arc::clone(&state.jobs), shutdown.clone());
    // スマートプレイリストの自動再評価と、記録済みプロファイルへの自動再書き出し（P1-7、D-54）
    let autoexport = AutoExport::new(
        Arc::clone(&state.db),
        Arc::clone(&state.jobs),
        Arc::clone(&playlists_root),
        std::time::Duration::from_secs(u64::from(state.config.export.autoexport_debounce_sec)),
    )
    .spawn(shutdown.clone());
    // 起動時に 1 回 incremental を投入する（停止中の外部変更を拾う。D-38）
    match scan::enqueue_scan(&state.jobs, "incremental").await {
        Ok(EnqueueResult::Inserted(id)) => info!(job_id = id, "起動時スキャンを投入した"),
        Ok(EnqueueResult::Duplicate(id)) => info!(job_id = id, "スキャンは既に投入済み"),
        Err(e) => tracing::warn!(error = %e, "起動時スキャンを投入できない"),
    }
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("待ち受けに失敗: {listen}"))?;
    info!(%listen, ?mode, "HTTP サーバを開始");
    // 接続元アドレスを ConnectInfo で渡す（trusted_cidrs / trusted_proxies / レート制限の判定に使う）
    axum::serve(
        listener,
        api::router(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown.clone().cancelled_owned())
    .await
    .context("HTTP サーバが異常終了")?;
    // ワーカーは新規 claim を止め、実行中は破棄済み（次回起動のリカバリで queued に戻る）
    let _ = worker.await;
    let _ = backup_scheduler.await;
    let _ = gc_scheduler.await;
    let _ = inbox_scheduler.await;
    if let Some(h) = subscription_dispatcher {
        let _ = h.await;
    }
    let _ = autoexport.await;
    info!("停止した");
    Ok(())
}

/// SIGINT / SIGTERM で graceful shutdown する（コンテナ停止時に SIGTERM が来る）
async fn shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let ctrl_c = tokio::signal::ctrl_c();
    let mut term = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "SIGTERM ハンドラを登録できない。Ctrl-C のみ待つ");
            let _ = ctrl_c.await;
            return;
        }
    };
    tokio::select! {
        _ = ctrl_c => {},
        _ = term.recv() => {},
    }
    info!("停止シグナルを受け取った");
}

/// `yt-dlp --version` を 1 回だけ叩く（5 秒で諦める）。1 行目を版として返す
async fn probe_ytdlp_version(program: &str) -> Option<String> {
    let out = spindle::jobs::process::ExternalCommand::new(program)
        .arg("--version")
        .timeout(std::time::Duration::from_secs(5))
        .run(&tokio_util::sync::CancellationToken::new())
        .await
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text.lines().next()?.trim();
    (!line.is_empty()).then(|| line.to_owned())
}
