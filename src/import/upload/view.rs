//! ③ の表示（設計 §4.2・§5.4）: ステージングの読み取り、既定のチェック、件の状態、API が返す
//! [`UploadView`] の組み立て。
//!
//! - 読むのは removed でも damaged でもない、受け取り終えたばらのファイル `files/<origin>` と、
//!   zip の成功した世代 `x/<origin>/g<generation>` の中身だけ（未完の `.part`・garbage・展開中の世代は
//!   読まない）。開くのはすべて Inbox の [`RootDir`] から
//! - チェックのキー: 件は [`Group::key`]、ファイルは [`Member::staged`]（ステージング内の相対パス。
//!   `files/o3`、`x/o2/g1/Disc 1/01.flac`）。マニフェストの `included_*` / `excluded_*` も同じキー
//! - 件の状態は強い順（[`Status`] の列挙順）に当たったものをすべて `statuses` に並べ、先頭を `status` に出す

use std::collections::{HashMap, HashSet};

use serde::Serialize;

use crate::config::{LayoutConfig, UploadConfig};
use crate::db::scans::Fingerprint;
use crate::domain::pathgen::sanitize_component;
use crate::domain::relpath::RelPath;
use crate::domain::tags::read_audio_file;
use crate::fsroot::{FileKind, FsError, RootDir};
use crate::import::inbox::{destination_of, library_rows_with_audio};
use crate::import::scanner::read_fingerprint_fd;

use super::group::{group, Group, Grouping, Member, StagedFile};
use super::manifest::{
    classify, FileClass, Manifest, Origin, Rejected, Stage, UploadError, UploadId, ZipError,
};

/// 公開先の Inbox の最上位
pub const PUBLISH_ROOT: &str = "upload";
/// アルバム名の無い件の置き場所（`upload/_untitled/<origin_label>`）
pub const UNTITLED_DIR: &str = "_untitled";
const DAY_SECS: i64 = 86_400;
/// 置き場所の 1 要素のバイト数の上限（RelPath の 255 より小さく、` (N)` を足せる）
pub const DEST_COMPONENT_MAX: usize = 200;

