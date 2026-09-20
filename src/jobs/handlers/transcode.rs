//! `transcode` ジョブ（SPEC §7.6 / §8、D-8 / D-9 / D-22 / D-25 / D-51、P1-10）。payload は
//! `{"track_id", "audio_version", "tag_version"}`、dedup `transcode:<track_id>:<audio_version>`。
//! 基盤が `audio_version` で stale 判定して track lock を取ってから始まる。
//!
//! 「そのトラックの Derived を現在値に揃える」ハンドラ。payload の版は使わず、毎回 DB の現在値
//! から [`plan`] で必要な処理を決める（投入側が retag / move / encode を判定しない。D-51）:
//!
//! - `Skip` / `UpToDate`: 何もしない
//! - `Encode`: Library の FD を開き、**fstat が DB の行と一致する**ことを確かめてから（パスは
//!   識別子ではない）ffmpeg → opusenc で tmp に作り、タグ・画像を書き、Derived の宛先ディレクトリの
//!   tmp へコピーして `replace_file`。旧パスに自分の Derived があれば消す。DB は配置後に upsert
//! - `Move`: `rename_noreplace`。宛先に何かあれば消してから（Derived は再生成物）。元が無ければ
//!   `Encode` に倒す
//! - `Retag`: Derived を同じディレクトリの tmp にコピーし、タグ・画像を書いて `replace_file`。
//!   Derived が消えていれば `Encode` に倒す
//!
//! 期待パスを**別トラック**の `derived_files` 行が持っていれば、その行が missing のときだけ行を消して
//! 上書きする。生きていれば失敗（`x.flac` と `x.wav` のように別の Library パスが同じ期待パスに写る
//! とき）。期待パスは **`derived_path_locks` で排他予約**してから触る（別のジョブが持っていれば
//! 再キュー。同じ宛先へ 2 本のジョブが書くことも、先に走っている retag の後から別のジョブが同じ
//! パスを置き換えることもない）。占有の確定（`claim_path`）は予約の下で、物理的な書き込みの前に
//! 1 トランザクションで行う（scanner は track lock を取らないので、確認と書き込みの間に占有側が
//! 復活しうる）。
//! cancel は Library を読む前と Derived に置く前で見る。tmp は失敗・cancel で消す。SIGKILL / 電源断で
//! 残った tmp は起動時に [`sweep_tmp`] が回収する

