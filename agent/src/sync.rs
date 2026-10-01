//! `sync` の段取り（仕様 ⑥「回復の順序」、D-100 判断 5）と `pair` / `resolve` / `abandon` / `status`。
//! 各コマンドは `<state_dir>/lock` を取り、state.json を読んで `Ctx` を作ってからエンジンを呼ぶ

use std::fmt::Write as _;
use std::path::PathBuf;

use agent_proto::{
    AbandonRequest, ManifestResponse, OpKind, Plan, PlaylistOpKind, ReportError, ReportPlaylist,
    ReportRequest, ReportState, ReportTrack,
};

use crate::ctx::{now_epoch, Ctx, Ui};
use crate::failpoint::Failpoints;
use crate::local::{self, LocalRoot, Marker};
use crate::music::Music;
use crate::plan::{current_of, runnable};
use crate::recover::Resolution;
use crate::secrets::Secrets;
use crate::server::{Confirmed, Reported, Server};
use crate::state::{SetupPhase, State, StateFile, STALE_TOKEN};
use crate::{exec, pair, recover, rediscover, Error, Result};

const NOT_PAIRED: &str =
    "pair されていません。spindle-agent pair <URL> <コード> を実行してください";
const OPEN_PLAN_EXISTS: &str = "別の計画が途中にあります。sync し直してください";
const KEEPS_CHANGING: &str = "差分が変わり続けています。少し待ってから sync し直してください";
/// 計画の確定を差分の変化で取り直す回数
const CONFIRM_ATTEMPTS: usize = 3;
/// `render_diff` が並べる行の上限
const DIFF_LINES: usize = 50;

/// state のディレクトリと root（`~/Music/spindle`）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    pub state_dir: PathBuf,
    pub root: PathBuf,
}