/// 件の状態（強い順 = 列挙順）
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Status {
    /// 件の全曲の音声がライブラリの active な行と一致（`paths` は一致したライブラリのパス）
    LibraryDuplicate {
        paths: Vec<String>,
    },
    /// 置き場所のディレクトリが Inbox に既にある（公開すると ` (2)` の別の件になる）
    InboxExists {
        dir: String,
    },
    /// ライブラリの既存の album に追記される見込み
    AppendTo {
        album: String,
        track_count: i64,
    },
    /// アルバム名のタグが無い
    NeedsName,
    New,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UploadView {
    pub id: String,
    pub created_at: i64,
    pub updated_at: i64,
    /// `updated_at + stale_days` 日。これを過ぎると GC が破棄扱いにする
    pub expires_at: i64,
    pub origins: Vec<OriginView>,
    /// 送信・展開・公開のジョブが動いている（呼び出し側が判定して渡す）
    pub busy: bool,
    pub groups: Vec<GroupView>,
    /// 取り込まないファイル（display。読めない音声は理由付き）
    pub ignored: Vec<String>,
    /// どの件にも付けない同梱ファイル（display）
    pub unassigned_companions: Vec<String>,
    pub publish: Option<PublishView>,
    pub publish_requested_at: Option<i64>,
    pub discarded_at: Option<i64>,
}

/// 送ったもの 1 つ（② の行）
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OriginView {
    pub id: String,
    pub client_path: String,
    pub class: FileClass,
    pub total: u64,
    pub received: u64,
    pub complete: bool,
    pub removed: bool,
    pub damaged: bool,
    pub zip: Option<ZipView>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ZipView {
    /// 成功した世代（0 = 未展開）
    pub generation: u32,
    /// 展開中
    pub unpacking: bool,
    pub rejected: Vec<Rejected>,
    pub error: Option<ZipError>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GroupView {
    pub key: String,
    pub albumartist: String,
    pub album: Option<String>,
    pub included: bool,
    /// チェックできるか（included な曲が 1 曲以上）
    pub selectable: bool,
    /// included の曲の数
    pub track_count: usize,
    /// `FLAC 96k/24`。混在は `FLAC ほか`
    pub format: String,
    pub status: Status,
    pub statuses: Vec<Status>,
    pub tracks: Vec<MemberView>,
    pub companions: Vec<MemberView>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MemberView {
    /// ファイルのチェックのキー
    pub staged: String,
    pub display: String,
    pub publish_name: String,
    /// 平らに集めるときに名前を変えた（「名前を変えて置く: 元 → 新」）
    pub renamed: bool,
    pub included: bool,
    /// 同じ音声のライブラリのパス（複数あれば先頭）
    pub library_match: Option<String>,
}

/// 凍結した公開計画の進み具合
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PublishView {
    pub frozen_at: i64,
    pub groups: Vec<PublishGroupView>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PublishGroupView {
    pub key: String,
    pub albumartist: String,
    pub album: Option<String>,
    /// `{"stage": "publishing", "dest": "upload/…"}` の形で平らに出す
    #[serde(flatten)]
    pub stage: Stage,
}

/// [`decide`] の結果（`grouping.groups` と同じ順）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decided {
    pub groups: Vec<GroupDecision>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupDecision {
    pub key: String,
    pub included: bool,
    pub selectable: bool,
    /// `Group::tracks` と同じ順
    pub tracks: Vec<bool>,
    /// `Group::companions` と同じ順
    pub companions: Vec<bool>,
}

fn basename(display: &str) -> &str {
    display.rsplit('/').next().unwrap_or(display)
}

/// 件の置き場所の基本の名前（` (2)` などを付ける前）。アルバム名があれば
/// `upload/<albumartist>/<album>`、無ければ `upload/_untitled/<origin_label>`（各要素は
/// `sanitize_component`）。状態の判定と公開で同じものを使う
pub fn base_dest(group: &Group) -> Result<RelPath, UploadError> {
    dest_of(
        &group.albumartist,
        group.album.as_deref(),
        &group.origin_label,
    )
}

/// [`base_dest`] の中身（凍結した計画の件からも引けるように値で受ける）
pub(crate) fn dest_of(
    albumartist: &str,
    album: Option<&str>,
    origin_label: &str,
) -> Result<RelPath, UploadError> {
    let path = match album {
        Some(album) => format!(
            "{PUBLISH_ROOT}/{}/{}",
            dest_component(albumartist),
            dest_component(album)
        ),
        None => format!(
            "{PUBLISH_ROOT}/{UNTITLED_DIR}/{}",
            dest_component(origin_label)
        ),
    };
    Ok(RelPath::parse(&path)?)
}

/// 置き場所の 1 要素: `sanitize_component` の後、`.` で始まれば `_` を前に付け（Inbox の走査は
/// `.` で始まる名前を飛ばす）、[`DEST_COMPONENT_MAX`] バイト以下に文字境界で切る（公開が ` (2)` を
/// 足す余地を残す）。切った後の末尾の `.`・空白は落とし、空になれば `_`
fn dest_component(s: &str) -> String {
    let mut c = sanitize_component(s);
    if c.starts_with('.') {
        c.insert(0, '_');
    }
    if c.len() > DEST_COMPONENT_MAX {
        let mut cut = DEST_COMPONENT_MAX;
        while !c.is_char_boundary(cut) {
            cut -= 1;
        }
        c.truncate(cut);
        let kept = c.trim_end_matches(['.', ' ']).len();
        c.truncate(kept);
    }
    if c.is_empty() {
        c.push('_');
    }
    c
}

/// ステージングの読める中身を読む（タグと音声の指紋。指紋は [`Member::staged`] で引く）。
/// 読めない音声は [`FileClass::Ignored`] にし、display に理由を付ける
pub fn read_staged(
    inbox: &RootDir,
    m: &Manifest,
) -> Result<(Vec<StagedFile>, HashMap<String, Fingerprint>), UploadError> {
    let id = UploadId::parse(&m.id)?;
    let mut files = Vec::new();
    let mut fps = HashMap::new();
    for o in &m.origins {
        if o.removed || o.damaged || !o.complete {
            continue;
        }
        if o.class == FileClass::Zip {
            let Some(z) = &o.zip else { continue };
            if z.generation == 0 || z.unpacking == Some(z.generation) {
                continue;
            }
            let gdir = o.unpack_dir(z.generation)?;
            if m.garbage.iter().any(|g| g.path == gdir.as_str()) {
                continue;
            }
            let mut found = Vec::new();
            walk(inbox, &id, &gdir, "", &mut found)?;
            found.sort();
            for (staged, inner, error) in found {
                let display = if inner.is_empty() {
                    o.client_path.clone()
                } else {
                    format!("{}/{inner}", o.client_path)
                };
                if let Some(reason) = error {
                    files.push(unreadable(o, &staged, display, &reason));
                    continue;
                }
                let class = classify(&inner);
                if let Some(f) = read_one(inbox, &id, o, &staged, display, class, &mut fps)? {
                    files.push(f);
                }
            }
        } else {
            let staged = o.file_path()?;
            if let Some(f) = read_one(
                inbox,
                &id,
                o,
                &staged,
                o.client_path.clone(),
                o.class,
                &mut fps,
            )? {
                files.push(f);
            }
        }
    }
    Ok((files, fps))
}

/// 世代の中のファイルを集める（`(ステージング内のパス, 世代内のパス, 読めない理由)`）。
/// ディレクトリが無ければ空。読めないディレクトリはその 1 項を理由付きで返す（表示全体は落とさない）
fn walk(
    inbox: &RootDir,
    id: &UploadId,
    dir: &RelPath,
    prefix: &str,
    out: &mut Vec<(RelPath, String, Option<String>)>,
) -> Result<(), UploadError> {
    let entries = match inbox.read_dir(Some(&id.staged(dir)?)) {
        Ok(e) => e,
        Err(FsError::NotFound) => {
            tracing::warn!(upload = id.as_str(), dir = %dir, "展開した世代が無い");
            return Ok(());
        }
        Err(e) => {
            tracing::warn!(upload = id.as_str(), dir = %dir, error = %e, "展開したディレクトリを読めない");
            out.push((dir.clone(), prefix.to_string(), Some(e.to_string())));
            return Ok(());
        }
    };
    for e in entries {
        let Some(name) = e.name.to_str() else {
            continue;
        };
        let Ok(child) = dir.join(name) else { continue };
        let inner = if prefix.is_empty() {
            name.to_string()
        } else {
            format!("{prefix}/{name}")
        };
        match e.kind {
            FileKind::Dir => walk(inbox, id, &child, &inner, out)?,
            FileKind::File => out.push((child, inner, None)),
            FileKind::Symlink | FileKind::Other => {}
        }
    }
    Ok(())
}

/// 1 ファイルを読む。実体が消えていれば `None`
fn read_one(
    inbox: &RootDir,
    id: &UploadId,
    o: &Origin,
    staged: &RelPath,
    display: String,
    class: FileClass,
    fps: &mut HashMap<String, Fingerprint>,
) -> Result<Option<StagedFile>, UploadError> {
    let mut f = StagedFile {
        staged: staged.as_str().to_string(),
        origin: o.id.clone(),
        display,
        class,
        tags: Vec::new(),
        codec: None,
        lossless: false,
        sample_rate: None,
        bit_depth: None,
    };
    if class != FileClass::Audio {
        return Ok(Some(f));
    }
    let file = match inbox.open_file(&id.staged(staged)?) {
        Ok(file) => file,
        Err(FsError::NotFound) => {
            tracing::warn!(upload = id.as_str(), path = %staged, "ステージングのファイルが無い");
            return Ok(None);
        }
        Err(e) => {
            tracing::warn!(upload = id.as_str(), path = %staged, error = %e, "ステージングのファイルを開けない");
            return Ok(Some(unreadable(o, staged, f.display, &e.to_string())));
        }
    };
    // ステージングの名前（`files/o3`）には拡張子が無いので、拡張子は元の名前から取る
    let name = RelPath::parse(basename(&f.display)).unwrap_or_else(|_| staged.clone());
    let ext = name
        .file_name()
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase());
    let dup = match file.try_clone() {
        Ok(d) => d,
        Err(e) => return Ok(Some(unreadable(o, staged, f.display, &e.to_string()))),
    };
    match read_audio_file(dup, ext.as_deref()) {
        Ok(af) => {
            // タグと同じ実体（FD）で指紋を取る（Inbox の走査と同じ計算）
            fps.insert(f.staged.clone(), read_fingerprint_fd(&file, &name, &af));
            f.tags = af.tags.items().to_vec();
            f.codec = Some(af.codec.as_str().to_string());
            f.lossless = af.lossless;
            f.sample_rate = af.sample_rate;
            f.bit_depth = af.bit_depth;
        }
        Err(e) => {
            tracing::warn!(upload = id.as_str(), path = %staged, error = %e, "アップロードの音声を読めない");
            f.class = FileClass::Ignored;
            f.display = format!("{}（読めない: {e}）", f.display);
        }
    }
    Ok(Some(f))
}

/// 読めないファイル（取り込まないへ理由付きで出す）
fn unreadable(o: &Origin, staged: &RelPath, display: String, reason: &str) -> StagedFile {
    StagedFile {
        staged: staged.as_str().to_string(),
        origin: o.id.clone(),
        display: format!("{display}（読めない: {reason}）"),
        class: FileClass::Ignored,
        tags: Vec::new(),
        codec: None,
        lossless: false,
        sample_rate: None,
        bit_depth: None,
    }
}

/// 件と曲の included を決める（純粋）。曲: 既定 true、ライブラリと一致する曲は既定 false。
/// 件: 既定 true、全曲が一致する件は既定 false。`included_*` が既定 false を true に戻し、
/// `excluded_*` が最後に勝つ。included な曲が 0 の件は included = false で選べない。
/// 同梱ファイルは既定 true で、`excluded_files` だけが効く（件の選べるかには効かない）
pub fn decide(
    m: &Manifest,
    grouping: &Grouping,
    matches: &HashMap<String, Vec<String>>,
) -> Decided {
    let set = |v: &[String]| -> HashSet<String> { v.iter().cloned().collect() };
    let (inc_files, exc_files) = (set(&m.included_files), set(&m.excluded_files));
    let (inc_groups, exc_groups) = (set(&m.included_groups), set(&m.excluded_groups));
    let is_match = |t: &Member| matches.get(&t.staged).is_some_and(|p| !p.is_empty());
    let pick = |default: bool, key: &str, inc: &HashSet<String>, exc: &HashSet<String>| {
        if exc.contains(key) {
            false
        } else {
            default || inc.contains(key)
        }
    };
    let groups = grouping
        .groups
        .iter()
        .map(|g| {
            let tracks: Vec<bool> = g
                .tracks
                .iter()
                .map(|t| pick(!is_match(t), &t.staged, &inc_files, &exc_files))
                .collect();
            let companions: Vec<bool> = g
                .companions
                .iter()
                .map(|c| !exc_files.contains(&c.staged))
                .collect();
            let all_match = !g.tracks.is_empty() && g.tracks.iter().all(is_match);
            let selectable = tracks.iter().any(|&b| b);
            let included = selectable && pick(!all_match, &g.key, &inc_groups, &exc_groups);
            GroupDecision {
                key: g.key.clone(),
                included,
                selectable,
                tracks,
                companions,
            }
        })
        .collect();
    Decided { groups }
}

/// 件の曲ごとに、同じ音声のライブラリの active な行のパス（整列・重複なし。無い曲は入れない）。
/// [`decide`] の入力（表示と公開の凍結で同じものを使う）
pub(crate) fn library_matches(
    conn: &rusqlite::Connection,
    grouping: &Grouping,
    fps: &HashMap<String, Fingerprint>,
) -> Result<HashMap<String, Vec<String>>, UploadError> {
    let mut matches: HashMap<String, Vec<String>> = HashMap::new();
    for t in grouping.groups.iter().flat_map(|g| &g.tracks) {
        let Some(fp) = fps.get(&t.staged) else {
            continue;
        };
        let mut paths: Vec<String> = library_rows_with_audio(conn, fp)?
            .into_iter()
            .map(|r| r.rel_path)
            .collect();
        if !paths.is_empty() {
            paths.sort();
            paths.dedup();
            matches.insert(t.staged.clone(), paths);
        }
    }
    Ok(matches)
}

/// ③ の表示を組み立てる。`busy` は呼び出し側がジョブから判定する
pub fn build_view(
    conn: &rusqlite::Connection,
    layout: &LayoutConfig,
    inbox: &RootDir,
    m: &Manifest,
    cfg: &UploadConfig,
    busy: bool,
) -> Result<UploadView, UploadError> {
    let (files, fps) = read_staged(inbox, m)?;
    let grouping = group(&files, m);
    let by_staged: HashMap<&str, &StagedFile> =
        files.iter().map(|f| (f.staged.as_str(), f)).collect();

    let matches = library_matches(conn, &grouping, &fps)?;
    let decided = decide(m, &grouping, &matches);

    let mut groups = Vec::with_capacity(grouping.groups.len());
    for (g, d) in grouping.groups.iter().zip(&decided.groups) {
        let statuses = statuses_of(conn, layout, inbox, g, &matches)?;
        let member = |mem: &Member, included: bool, with_match: bool| MemberView {
            staged: mem.staged.clone(),
            display: mem.display.clone(),
            publish_name: mem.publish_name.clone(),
            renamed: mem.publish_name != basename(&mem.display),
            included,
            library_match: with_match
                .then(|| matches.get(&mem.staged).and_then(|p| p.first().cloned()))
                .flatten(),
        };
        groups.push(GroupView {
            key: g.key.clone(),
            albumartist: g.albumartist.clone(),
            album: g.album.clone(),
            included: d.included,
            selectable: d.selectable,
            track_count: d.tracks.iter().filter(|&&b| b).count(),
            format: format_of(g, &by_staged),
            status: statuses.first().cloned().unwrap_or(Status::New),
            statuses,
            tracks: g
                .tracks
                .iter()
                .zip(&d.tracks)
                .map(|(t, &inc)| member(t, inc, true))
                .collect(),
            companions: g
                .companions
                .iter()
                .zip(&d.companions)
                .map(|(c, &inc)| member(c, inc, false))
                .collect(),
        });
    }

    Ok(UploadView {
        id: m.id.clone(),
        created_at: m.created_at,
        updated_at: m.updated_at,
        expires_at: m
            .updated_at
            .saturating_add(i64::from(cfg.stale_days) * DAY_SECS),
        origins: m.origins.iter().map(origin_view).collect(),
        busy,
        groups,
        ignored: grouping.ignored,
        unassigned_companions: grouping.unassigned_companions,
        publish: m.publish.as_ref().map(|p| PublishView {
            frozen_at: p.frozen_at,
            groups: p
                .groups
                .iter()
                .map(|g| PublishGroupView {
                    key: g.key.clone(),
                    albumartist: g.albumartist.clone(),
                    album: g.album.clone(),
                    stage: g.stage.clone(),
                })
                .collect(),
        }),
        publish_requested_at: m.publish_requested_at,
        discarded_at: m.discarded_at,
    })
}

/// 件に当たる状態をすべて強い順に。何も当たらなければ `[New]`
fn statuses_of(
    conn: &rusqlite::Connection,
    layout: &LayoutConfig,
    inbox: &RootDir,
    g: &Group,
    matches: &HashMap<String, Vec<String>>,
) -> Result<Vec<Status>, UploadError> {
    let mut out = Vec::new();
    if !g.tracks.is_empty() && g.tracks.iter().all(|t| matches.contains_key(&t.staged)) {
        let mut paths: Vec<String> = g
            .tracks
            .iter()
            .filter_map(|t| matches.get(&t.staged))
            .flatten()
            .cloned()
            .collect();
        paths.sort();
        paths.dedup();
        out.push(Status::LibraryDuplicate { paths });
    }
    // Inbox に同じ件: 置き場所が**ディレクトリとして**あるときだけ（音声の一致は見ない。設計 §4.2）
    match base_dest(g) {
        Ok(dir) => match inbox.stat(&dir) {
            Ok(st) if st.kind == FileKind::Dir => out.push(Status::InboxExists {
                dir: dir.as_str().to_string(),
            }),
            Ok(_) | Err(FsError::NotFound) => {}
            Err(e) => {
                tracing::warn!(dir = %dir, error = %e, "アップロードの置き場所を確かめられない")
            }
        },
        Err(e) => {
            tracing::warn!(key = g.key.as_str(), error = %e, "アップロードの置き場所の名前を作れない")
        }
    }
    match &g.album {
        Some(album) => {
            if let Some(d) = destination_of(conn, layout, None, &g.albumartist, album)
                .map_err(|e| UploadError::Inbox(Box::new(e)))?
            {
                out.push(Status::AppendTo {
                    album: d.album.unwrap_or_else(|| album.clone()),
                    track_count: d.track_count,
                });
            }
        }
        None => out.push(Status::NeedsName),
    }
    if out.is_empty() {
        out.push(Status::New);
    }
    Ok(out)
}

fn codec_label(codec: &str) -> String {
    match codec {
        "opus" => "Opus".to_string(),
        other => other.to_ascii_uppercase(),
    }
}

/// 44100 → `44.1k`、96000 → `96k`
fn rate_label(hz: u32) -> String {
    let s = format!("{:.2}", f64::from(hz) / 1000.0);
    let s = s.trim_end_matches('0').trim_end_matches('.');
    format!("{s}k")
}

/// 1 曲の形式（可逆は標本化周波数とビット深度まで）
fn track_format(f: &StagedFile) -> String {
    let codec = codec_label(f.codec.as_deref().unwrap_or("?"));
    if !f.lossless {
        return codec;
    }
    match (f.sample_rate, f.bit_depth) {
        (Some(sr), Some(bits)) => format!("{codec} {}/{bits}", rate_label(sr)),
        (Some(sr), None) => format!("{codec} {}", rate_label(sr)),
        _ => codec,
    }
}

/// 件の形式。全曲同じならそれ、混在は先頭の曲の codec + ` ほか`
fn format_of(g: &Group, by_staged: &HashMap<&str, &StagedFile>) -> String {
    let files: Vec<&StagedFile> = g
        .tracks
        .iter()
        .filter_map(|t| by_staged.get(t.staged.as_str()).copied())
        .collect();
    let Some(first) = files.first() else {
        return String::new();
    };
    let head = track_format(first);
    if files.iter().all(|f| track_format(f) == head) {
        head
    } else {
        format!(
            "{} ほか",
            codec_label(first.codec.as_deref().unwrap_or("?"))
        )
    }
}

fn origin_view(o: &Origin) -> OriginView {
    OriginView {
        id: o.id.clone(),
        client_path: o.client_path.clone(),
        class: o.class,
        total: o.total,
        received: o.received,
        complete: o.complete,
        removed: o.removed,
        damaged: o.damaged,
        zip: o.zip.as_ref().map(|z| ZipView {
            generation: z.generation,
            unpacking: z.unpacking.is_some(),
            rejected: z.rejected.clone(),
            error: z.error.clone(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_and_format_labels() {
        assert_eq!(rate_label(44_100), "44.1k");
        assert_eq!(rate_label(96_000), "96k");
        assert_eq!(rate_label(22_050), "22.05k");
        let f = StagedFile {
            staged: "files/o1".into(),
            origin: "o1".into(),
            display: "a.flac".into(),
            class: FileClass::Audio,
            tags: vec![],
            codec: Some("flac".into()),
            lossless: true,
            sample_rate: Some(96_000),
            bit_depth: Some(24),
        };
        assert_eq!(track_format(&f), "FLAC 96k/24");
        let lossy = StagedFile {
            codec: Some("opus".into()),
            lossless: false,
            ..f.clone()
        };
        assert_eq!(track_format(&lossy), "Opus");
    }

    #[test]
    fn base_dest_sanitizes_and_handles_untitled() {
        let g = Group {
            key: "k".into(),
            albumartist: "A/B".into(),
            album: Some("Best?".into()),
            origin_label: "mora".into(),
            origins: vec![],
            tracks: vec![],
            companions: vec![],
        };
        assert_eq!(base_dest(&g).unwrap().as_str(), "upload/A／B/Best？");
        let g = Group { album: None, ..g };
        assert_eq!(base_dest(&g).unwrap().as_str(), "upload/_untitled/mora");
    }

    fn named(albumartist: &str, album: Option<&str>, label: &str) -> Group {
        Group {
            key: "k".into(),
            albumartist: albumartist.into(),
            album: album.map(str::to_string),
            origin_label: label.into(),
            origins: vec![],
            tracks: vec![],
            companions: vec![],
        }
    }

    #[test]
    fn base_dest_does_not_make_hidden_names() {
        let g = named("Artist", Some(".hack"), "x");
        assert_eq!(base_dest(&g).unwrap().as_str(), "upload/Artist/_.hack");
        let g = named("Artist", None, ".secret");
        assert_eq!(base_dest(&g).unwrap().as_str(), "upload/_untitled/_.secret");
    }

    #[test]
    fn base_dest_cuts_long_components() {
        // 3 バイト文字 100 個 = 300 バイト
        let long = "あ".repeat(100);
        let g = named("Artist", Some(&long), "x");
        let dest = base_dest(&g).unwrap();
        let last = dest.file_name();
        assert!(last.len() <= DEST_COMPONENT_MAX, "{}", last.len());
        assert!(last.chars().all(|c| c == 'あ'));
        assert!(RelPath::parse(dest.as_str()).is_ok());
        // 切った後の末尾のドット・空白は落とす（200 バイト目の手前が空白）
        let s = format!("{}  {}", "a".repeat(198), "b".repeat(50));
        let c = dest_component(&s);
        assert_eq!(c, "a".repeat(198));
        assert_eq!(dest_component("..."), "_");
    }
}