use std::fs::File;
use std::io::{Seek as _, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use lofty::picture::{MimeType, Picture, PictureType};

use crate::db::artwork as dbart;
use crate::db::derived::{self as dbderived, TagState};
use crate::db::replaygain::{self as dbrg, Member};
use crate::db::{now_epoch, DbError};
use crate::domain::derived::{expected_rel_path, opus_tags, plan, Current, Plan, Target};
use crate::domain::relpath::{canonical_key, RelPath};
use crate::domain::replaygain::Values;
use crate::domain::tags::{read_transfer_tags, write_opus_tags, TransferTags};
use crate::fsroot::{self, FsError, RootDir};
use crate::jobs::handlers::thumbnail::make_thumb;
use crate::jobs::{BoxFuture, Handler, HandlerResult, JobContext, JobError, Outcome};
use crate::media::artwork::ArtworkStore;
use crate::media::encode::OpusEncoder;

/// Derived に埋める画像の一辺（`THUMB_SIZES` の大きい方）
const COVER_SIZE: u32 = 768;
/// Derived の codec 列
const DERIVED_CODEC: &str = "opus";

/// テストが競合を差し込むためのフック（`"reserved"` = 期待パスの排他予約を取った直後、
/// `"claimed"` = 占有を DB で確定した直後、`"retag_before_write"` = retag が Library を読み終えて
/// Derived を書く直前。どれも物理的な書き込みの前）。本番では使わない
pub type TestHook = Arc<dyn Fn(&'static str) -> BoxFuture<'static, ()> + Send + Sync>;

pub struct TranscodeHandler {
    library: Arc<RootDir>,
    derived: Arc<RootDir>,
    encoder: OpusEncoder,
    artwork: Arc<ArtworkStore>,
    ffmpeg: PathBuf,
    reference_lufs: f64,
    test_hook: Option<TestHook>,
}

impl TranscodeHandler {
    pub fn new(
        library: Arc<RootDir>,
        derived: Arc<RootDir>,
        encoder: OpusEncoder,
        artwork: Arc<ArtworkStore>,
        ffmpeg: impl AsRef<Path>,
        reference_lufs: f64,
    ) -> Self {
        Self {
            library,
            derived,
            encoder,
            artwork,
            ffmpeg: ffmpeg.as_ref().to_path_buf(),
            reference_lufs,
            test_hook: None,
        }
    }

    #[doc(hidden)]
    pub fn with_test_hook(mut self, hook: TestHook) -> Self {
        self.test_hook = Some(hook);
        self
    }

    async fn hook(&self, point: &'static str) {
        if let Some(h) = &self.test_hook {
            h(point).await;
        }
    }
}

/// 解決したトラックの状態（1 回の読み取りで揃える）
struct Resolved {
    target: Target,
    current: Option<Current>,
    /// Library の行の物理属性（開いた FD と照合する）
    member: Member,
    rg: Option<Values>,
    bit_depth: Option<u32>,
}

/// `do_move` の結果
enum Step {
    Moved,
    /// 元のファイルが無い（呼び出し側が再エンコードする）
    SourceMissing,
    /// 期待パスを、追随待ちの生きているトラックが持っている（再キュー）
    Blocked,
}

/// 期待パスの占有の確定の結果
enum Claim {
    Claimed,
    /// 生きているトラックが自分の期待パスとは違うパスとして持っている（Library の swap / 循環
    /// rename の途中）。相手の transcode が動いていれば待ち、いなければ失敗（次の scan で両方
    /// 投入される）
    Blocked {
        holder: i64,
        holder_busy: bool,
    },
}

fn failed(msg: impl Into<String>) -> JobError {
    JobError::Failed(anyhow::anyhow!(msg.into()))
}

fn fs_failed(what: &str, e: FsError) -> JobError {
    failed(format!("{what}: {e}"))
}

fn parse_rel(s: &str) -> Result<RelPath, JobError> {
    RelPath::parse(s).map_err(|e| failed(format!("不正なパス {s:?}: {e}")))
}

fn ext_of(rel: &RelPath) -> Option<String> {
    rel.file_name().rsplit_once('.').map(|(_, e)| e.to_owned())
}

impl Handler for TranscodeHandler {
    fn run(&self, ctx: JobContext) -> BoxFuture<'static, HandlerResult> {
        let this = TranscodeHandler {
            library: Arc::clone(&self.library),
            derived: Arc::clone(&self.derived),
            encoder: self.encoder.clone(),
            artwork: Arc::clone(&self.artwork),
            ffmpeg: self.ffmpeg.clone(),
            reference_lufs: self.reference_lufs,
            test_hook: self.test_hook.clone(),
        };
        Box::pin(async move { this.run_inner(ctx).await })
    }
}

impl TranscodeHandler {
    async fn run_inner(&self, ctx: JobContext) -> HandlerResult {
        let job_id = ctx.job.id;
        let Some(track_id) = ctx.job.payload.get("track_id").and_then(|v| v.as_i64()) else {
            return Err(failed(format!(
                "payload に整数の track_id が無い: {}",
                ctx.job.payload
            )));
        };
        let Some(r) = self.resolve(&ctx, track_id).await? else {
            tracing::info!(job_id, track_id, "トラックが無いので何もしない");
            return Ok(Outcome::Done);
        };
        let what = plan(&r.target, r.current.as_ref());
        tracing::debug!(job_id, track_id, ?what, "Derived の判定");
        if !what.needs_job() {
            return Ok(Outcome::Done);
        }
        // 期待パスの排他予約（物理的な書き込みの前。encode / move / retag の全経路で持つ）。
        // 別のジョブが持っていれば試行回数を数えずに再キュー
        let key = canonical_key(&expected_rel_path(&r.target.library_rel_path));
        let locked = ctx
            .db()
            .write({
                let key = key.clone();
                move |c| dbderived::lock_path(c, &key, Some(track_id), job_id, now_epoch())
            })
            .await?;
        if !locked {
            tracing::debug!(
                job_id,
                track_id,
                key,
                "Derived のパスを別のジョブが使っている（再キュー）"
            );
            return Ok(Outcome::Requeue);
        }
        self.hook("reserved").await;
        // 予約はどの結果（Done / Requeue / Failed / Cancelled）でも必ず解放する
        let result = self.execute(&ctx, &r, what).await;
        if let Err(e) = ctx
            .db()
            .write(move |c| dbderived::unlock_paths(c, job_id))
            .await
        {
            // 解放できなくても持ち主が running でなくなれば無効になる
            tracing::warn!(job_id, error = %e, "Derived のパス予約を解放できない");
        }
        result
    }

    /// 予約を持った状態で `what` を実行する
    async fn execute(&self, ctx: &JobContext, r: &Resolved, what: Plan) -> HandlerResult {
        match what {
            Plan::Skip | Plan::UpToDate => Ok(Outcome::Done),
            Plan::Encode => self.encode(ctx, r).await,
            Plan::Move => match self.do_move(ctx, r).await? {
                Step::Moved => Ok(Outcome::Done),
                Step::SourceMissing => self.encode(ctx, r).await,
                Step::Blocked => Ok(Outcome::Requeue),
            },
            Plan::MoveAndRetag => match self.do_move(ctx, r).await? {
                Step::Moved => self.retag(ctx, r).await,
                Step::SourceMissing => self.encode(ctx, r).await,
                Step::Blocked => Ok(Outcome::Requeue),
            },
            Plan::Retag => self.retag(ctx, r).await,
        }
    }

    async fn resolve(&self, ctx: &JobContext, track_id: i64) -> Result<Option<Resolved>, JobError> {
        let r = ctx
            .db()
            .read(move |c| {
                let Some(target) = dbderived::load_target(c, track_id)? else {
                    return Ok(None);
                };
                let current = dbderived::get(c, track_id)?;
                // missing なら member が空。plan が Skip にするので中身は使われない
                let member = dbrg::track_member(c, track_id)?.pop().unwrap_or(Member {
                    id: track_id,
                    rel_path: target.library_rel_path.clone(),
                    dev: None,
                    inode: None,
                    size: -1,
                    mtime_ns: 0,
                    ctime_ns: 0,
                });
                let rg = dbrg::write_rows(c, &[track_id])?
                    .pop()
                    .and_then(|w| w.values);
                let bit_depth: Option<i64> = c.query_row(
                    "SELECT bit_depth FROM tracks WHERE id = ?1",
                    [track_id],
                    |r| r.get(0),
                )?;
                Ok(Some(Resolved {
                    target,
                    current,
                    member,
                    rg,
                    bit_depth: bit_depth.and_then(|b| u32::try_from(b).ok()),
                }))
            })
            .await?;
        Ok(r)
    }

    /// Library を開き、FD の stat が行と一致することを確かめてタグを読む。`File` は同じ FD
    /// （エンコード後の再照合に使う）
    async fn open_library(&self, r: &Resolved) -> Result<(File, TransferTags), JobError> {
        let library = Arc::clone(&self.library);
        let member = r.member.clone();
        let rel = parse_rel(&r.target.library_rel_path)?;
        tokio::task::spawn_blocking(move || -> Result<(File, TransferTags), JobError> {
            let file = library
                .open_file(&rel)
                .map_err(|e| fs_failed("Library を開けない", e))?;
            let st = fsroot::fstat(&file).map_err(|e| fs_failed("stat できない", e))?;
            if !member.matches(&st) {
                return Err(failed(
                    "ファイルが DB の行と一致しない（再スキャン後に再試行）",
                ));
            }
            let reader = file.try_clone()?;
            let tags = read_transfer_tags(reader, ext_of(&rel).as_deref())
                .map_err(|e| failed(format!("タグを読めない: {e}")))?;
            Ok((file, tags))
        })
        .await
        .map_err(|e| failed(format!("読み取りタスクが異常終了: {e}")))?
    }

    /// 期待パスの占有を DB で確定する（物理的な書き込みの**前**に呼ぶ）。別トラックの行が
    /// 持っていれば、その行が missing のときだけ消して自分のものにする。生きていれば失敗。
    /// 読んでから書くまでを 1 トランザクションにするので、確認の後に占有側が復活しても
    /// （scanner は track lock を取らない）、復活した側は行を失って自分の期待パスに作り直す
    /// だけで、こちらが置いたファイルを動かすことはない
    ///
    /// 期待パスを**生きている**トラックが持っていて、それがその相手の期待パスではない（相手も
    /// Library で移動していて Derived が追随待ち。swap / 循環 rename）ときは [`Claim::Blocked`]。
    /// このとき自分の Derived が期待パス以外にあれば（Move）、先に**一時名へ退避**して自分の
    /// 現在のパスを空ける。相手はそれで進め、自分は次の試行で相手が空けたパスへ移る（rename バッチの
    /// 2 段階と同じ考え方。相手のファイルは相手のロックを持たないので触らない）
    async fn claim_path(
        &self,
        ctx: &JobContext,
        r: &Resolved,
        dst: &RelPath,
    ) -> Result<Claim, JobError> {
        let track_id = r.target.track_id;
        let key_dst = dst.as_str().to_owned();
        let claim = ctx
            .db()
            .write(move |c| {
                let tx = c.transaction()?;
                let claim = take_over_path(&tx, track_id, &key_dst)?;
                tx.commit()?;
                Ok(claim)
            })
            .await
            .map_err(|e| match e {
                DbError::Internal(msg) => failed(msg),
                other => JobError::from(other),
            })?;
        self.hook("claimed").await;
        if let Claim::Blocked {
            holder,
            holder_busy,
        } = &claim
        {
            self.vacate_self(ctx, r).await?;
            tracing::info!(
                track_id,
                holder,
                holder_busy,
                path = %dst,
                "期待パスを追随待ちのトラックが持っている（相手の移動を待つ）"
            );
        }
        Ok(claim)
    }

    /// 自分の Derived（期待パス以外にある）を同じディレクトリの一時名 `<名前>.moving-<track_id>` へ
    /// 退避し、行のパスもそこへ向ける。音声版が古くても退避する（次の試行で Encode が置き換えて
    /// 退避ファイルを消す）。実体が無ければ行を消して key を明け渡す（次の試行は Encode）。
    /// どちらにしても自分の元の key は空くので、循環の相手が進める
    async fn vacate_self(&self, ctx: &JobContext, r: &Resolved) -> Result<(), JobError> {
        let track_id = r.target.track_id;
        let Some(cur) = &r.current else {
            return Ok(());
        };
        let expected = expected_rel_path(&r.target.library_rel_path);
        if canonical_key(&cur.rel_path) == canonical_key(&expected) {
            return Ok(());
        }
        let from = parse_rel(&cur.rel_path)?;
        let already_aside = from.file_name().contains(".moving-");
        let aside_name = format!("{}.moving-{track_id}", from.file_name());
        let aside = match from.parent() {
            Some(p) => p.join(&aside_name),
            None => RelPath::parse(&aside_name),
        }
        .map_err(|e| failed(format!("退避名を作れない: {e}")))?;
        let moved = {
            let derived = Arc::clone(&self.derived);
            let (from, aside) = (from.clone(), aside.clone());
            tokio::task::spawn_blocking(move || -> Result<Option<bool>, JobError> {
                match derived.stat(&from) {
                    Ok(_) => {}
                    Err(FsError::NotFound) => return Ok(None),
                    Err(e) => return Err(fs_failed("Derived を stat できない", e)),
                }
                if already_aside {
                    return Ok(Some(false));
                }
                // 前回の退避の残骸があれば消す（自分の生成物）
                match derived.unlink(&aside) {
                    Ok(()) | Err(FsError::NotFound) => {}
                    Err(e) => return Err(fs_failed("退避先を空けられない", e)),
                }
                derived
                    .rename_noreplace(&from, &aside)
                    .map_err(|e| fs_failed("退避に失敗", e))?;
                Ok(Some(true))
            })
            .await
            .map_err(|e| failed(format!("退避タスクが異常終了: {e}")))??
        };
        match moved {
            None => {
                // 実体が無い。行を消して key を明け渡す（次の試行は Encode）
                ctx.db()
                    .write(move |c| dbderived::delete(c, track_id))
                    .await?;
                tracing::info!(track_id, path = %from, "Derived の実体が無いので行を消して key を明け渡した");
            }
            Some(false) => {} // 退避済み
            Some(true) => {
                let aside_str = aside.as_str().to_owned();
                ctx.db()
                    .write(move |c| dbderived::set_path(c, track_id, &aside_str))
                    .await?;
                tracing::info!(track_id, from = %from, to = %aside, "Derived を一時名へ退避した");
            }
        }
        Ok(())
    }

    // ---- Encode

    async fn encode(&self, ctx: &JobContext, r: &Resolved) -> HandlerResult {
        let job_id = ctx.job.id;
        let track_id = r.target.track_id;
        ctx.check_cancel().await?;
        let dst_rel = parse_rel(&expected_rel_path(&r.target.library_rel_path))?;
        // 期待パスの占有を先に確定する（エンコードしてから気付くと無駄になる。配置の直前にもう一度見る）
        if let Claim::Blocked { holder_busy, .. } = self.claim_path(ctx, r, &dst_rel).await? {
            return blocked(holder_busy);
        }

        // 1. Library を開いてタグを読む
        let (file, tags) = self.open_library(r).await?;
        let source = file.try_clone()?;

        // 2. エンコード
        let token = ctx.cancel_token();
        let encoded = match self.encoder.encode(source, r.bit_depth, &token).await {
            Ok(e) => e,
            Err(e) if e.is_cancelled() => return Err(JobError::Cancelled),
            Err(e) => return Err(failed(format!("エンコードに失敗: {e}"))),
        };

        // 3. タグと画像
        let (cover, embedded_artwork) = self.cover_picture(ctx, r.target.artwork_id).await?;
        let out = opus_tags(&tags, r.rg.as_ref(), self.reference_lufs, cover);
        {
            let path = encoded.guard.path().to_path_buf();
            tokio::task::spawn_blocking(move || -> Result<(), JobError> {
                let mut f = File::options().read(true).write(true).open(&path)?;
                write_opus_tags(&mut f, &out)
                    .map_err(|e| failed(format!("タグを書けない: {e}")))?;
                f.sync_all()?;
                Ok(())
            })
            .await
            .map_err(|e| failed(format!("タグ書き込みタスクが異常終了: {e}")))??;
        }

        // 4. エンコード中に Library が変わっていないか（同じ FD を fstat）
        let st = fsroot::fstat(&file).map_err(|e| fs_failed("stat できない", e))?;
        if !r.member.matches(&st) {
            return Err(failed(
                "エンコード中にファイルが変わった（再スキャン後に再試行）",
            ));
        }
        ctx.check_cancel().await?;

        // 5. 期待パスの占有を確定し直してから Derived へ配置（宛先ディレクトリの tmp → replace）
        if let Claim::Blocked { holder_busy, .. } = self.claim_path(ctx, r, &dst_rel).await? {
            return blocked(holder_busy);
        }
        {
            let derived = Arc::clone(&self.derived);
            let src = encoded.guard.path().to_path_buf();
            let dst_rel = dst_rel.clone();
            tokio::task::spawn_blocking(move || place(&derived, &src, &dst_rel))
                .await
                .map_err(|e| failed(format!("配置タスクが異常終了: {e}")))??;
        }
        drop(encoded);

        // 6. DB（占有は 5 で確定済み。ここで別の行が現れるのは `x.flac` と `x.wav` が同じ期待パスを
        //    持つような衝突だけで、その場合は失敗にする）
        let tag_state = TagState {
            src_artwork_id: embedded_artwork,
            ..TagState::of(&r.target)
        };
        let av = r.target.audio_version;
        let dst = dst_rel.as_str().to_owned();
        let bitrate = i64::from(self.encoder.bitrate_kbps());
        let before = r.target.clone();
        let drifted = ctx
            .db()
            .write(move |c| {
                let tx = c.transaction()?;
                if !matches!(take_over_path(&tx, track_id, &dst)?, Claim::Claimed) {
                    return Err(DbError::Internal(format!(
                        "配置の直後に別のトラックが期待パスを持っている: {dst}"
                    )));
                }
                dbderived::upsert(
                    &tx,
                    track_id,
                    &dst,
                    DERIVED_CODEC,
                    Some(bitrate),
                    av,
                    tag_state,
                    now_epoch(),
                )?;
                let drifted = dbderived::target_drifted(&tx, &before)?;
                tx.commit()?;
                Ok(drifted)
            })
            .await?;

        // 7. 旧パスの自分の Derived を消す（パスが変わっていたとき）
        if let Some(old) = r
            .current
            .as_ref()
            .map(|c| c.rel_path.as_str())
            .filter(|o| canonical_key(o) != canonical_key(dst_rel.as_str()))
        {
            if let Ok(old) = RelPath::parse(old) {
                match self.derived.unlink(&old) {
                    Ok(()) | Err(FsError::NotFound) => {}
                    Err(e) => tracing::warn!(
                        track_id,
                        path = %old,
                        error = %e,
                        "旧 Derived を消せない（GC が回収する）"
                    ),
                }
            }
        }
        tracing::info!(job_id, track_id, path = %dst_rel, "Derived を生成した");
        if drifted {
            // 読んでから記録するまでに世代が動いた（album gain の切り替え等）。記録は書いた内容で
            // 正しいので、揃え直しは同じジョブの再実行に任せる
            tracing::info!(
                job_id,
                track_id,
                "生成中に元の世代が動いたので揃え直す（再キュー）"
            );
            return Ok(Outcome::Requeue);
        }
        Ok(Outcome::Done)
    }

    // ---- Move

    /// 旧パスから期待パスへ rename。元が無ければ [`Step::SourceMissing`]（呼び出し側が Encode に倒す）
    async fn do_move(&self, ctx: &JobContext, r: &Resolved) -> Result<Step, JobError> {
        let track_id = r.target.track_id;
        let Some(cur) = &r.current else {
            return Ok(Step::SourceMissing);
        };
        let from = parse_rel(&cur.rel_path)?;
        let to = parse_rel(&expected_rel_path(&r.target.library_rel_path))?;
        ctx.check_cancel().await?;
        match self.claim_path(ctx, r, &to).await? {
            Claim::Claimed => {}
            Claim::Blocked {
                holder_busy: true, ..
            } => return Ok(Step::Blocked),
            Claim::Blocked { holder, .. } => {
                return Err(failed(format!(
                    "Derived のパスを追随待ちのトラック {holder} が持っていて、その transcode が動いて \
                     いない（次の scan で投入される）: {to}"
                )))
            }
        }
        let moved = {
            let derived = Arc::clone(&self.derived);
            let (from, to) = (from.clone(), to.clone());
            tokio::task::spawn_blocking(move || -> Result<bool, JobError> {
                match derived.stat(&from) {
                    Ok(_) => {}
                    Err(FsError::NotFound) => return Ok(false),
                    Err(e) => return Err(fs_failed("Derived を stat できない", e)),
                }
                if let Some(parent) = to.parent() {
                    derived
                        .create_dir_all(&parent)
                        .map_err(|e| fs_failed("ディレクトリを作れない", e))?;
                }
                // 宛先に何かあれば消す（Derived は再生成物。DB 上の占有は claim_path で確定した）
                match derived.unlink(&to) {
                    Ok(()) | Err(FsError::NotFound) => {}
                    Err(e) => return Err(fs_failed("宛先を空けられない", e)),
                }
                derived
                    .rename_noreplace(&from, &to)
                    .map_err(|e| fs_failed("rename に失敗", e))?;
                derived
                    .fsync_dir(to.parent().as_ref())
                    .map_err(|e| fs_failed("fsync できない", e))?;
                Ok(true)
            })
            .await
            .map_err(|e| failed(format!("rename タスクが異常終了: {e}")))??
        };
        if !moved {
            return Ok(Step::SourceMissing);
        }
        let dst = to.as_str().to_owned();
        ctx.db()
            .write(move |c| {
                let tx = c.transaction()?;
                if !matches!(take_over_path(&tx, track_id, &dst)?, Claim::Claimed) {
                    return Err(DbError::Internal(format!(
                        "移動の直後に別のトラックが期待パスを持っている: {dst}"
                    )));
                }
                dbderived::set_path(&tx, track_id, &dst)?;
                tx.commit()?;
                Ok(())
            })
            .await?;
        tracing::info!(track_id, from = %from, to = %to, "Derived を移動した");
        Ok(Step::Moved)
    }

    // ---- Retag

    async fn retag(&self, ctx: &JobContext, r: &Resolved) -> HandlerResult {
        let track_id = r.target.track_id;
        let dst = parse_rel(&expected_rel_path(&r.target.library_rel_path))?;
        ctx.check_cancel().await?;
        let (_file, tags) = self.open_library(r).await?;
        let (cover, embedded_artwork) = self.cover_picture(ctx, r.target.artwork_id).await?;
        let out = opus_tags(&tags, r.rg.as_ref(), self.reference_lufs, cover);
        ctx.check_cancel().await?;
        self.hook("retag_before_write").await;
        let exists = {
            let derived = Arc::clone(&self.derived);
            let dst = dst.clone();
            tokio::task::spawn_blocking(move || -> Result<bool, JobError> {
                let mut src = match derived.open_file(&dst) {
                    Ok(f) => f,
                    Err(FsError::NotFound) => return Ok(false),
                    Err(e) => return Err(fs_failed("Derived を開けない", e)),
                };
                let (tmp_rel, mut tmp) = derived
                    .create_tmp(dst.parent().as_ref())
                    .map_err(|e| fs_failed("tmp を作れない", e))?;
                let result = (|| -> Result<(), JobError> {
                    std::io::copy(&mut src, &mut tmp)?;
                    tmp.seek(SeekFrom::Start(0))?;
                    write_opus_tags(&mut tmp, &out)
                        .map_err(|e| failed(format!("タグを書けない: {e}")))?;
                    tmp.sync_all()?;
                    derived
                        .replace_file(&tmp_rel, &dst)
                        .map_err(|e| fs_failed("置き換えられない", e))?;
                    Ok(())
                })();
                if result.is_err() {
                    let _ = derived.unlink(&tmp_rel);
                }
                result.map(|()| true)
            })
            .await
            .map_err(|e| failed(format!("タグ書き込みタスクが異常終了: {e}")))??
        };
        if !exists {
            // Derived が消えている → 作り直す
            tracing::info!(track_id, path = %dst, "Derived が無いので作り直す");
            return self.encode(ctx, r).await;
        }
        let tag_state = TagState {
            src_artwork_id: embedded_artwork,
            ..TagState::of(&r.target)
        };
        let before = r.target.clone();
        let drifted = ctx
            .db()
            .write(move |c| {
                let tx = c.transaction()?;
                dbderived::set_tag_state(&tx, track_id, tag_state, now_epoch())?;
                let drifted = dbderived::target_drifted(&tx, &before)?;
                tx.commit()?;
                Ok(drifted)
            })
            .await?;
        tracing::info!(track_id, path = %dst, "Derived のタグを更新した");
        if drifted {
            tracing::info!(
                track_id,
                "タグ更新中に元の世代が動いたので揃え直す（再キュー）"
            );
            return Ok(Outcome::Requeue);
        }
        Ok(Outcome::Done)
    }

    // ---- 画像

    /// album のアートワークの 768 WebP を `Picture` にする。キャッシュに無ければ原画像から作る
    /// （thumbnail ジョブと同じ変換。tmp + rename なので thumbnail ジョブと同時に走っても壊れない）。
    /// 原画像も無ければ画像なし。返す id は**実際に埋めた**画像のもの（画像なしなら None）で、
    /// `src_artwork_id` にはこれを記録する。album の `artwork_id` をそのまま記録すると、次のスキャンが
    /// 原画像を復旧しても（同じ SHA-256 なので同じ id）不一致が出ず、画像なしのまま固定される
    async fn cover_picture(
        &self,
        ctx: &JobContext,
        artwork_id: Option<i64>,
    ) -> Result<(Option<Picture>, Option<i64>), JobError> {
        let Some(id) = artwork_id else {
            return Ok((None, None));
        };
        let Some(art) = ctx.db().read(move |c| dbart::get(c, id)).await? else {
            return Ok((None, None));
        };
        let thumb = self.artwork.thumb_path(&art.sha256, COVER_SIZE);
        if !thumb.is_file() {
            let orig = self.artwork.original_path(&art.sha256, &art.mime);
            if !orig.is_file() {
                tracing::warn!(
                    artwork_id = id,
                    "原画像がキャッシュに無いので画像なしで作る（復旧後に書き直す）"
                );
                return Ok((None, None));
            }
            make_thumb(
                &self.ffmpeg,
                &orig,
                &thumb,
                COVER_SIZE,
                ctx.job.id,
                &ctx.cancel_token(),
            )
            .await?;
        }
        let bytes = tokio::fs::read(&thumb).await?;
        let picture = Picture::unchecked(bytes)
            .pic_type(PictureType::CoverFront)
            .mime_type(MimeType::Unknown("image/webp".into()))
            .build();
        Ok((Some(picture), Some(id)))
    }
}

/// 期待パスを別トラックの行が持っていれば明け渡させる（missing の行だけ。生きていれば
/// [`DbError::Internal`]）。`claim_path` と、最後の upsert のトランザクションから呼ぶ
fn take_over_path(tx: &rusqlite::Connection, track_id: i64, dst: &str) -> crate::db::Result<Claim> {
    let Some(h) = dbderived::holder_state(tx, &canonical_key(dst))? else {
        return Ok(Claim::Claimed);
    };
    if h.track_id == track_id {
        return Ok(Claim::Claimed);
    }
    if h.missing {
        dbderived::delete(tx, h.track_id)?;
        tracing::warn!(track_id, holder = h.track_id, path = %dst, "missing 行の Derived を明け渡した");
        return Ok(Claim::Claimed);
    }
    if h.stale {
        return Ok(Claim::Blocked {
            holder: h.track_id,
            holder_busy: dbderived::has_active_job(tx, h.track_id)?,
        });
    }
    // 相手の期待パスでもある: `x.flac` と `x.wav` のような衝突。片方しか持てない
    Err(DbError::Internal(format!(
        "Derived のパスを生きているトラック {} が持っている: {dst}",
        h.track_id
    )))
}

/// `Claim::Blocked` のときの結果: 相手が動いていれば再キュー、いなければ失敗（バックオフ。次の scan で
/// 両方が投入される）
fn blocked(holder_busy: bool) -> HandlerResult {
    if holder_busy {
        Ok(Outcome::Requeue)
    } else {
        Err(failed(
            "Derived のパスを追随待ちのトラックが持っていて、その transcode が動いていない（次の scan で投入される）",
        ))
    }
}

/// tmp の Opus を Derived の `dst` に置く（宛先ディレクトリの tmp にコピー → replace）
fn place(derived: &RootDir, src: &Path, dst: &RelPath) -> Result<(), JobError> {
    if let Some(parent) = dst.parent() {
        derived
            .create_dir_all(&parent)
            .map_err(|e| fs_failed("ディレクトリを作れない", e))?;
    }
    let (tmp_rel, mut tmp) = derived
        .create_tmp(dst.parent().as_ref())
        .map_err(|e| fs_failed("tmp を作れない", e))?;
    let result = (|| -> Result<(), JobError> {
        let mut s = File::open(src)?;
        std::io::copy(&mut s, &mut tmp)?;
        tmp.sync_all()?;
        derived
            .replace_file(&tmp_rel, dst)
            .map_err(|e| fs_failed("置き換えられない", e))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = derived.unlink(&tmp_rel);
    }
    result
}

// ---------------------------------------------------------------- 取り残しの回収

/// [`sweep_tmp`] の結果
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SweepReport {
    /// `<data>/tmp` から消した作業ファイル
    pub data_tmp: usize,
    /// Derived から消した `.spindle-tmp-*`
    pub derived_tmp: usize,
}

/// 起動時に取り残しを回収する。`<data>/tmp` の `spindle-transcode-*` / `spindle-normalize-*`
/// （エンコードの中間 WAV と生成物。SIGKILL / 電源断で `TempGuard` が走らないと残る）と、Derived の
/// `.spindle-tmp-*`（配置・retag の途中）。単一インスタンスで、ワーカーを起動する前は何も走って
/// いないので、年齢を見ずに全部消してよい（Library の tmp は scanner が猶予付きで回収する。
/// Derived は scan の対象外なので、ここで行う）。消せなかったものは警告して続ける
pub fn sweep_tmp(data_tmp: &Path, derived: &RootDir) -> SweepReport {
    let mut report = SweepReport::default();
    if let Ok(entries) = std::fs::read_dir(data_tmp) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !(name.starts_with("spindle-transcode-") || name.starts_with("spindle-normalize-")) {
                continue;
            }
            match std::fs::remove_file(entry.path()) {
                Ok(()) => {
                    tracing::info!(path = %entry.path().display(), "取り残された作業ファイルを回収した");
                    report.data_tmp += 1;
                }
                Err(e) => {
                    tracing::warn!(path = %entry.path().display(), error = %e, "作業ファイルを消せない")
                }
            }
        }
    }
    let mut stack: Vec<Option<RelPath>> = vec![None];
    while let Some(dir) = stack.pop() {
        let entries = match derived.read_dir(dir.as_ref()) {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(dir = ?dir, error = %e, "Derived のディレクトリを読めない");
                continue;
            }
        };
        for entry in entries {
            let Some(name) = entry.name.to_str() else {
                continue;
            };
            let rel = match &dir {
                Some(d) => d.join(name),
                None => RelPath::parse(name),
            };
            let Ok(rel) = rel else {
                continue;
            };
            match entry.kind {
                fsroot::FileKind::Dir => stack.push(Some(rel)),
                fsroot::FileKind::File if name.starts_with(fsroot::TMP_PREFIX) => {
                    match derived.unlink(&rel) {
                        Ok(()) => {
                            tracing::info!(path = %rel, "Derived の取り残された一時ファイルを回収した");
                            report.derived_tmp += 1;
                        }
                        Err(e) => {
                            tracing::warn!(path = %rel, error = %e, "一時ファイルを消せない")
                        }
                    }
                }
                _ => {}
            }
        }
    }
    report
}