impl Paths {
    /// `SPINDLE_AGENT_STATE_DIR` / `SPINDLE_AGENT_ROOT`。無ければ
    /// `$HOME/Library/Application Support/spindle-agent` と `$HOME/Music/spindle`
    pub fn from_env() -> Result<Paths> {
        let home = || {
            std::env::var_os("HOME")
                .filter(|h| !h.is_empty())
                .map(PathBuf::from)
                .ok_or_else(|| Error::Stop("環境変数 HOME が設定されていません".to_owned()))
        };
        let state_dir = match std::env::var_os("SPINDLE_AGENT_STATE_DIR").filter(|v| !v.is_empty())
        {
            Some(v) => PathBuf::from(v),
            None => home()?
                .join("Library")
                .join("Application Support")
                .join("spindle-agent"),
        };
        let root = match std::env::var_os("SPINDLE_AGENT_ROOT").filter(|v| !v.is_empty()) {
            Some(v) => PathBuf::from(v),
            None => home()?.join("Music").join("spindle"),
        };
        Ok(Paths { state_dir, root })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncOutcome {
    /// 差分が無い
    NothingToDo,
    /// 確認で断られた
    Declined,
    /// スマートプレイリストの再評価待ちで確定できない
    PendingReevaluation,
    Applied {
        executed: usize,
        dropped: usize,
        errors: usize,
    },
}

/// ロックを取ったまま、state を読んで `Ctx` を作り `f` を走らせる
fn with_ctx<M: Music, S: Server, T>(
    music: &M,
    server: &S,
    paths: &Paths,
    fp: &Failpoints,
    ui: &mut dyn Ui,
    f: impl FnOnce(&mut Ctx<'_, M, S>) -> Result<T>,
) -> Result<T> {
    let _lock = local::lock(&paths.state_dir)?;
    let file = StateFile::new(&paths.state_dir);
    let local = LocalRoot::new(paths.root.clone());
    let state = file.load()?;
    let mut cx = Ctx {
        music,
        server,
        local: &local,
        file: &file,
        fp,
        ui,
        state,
        errors: Vec::new(),
        now: now_epoch,
    };
    f(&mut cx)
}

fn require_paired(s: &State) -> Result<()> {
    if s.server.is_none() || s.setup.is_none() {
        return Err(Error::Stop(NOT_PAIRED.to_owned()));
    }
    Ok(())
}

pub fn sync<M: Music, S: Server>(
    music: &M,
    server: &S,
    paths: &Paths,
    fp: &Failpoints,
    ui: &mut dyn Ui,
) -> Result<SyncOutcome> {
    with_ctx(music, server, paths, fp, ui, run_sync)
}

fn run_sync<M: Music, S: Server>(cx: &mut Ctx<'_, M, S>) -> Result<SyncOutcome> {
    require_paired(&cx.state)?;
    pair::check_copy_setting(cx)?;
    // ① root の印と初期化の続き。印を書く前（Started）の検証と書き込みは continue_setup が行う
    check_marker(cx)?;
    pair::continue_setup(cx)?;
    // ② バッチと pending_ops の回復
    recover::recover(cx)?;
    let mut m = cx.server.manifest()?;
    let uuid = cx
        .state
        .server
        .as_ref()
        .map(|s| s.device_uuid.clone())
        .unwrap_or_default();
    if m.device_uuid != uuid {
        return Err(Error::Stop(format!(
            "spindle が返した端末（{}）がこの Mac の登録と違います。pair し直してください",
            m.device_name
        )));
    }
    // ③ 再発見
    rediscover::rediscover(cx, &m)?;
    // ④ open な計画の照合
    match cx.server.open_plan()? {
        // 再開するのは、自分が確定して plan_id を記録した計画だけ（`apply` は副作用の前に記録する）。
        // それ以外（報告だけの回の確定の後で落ちた・別の実行が確定しただけ）は y/N を経ていないので、
        // 実行せずに今の state を報告して閉じる
        Some(p) if cx.state.plan_id != Some(p.plan_id) => {
            report(cx, &p)?;
            cx.ui
                .info("確定だけされて実行されていない計画を、実行せずに閉じました");
            m = cx.server.manifest()?;
        }
        Some(p) => {
            cx.ui.info("前回の同期の続きを実行します");
            if let SyncOutcome::Applied {
                executed,
                dropped,
                errors,
            } = apply(cx, &p, &m)?
            {
                cx.ui.info(&format!(
                    "前回の続き: 実行 {executed} 件、見送り {dropped} 件、エラー {errors} 件"
                ));
            }
            m = cx.server.manifest()?;
        }
        None if cx.state.needs_report => {
            if let Some(next) = report_only(cx, m)? {
                m = next;
            } else {
                return Ok(SyncOutcome::PendingReevaluation);
            }
        }
        None => {}
    }
    // ⑤ 表示と確認
    for attempt in 1..=CONFIRM_ATTEMPTS {
        if m.diff.items.is_empty() && m.diff.playlists.is_empty() {
            cx.ui.info(&format!(
                "変更はありません（保留 {} 件）",
                m.diff.held.len()
            ));
            return Ok(SyncOutcome::NothingToDo);
        }
        if m.pending_reevaluation {
            cx.ui.info(&render_diff(&m));
            cx.ui.info(
                "スマートプレイリストの再評価を待っています。少し待ってから sync し直してください",
            );
            return Ok(SyncOutcome::PendingReevaluation);
        }
        if !cx.ui.confirm(&m) {
            return Ok(SyncOutcome::Declined);
        }
        match cx.server.confirm(&m.plan_token)? {
            Confirmed::Plan(p) => {
                let out = apply(cx, &p, &m)?;
                note_remaining(cx);
                return Ok(out);
            }
            Confirmed::Changed { .. } if attempt == CONFIRM_ATTEMPTS => break,
            Confirmed::Changed { .. } => {
                cx.ui.info("差分が変わりました。取り直して表示し直します");
                m = cx.server.manifest()?;
            }
            Confirmed::PendingReevaluation => {
                cx.ui.info(
                    "スマートプレイリストの再評価を待っています。少し待ってから sync し直してください",
                );
                return Ok(SyncOutcome::PendingReevaluation);
            }
            Confirmed::OpenPlanExists => return Err(Error::Stop(OPEN_PLAN_EXISTS.to_owned())),
        }
    }
    Err(Error::Stop(KEEPS_CHANGING.to_owned()))
}

/// 印を書いた後の相なら、root の `.spindle-device` が自分のものであること
fn check_marker<M: Music, S: Server>(cx: &Ctx<'_, M, S>) -> Result<()> {
    let (Some(server), Some(setup)) = (&cx.state.server, &cx.state.setup) else {
        return Err(Error::Stop(NOT_PAIRED.to_owned()));
    };
    if setup.phase == SetupPhase::Started {
        return Ok(());
    }
    let mine = Marker {
        device_uuid: server.device_uuid.clone(),
        nonce: setup.nonce.clone(),
    };
    if cx.local.read_marker()?.as_ref() != Some(&mine) {
        return Err(Error::Stop(format!(
            "{} の {} が無いか、別の端末のものです。中身を確かめてから pair し直してください",
            cx.local.path().display(),
            local::MARKER
        )));
    }
    Ok(())
}

/// 報告だけの回（D-100 判断 5）: 何も実行せずに計画を確定して報告し、取り直した manifest を返す。
/// 再評価待ちで確定できなければ None（報告は次の sync に回す）
fn report_only<M: Music, S: Server>(
    cx: &mut Ctx<'_, M, S>,
    mut m: ManifestResponse,
) -> Result<Option<ManifestResponse>> {
    for attempt in 1..=CONFIRM_ATTEMPTS {
        match cx.server.confirm(&m.plan_token)? {
            Confirmed::Plan(p) => {
                report(cx, &p)?;
                return Ok(Some(cx.server.manifest()?));
            }
            Confirmed::Changed { .. } if attempt == CONFIRM_ATTEMPTS => break,
            Confirmed::Changed { .. } => m = cx.server.manifest()?,
            Confirmed::PendingReevaluation => {
                // ⑤ が差分を表示して「再評価待ち」で終える
                return if m.diff.items.is_empty() && m.diff.playlists.is_empty() {
                    cx.ui.info(
                        "スマートプレイリストの再評価を待っています。少し待ってから sync し直してください",
                    );
                    Ok(None)
                } else {
                    m.pending_reevaluation = true;
                    Ok(Some(m))
                };
            }
            Confirmed::OpenPlanExists => return Err(Error::Stop(OPEN_PLAN_EXISTS.to_owned())),
        }
    }
    Err(Error::Stop(KEEPS_CHANGING.to_owned()))
}

/// 確定した計画のうち今回実行する部分を実行して報告する
fn apply<M: Music, S: Server>(
    cx: &mut Ctx<'_, M, S>,
    p: &Plan,
    m: &ManifestResponse,
) -> Result<SyncOutcome> {
    cx.state.plan_id = Some(p.plan_id);
    cx.save()?;
    let (c, cp) = current_of(&cx.state);
    let r = runnable(p, &c, &cp, &m.diff);
    let out = exec::run(cx, &r, m)?;
    let errors = cx.errors.len();
    report(cx, p)?;
    if let Some(stop) = out.stop {
        return Err(Error::Stop(stop));
    }
    // `out.dropped` は runnable の見送りを含む
    Ok(SyncOutcome::Applied {
        executed: out.executed,
        dropped: out.dropped,
        errors,
    })
}

/// 報告の後に manifest を取り直し、残った差分（見送り・保留・エラーにした操作）の件数を知らせる
/// （D-100 判断 6）。知らせるだけなので、取り直しに失敗しても実行の結果は変えない
fn note_remaining<M: Music, S: Server>(cx: &mut Ctx<'_, M, S>) {
    let Ok(next) = cx.server.manifest() else {
        return;
    };
    let n = next.diff.items.len() + next.diff.playlists.len();
    if n > 0 {
        cx.ui.info(&format!(
            "まだ {n} 件の変更が残っています。次の sync で反映します"
        ));
    }
}

/// 計画の generation で報告する（確定の後で端末の設定が変われば、サーバが拒否する）
fn report<M: Music, S: Server>(cx: &mut Ctx<'_, M, S>, p: &Plan) -> Result<()> {
    let req = report_request(&cx.state, p.generation, p.plan_id, cx.errors.clone());
    match cx.server.report(&req)? {
        Reported::Ok => {
            cx.state.plan_id = None;
            cx.state.needs_report = false;
            cx.errors.clear();
            cx.save()
        }
        Reported::GenerationMismatch => Err(Error::Stop(
            "端末の設定が変わりました。sync し直してください".to_owned(),
        )),
        Reported::Closed | Reported::NoPlan => {
            cx.state.plan_id = None;
            cx.save()?;
            Err(Error::Stop(
                "計画は既に閉じています（UI から破棄された可能性）。sync し直してください"
                    .to_owned(),
            ))
        }
        Reported::Invalid(msg) => Err(Error::Stop(format!(
            "spindle が報告を受け付けませんでした: {msg}"
        ))),
    }
}

pub fn report_request(
    s: &State,
    generation: i64,
    plan_id: i64,
    errors: Vec<ReportError>,
) -> ReportRequest {
    // tracks / playlists は BTreeMap なので id の昇順
    ReportRequest {
        generation,
        plan_id,
        state: ReportState {
            tracks: s
                .tracks
                .iter()
                .map(|(id, e)| ReportTrack {
                    track_id: *id,
                    dest_path: e.path.clone(),
                    token: e.token.clone(),
                    size: e.size,
                    sha256: e.sha256.clone(),
                })
                .collect(),
            playlists: s
                .playlists
                .iter()
                .map(|(id, e)| ReportPlaylist {
                    playlist_id: *id,
                    name: e.name.clone(),
                    token: e.token.clone(),
                })
                .collect(),
        },
        errors,
    }
}

#[allow(clippy::too_many_arguments)]
pub fn pair_cmd<M: Music, S: Server>(
    music: &M,
    server: &S,
    secrets: &dyn Secrets,
    paths: &Paths,
    fp: &Failpoints,
    ui: &mut dyn Ui,
    url: &str,
    insecure_http: bool,
    code: &str,
) -> Result<()> {
    with_ctx(music, server, paths, fp, ui, |cx| {
        pair::pair(cx, secrets, url, insecure_http, code)?;
        let name = cx
            .state
            .server
            .as_ref()
            .map(|s| s.device_name.clone())
            .unwrap_or_default();
        cx.ui.info(&format!(
            "端末「{name}」として pair しました。spindle-agent sync で反映します"
        ));
        Ok(())
    })
}

pub fn resolve_cmd<M: Music, S: Server>(
    music: &M,
    server: &S,
    paths: &Paths,
    fp: &Failpoints,
    ui: &mut dyn Ui,
    op_id: &str,
    choice: Resolution,
) -> Result<()> {
    with_ctx(music, server, paths, fp, ui, |cx| {
        require_paired(&cx.state)?;
        recover::resolve(cx, op_id, choice)?;
        cx.ui
            .info("解決しました。spindle-agent sync で続きを行います");
        Ok(())
    })
}

/// open な計画を、今の state を報告して破棄する（途中の操作が無いときだけ）
pub fn abandon<M: Music, S: Server>(
    music: &M,
    server: &S,
    paths: &Paths,
    fp: &Failpoints,
    ui: &mut dyn Ui,
) -> Result<()> {
    with_ctx(music, server, paths, fp, ui, |cx| {
        require_paired(&cx.state)?;
        if !cx.state.pending_ops.is_empty() || !cx.state.pending_batches.is_empty() {
            return Err(Error::Stop(
                "途中の操作が残っています。先に sync してください".to_owned(),
            ));
        }
        let Some(p) = cx.server.open_plan()? else {
            cx.ui.info("途中の計画はありません");
            return Ok(());
        };
        let req = AbandonRequest {
            report: report_request(&cx.state, p.generation, p.plan_id, vec![]),
            pending_ops: 0,
            pending_batches: 0,
        };
        match cx.server.abandon(p.plan_id, &req)? {
            Reported::Ok => {
                cx.state.plan_id = None;
                cx.save()?;
                cx.ui.info(&format!("計画 {} を破棄しました", p.plan_id));
                Ok(())
            }
            Reported::GenerationMismatch => Err(Error::Stop(
                "端末の設定が変わりました。sync し直してください".to_owned(),
            )),
            Reported::Closed | Reported::NoPlan => {
                cx.state.plan_id = None;
                cx.save()?;
                Err(Error::Stop(
                    "計画は既に閉じています。sync し直してください".to_owned(),
                ))
            }
            Reported::Invalid(msg) => Err(Error::Stop(format!(
                "spindle が破棄を受け付けませんでした: {msg}"
            ))),
        }
    })
}

/// state の要約（読むだけなのでロックは取らない）
pub fn status(paths: &Paths) -> Result<String> {
    let s = StateFile::new(&paths.state_dir).load()?;
    let (Some(server), Some(setup)) = (&s.server, &s.setup) else {
        return Err(Error::Stop(NOT_PAIRED.to_owned()));
    };
    let stale = s.tracks.values().filter(|e| e.token == STALE_TOKEN).count();
    let mut out = String::new();
    let _ = writeln!(out, "端末: {}", server.device_name);
    let _ = writeln!(
        out,
        "spindle: {}{}",
        server.url,
        if server.insecure_http {
            "（平文 HTTP）"
        } else {
            ""
        }
    );
    let _ = writeln!(out, "保存先: {}", paths.root.display());
    if setup.phase != SetupPhase::Done {
        let _ = writeln!(out, "初期化: 途中（sync で続きを行います）");
    }
    let _ = writeln!(out, "曲: {}（うち古い写し {stale}）", s.tracks.len());
    let _ = writeln!(out, "プレイリスト: {}", s.playlists.len());
    let _ = writeln!(
        out,
        "途中の操作: {} 件、途中のパス変更: {} 件",
        s.pending_ops.len(),
        s.pending_batches.len()
    );
    for op in s.pending_ops.iter().filter(|o| o.candidates.is_some()) {
        let _ = writeln!(
            out,
            "  resolve 待ち: {}（{}）",
            op.op_id,
            op.to.as_deref().unwrap_or("")
        );
    }
    let _ = writeln!(
        out,
        "途中の計画: {}",
        s.plan_id
            .map(|id| id.to_string())
            .unwrap_or_else(|| "なし".to_owned())
    );
    let _ = write!(
        out,
        "報告待ち: {}",
        if s.needs_report { "あり" } else { "なし" }
    );
    Ok(out)
}

fn op_label(op: OpKind) -> (&'static str, &'static str) {
    match op {
        OpKind::Add => ("+", "追加"),
        OpKind::Update => ("~", "更新"),
        OpKind::Move => (">", "移動"),
        OpKind::UpdateMove => ("±", "更新+移動"),
        OpKind::Delete => ("-", "削除"),
    }
}

fn playlist_label(op: PlaylistOpKind) -> (&'static str, &'static str) {
    match op {
        PlaylistOpKind::Add => ("+", "追加"),
        PlaylistOpKind::Update => ("~", "更新"),
        PlaylistOpKind::Delete => ("-", "削除"),
    }
}

fn arrow(from: Option<&str>, to: Option<&str>) -> String {
    match (from, to) {
        (Some(f), Some(t)) if f != t => format!("{f} → {t}"),
        (_, Some(t)) => t.to_owned(),
        (Some(f), None) => f.to_owned(),
        (None, None) => String::new(),
    }
}

/// 差分の表示（件数の要約と、先頭 50 行）
pub fn render_diff(m: &ManifestResponse) -> String {
    let d = &m.diff;
    let count = |k: OpKind| d.items.iter().filter(|o| o.op == k).count();
    let pcount = |k: PlaylistOpKind| d.playlists.iter().filter(|o| o.op == k).count();
    let mut out = String::new();
    let items: Vec<String> = [
        OpKind::Add,
        OpKind::Update,
        OpKind::Move,
        OpKind::UpdateMove,
        OpKind::Delete,
    ]
    .into_iter()
    .map(|k| format!("{} {}", op_label(k).1, count(k)))
    .collect();
    let lists: Vec<String> = [
        PlaylistOpKind::Add,
        PlaylistOpKind::Update,
        PlaylistOpKind::Delete,
    ]
    .into_iter()
    .map(|k| format!("{} {}", playlist_label(k).1, pcount(k)))
    .collect();
    let _ = writeln!(
        out,
        "曲: {}／プレイリスト: {}／保留 {}",
        items.join("・"),
        lists.join("・"),
        d.held.len()
    );
    if !d.playlist_errors.is_empty() {
        let _ = writeln!(out, "プレイリストのエラー: {}", d.playlist_errors.len());
    }
    let mut lines: Vec<String> = d
        .items
        .iter()
        .map(|o| {
            format!(
                "  {} {}",
                op_label(o.op).0,
                arrow(o.from.as_deref(), o.to.as_deref())
            )
        })
        .chain(d.playlists.iter().map(|p| {
            format!(
                "  {} プレイリスト {}",
                playlist_label(p.op).0,
                arrow(p.from.as_deref(), p.to.as_deref())
            )
        }))
        .collect();
    let rest = lines.len().saturating_sub(DIFF_LINES);
    lines.truncate(DIFF_LINES);
    for l in lines {
        let _ = writeln!(out, "{l}");
    }
    if rest > 0 {
        let _ = writeln!(out, "  …ほか {rest} 件");
    }
    out.trim_end().to_owned()
}
