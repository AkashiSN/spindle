//! 件（アルバムのまとまり）への分け方（設計 §5.4）。純粋関数で、I/O はしない。
//!
//! ステージング内のファイルを、タグ（なければ来た zip / フォルダ）で件に分け、サブフォルダを
//! 件の直下へ平らに集め（名前がぶつかれば改名）、同梱ファイルを付ける。included / excluded の
//! 判定はここでは扱わない。

use std::collections::{HashMap, HashSet};

use super::manifest::{FileClass, Manifest};
use crate::domain::relpath::canonical_key;

/// サイドカーの名前。件の中身にこの名前は使わせない
const SIDECAR_NAME: &str = "spindle-inbox.json";

/// 分け方の入力 1 ファイル
#[derive(Debug, Clone)]
pub struct StagedFile {
    /// ステージング内の相対パス（`files/o3`、`x/o2/g1/Disc 1/01.flac`）
    pub staged: String,
    /// 元の origin の ID
    pub origin: String,
    /// 利用者に見せる元のパス（`mora.zip/Disc 1/01.flac`。区切りは `/`）
    pub display: String,
    pub class: FileClass,
    /// 大文字キー、複数値は繰り返し（Inbox の `FileRow.tags` と同じ）
    pub tags: Vec<(String, String)>,
    pub codec: Option<String>,
    pub lossless: bool,
    pub sample_rate: Option<u32>,
    pub bit_depth: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    pub staged: String,
    pub display: String,
    /// 件の直下に置く名前
    pub publish_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    pub key: String,
    pub albumartist: String,
    pub album: Option<String>,
    /// 未タグの公開名・サイドカーの origins に使う名前
    pub origin_label: String,
    /// メンバーの origin の client_path（重複なし、出現順）
    pub origins: Vec<String>,
    pub tracks: Vec<Member>,
    pub companions: Vec<Member>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Grouping {
    pub groups: Vec<Group>,
    /// 取り込まないファイル（display）
    pub ignored: Vec<String>,
    /// どの件にも付けない同梱ファイル（display）
    pub unassigned_companions: Vec<String>,
}

fn first_tag<'a>(f: &'a StagedFile, key: &str) -> Option<&'a str> {
    f.tags
        .iter()
        .find(|(k, v)| k == key && !v.trim().is_empty())
        .map(|(_, v)| v.as_str())
}

fn basename(display: &str) -> &str {
    display.rsplit('/').next().unwrap_or(display)
}

/// display の親ディレクトリ名（basename の直前の要素）。トップレベルなら None
fn parent_name(display: &str) -> Option<&str> {
    let mut it = display.rsplit('/');
    it.next();
    it.next().filter(|s| !s.is_empty())
}

/// display の親ディレクトリのパス（同梱の付け先の判定に使う）
fn parent_dir(display: &str) -> &str {
    display.rsplit_once('/').map(|(d, _)| d).unwrap_or("")
}

/// 拡張子の前に ` (n)` を入れる
fn with_counter(name: &str, n: u32) -> String {
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => format!("{stem} ({n}).{ext}"),
        _ => format!("{name} ({n})"),
    }
}

fn origin_label(client_path: &str, class: FileClass) -> String {
    let first = client_path.split('/').next().unwrap_or(client_path);
    if client_path.contains('/') {
        return first.to_string();
    }
    match (class, first.rsplit_once('.')) {
        (_, Some((stem, _))) if !stem.is_empty() => stem.to_string(),
        _ => first.to_string(),
    }
}

struct Draft {
    key: String,
    albumartist: String,
    album: Option<String>,
    label: String,
    origins: Vec<String>,
    tracks: Vec<usize>,
}

/// 名前をぶつからないように決める（曲と同梱で同じ名前空間）
fn unique_name(taken: &mut HashSet<String>, display: &str) -> String {
    let base = basename(display);
    let is_sidecar = canonical_key(base) == SIDECAR_NAME;
    let mut candidates = Vec::new();
    if !is_sidecar {
        candidates.push(base.to_string());
        if let Some(p) = parent_name(display) {
            candidates.push(format!("{p} - {base}"));
        }
    }
    for c in &candidates {
        if taken.insert(canonical_key(c)) {
            return c.clone();
        }
    }
    let seed = candidates.last().map(String::as_str).unwrap_or(base);
    let mut n = 2;
    loop {
        let c = with_counter(seed, n);
        if canonical_key(&c) != SIDECAR_NAME && taken.insert(canonical_key(&c)) {
            return c;
        }
        n += 1;
    }
}

/// ファイルを件に分ける。順序は display のパス順で、件はキーの最初の出現順
pub fn group(files: &[StagedFile], m: &Manifest) -> Grouping {
    let mut sorted: Vec<&StagedFile> = files.iter().collect();
    sorted.sort_by(|a, b| a.display.cmp(&b.display));
    let origin_of = |id: &str| m.origins.iter().find(|o| o.id == id);

    let mut out = Grouping::default();
    let mut drafts: Vec<Draft> = Vec::new();
    let mut by_key: HashMap<String, usize> = HashMap::new();
    let mut companions: Vec<&StagedFile> = Vec::new();

    // 件のキーを決め、曲を振り分ける（merge は後で畳む）
    for f in &sorted {
        match f.class {
            FileClass::Audio => {}
            FileClass::Companion => {
                companions.push(f);
                continue;
            }
            FileClass::Zip | FileClass::Ignored => {
                out.ignored.push(f.display.clone());
                continue;
            }
        }
        let client_path = origin_of(&f.origin).map(|o| o.client_path.as_str());
        let origin_class = origin_of(&f.origin).map(|o| o.class);
        let album = first_tag(f, "ALBUM").map(str::to_string);
        let albumartist = first_tag(f, "ALBUMARTIST")
            .or_else(|| first_tag(f, "ARTIST"))
            .unwrap_or("")
            .to_string();
        let key = match &album {
            Some(a) => format!(
                "tag:{}\u{1f}{}",
                canonical_key(&albumartist),
                canonical_key(a)
            ),
            None => match (origin_class, client_path) {
                (Some(FileClass::Zip), _) => format!("origin:{}", f.origin),
                (_, Some(p)) if p.contains('/') => {
                    format!("folder:{}", canonical_key(p.split('/').next().unwrap_or(p)))
                }
                _ => format!("file:{}", f.origin),
            },
        };
        let label = match (&album, client_path) {
            (Some(a), _) => a.clone(),
            (None, Some(p)) => origin_label(p, origin_class.unwrap_or(FileClass::Audio)),
            (None, None) => basename(&f.display).to_string(),
        };
        let idx = *by_key.entry(key.clone()).or_insert_with(|| {
            drafts.push(Draft {
                key,
                albumartist,
                album,
                label,
                origins: Vec::new(),
                tracks: Vec::new(),
            });
            drafts.len() - 1
        });
        if let Some(p) = client_path {
            if !drafts[idx].origins.iter().any(|o| o == p) {
                drafts[idx].origins.push(p.to_string());
            }
        }
        drafts[idx].tracks.push(files_index(&sorted, f));
    }

    // merges: 各組の存在するキーを最初の件へ畳む
    let mut absorbed: HashMap<usize, usize> = HashMap::new();
    for set in &m.merges {
        let mut idxs = set
            .iter()
            .filter_map(|k| by_key.get(k).copied())
            .map(|i| resolve(&absorbed, i));
        let Some(head) = idxs.next() else { continue };
        for i in idxs.collect::<Vec<_>>() {
            if i != head {
                absorbed.insert(i, head);
            }
        }
    }
    let mut finals: Vec<Draft> = Vec::new();
    let mut slot: HashMap<usize, usize> = HashMap::new();
    for (i, draft) in drafts.iter().enumerate() {
        let head = resolve(&absorbed, i);
        if head == i {
            slot.insert(i, finals.len());
            finals.push(Draft {
                key: draft.key.clone(),
                albumartist: draft.albumartist.clone(),
                album: draft.album.clone(),
                label: draft.label.clone(),
                origins: draft.origins.clone(),
                tracks: draft.tracks.clone(),
            });
        } else if let Some(&s) = slot.get(&head) {
            for o in &draft.origins {
                if !finals[s].origins.contains(o) {
                    finals[s].origins.push(o.clone());
                }
            }
            finals[s].tracks.extend(draft.tracks.iter().copied());
        }
    }

    // 同梱の付け先: 曲と同じ親ディレクトリの件、残りは件が 1 つならそこ
    let dirs: Vec<HashSet<&str>> = finals
        .iter()
        .map(|d| {
            d.tracks
                .iter()
                .map(|&t| parent_dir(&sorted[t].display))
                .collect()
        })
        .collect();
    let mut comp_of: Vec<Vec<usize>> = vec![Vec::new(); finals.len()];
    for (ci, c) in companions.iter().enumerate() {
        let dir = parent_dir(&c.display);
        let target = dirs
            .iter()
            .position(|s| s.contains(dir))
            .or(if finals.len() == 1 { Some(0) } else { None });
        match target {
            Some(g) => comp_of[g].push(ci),
            None => out.unassigned_companions.push(c.display.clone()),
        }
    }

    for (gi, mut d) in finals.into_iter().enumerate() {
        d.tracks
            .sort_by(|&a, &b| sorted[a].display.cmp(&sorted[b].display));
        let mut taken = HashSet::new();
        let mut named: Vec<(bool, &StagedFile)> = d
            .tracks
            .iter()
            .map(|&t| (true, sorted[t]))
            .chain(comp_of[gi].iter().map(|&c| (false, companions[c])))
            .collect();
        named.sort_by(|a, b| a.1.display.cmp(&b.1.display));
        let mut tracks = Vec::new();
        let mut comps = Vec::new();
        for (is_track, f) in named {
            if let Some(o) = origin_of(&f.origin) {
                if !d.origins.contains(&o.client_path) {
                    d.origins.push(o.client_path.clone());
                }
            }
            let member = Member {
                staged: f.staged.clone(),
                display: f.display.clone(),
                publish_name: unique_name(&mut taken, &f.display),
            };
            if is_track {
                tracks.push(member);
            } else {
                comps.push(member);
            }
        }
        out.groups.push(Group {
            key: d.key,
            albumartist: d.albumartist,
            album: d.album,
            origin_label: d.label,
            origins: d.origins,
            tracks,
            companions: comps,
        });
    }
    out
}

fn resolve(absorbed: &HashMap<usize, usize>, mut i: usize) -> usize {
    while let Some(&n) = absorbed.get(&i) {
        i = n;
    }
    i
}

fn files_index(sorted: &[&StagedFile], f: &StagedFile) -> usize {
    sorted.iter().position(|s| std::ptr::eq(*s, f)).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn origin(id: &str, path: &str, class: FileClass) -> super::super::manifest::Origin {
        super::super::manifest::Origin {
            id: id.into(),
            client_path: path.into(),
            class,
            total: 1,
            received: 1,
            complete: true,
            removed: false,
            damaged: false,
            zip: None,
        }
    }

    fn manifest(origins: Vec<super::super::manifest::Origin>) -> Manifest {
        let mut m: Manifest = serde_json::from_str(
            r#"{"version":1,"id":"0123456789abcdef","created_at":0,"updated_at":0,"origins":[]}"#,
        )
        .expect("manifest");
        m.origins = origins;
        m
    }

    fn sf(origin: &str, display: &str, class: FileClass, tags: &[(&str, &str)]) -> StagedFile {
        StagedFile {
            staged: format!("s/{display}"),
            origin: origin.into(),
            display: display.into(),
            class,
            tags: tags
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            codec: None,
            lossless: true,
            sample_rate: None,
            bit_depth: None,
        }
    }

    fn names(g: &Group) -> Vec<&str> {
        g.tracks
            .iter()
            .chain(g.companions.iter())
            .map(|m| m.publish_name.as_str())
            .collect()
    }

    #[test]
    fn groups_singles_by_album_tag() {
        let m = manifest(vec![
            origin("o1", "a.flac", FileClass::Audio),
            origin("o2", "b.flac", FileClass::Audio),
            origin("o3", "c.flac", FileClass::Audio),
        ]);
        let t1 = [("ALBUMARTIST", "Foo"), ("ALBUM", "Bar")];
        let t2 = [("ARTIST", "FOO"), ("ALBUM", "bar")];
        let t3 = [("ALBUMARTIST", "Foo"), ("ALBUM", "Baz")];
        let g = group(
            &[
                sf("o1", "a.flac", FileClass::Audio, &t1),
                sf("o2", "b.flac", FileClass::Audio, &t2),
                sf("o3", "c.flac", FileClass::Audio, &t3),
            ],
            &m,
        );
        assert_eq!(g.groups.len(), 2);
        assert_eq!(g.groups[0].tracks.len(), 2);
        assert_eq!(g.groups[0].album.as_deref(), Some("Bar"));
        assert_eq!(g.groups[0].albumartist, "Foo");
    }

    #[test]
    fn multi_disc_zip_is_one_group_and_renames() {
        let m = manifest(vec![origin("o1", "mora.zip", FileClass::Zip)]);
        let t = [("ALBUMARTIST", "A"), ("ALBUM", "B")];
        let g = group(
            &[
                sf("o1", "mora.zip/Disc 1/01.flac", FileClass::Audio, &t),
                sf("o1", "mora.zip/Disc 2/01.flac", FileClass::Audio, &t),
            ],
            &m,
        );
        assert_eq!(g.groups.len(), 1);
        assert_eq!(names(&g.groups[0]), vec!["01.flac", "Disc 2 - 01.flac"]);
        assert_eq!(g.groups[0].origins, vec!["mora.zip"]);
    }

    #[test]
    fn untagged_grouped_by_zip_origin() {
        let m = manifest(vec![
            origin("o1", "a.zip", FileClass::Zip),
            origin("o2", "b.zip", FileClass::Zip),
        ]);
        let g = group(
            &[
                sf("o1", "a.zip/01.flac", FileClass::Audio, &[]),
                sf("o1", "a.zip/02.flac", FileClass::Audio, &[]),
                sf("o2", "b.zip/01.flac", FileClass::Audio, &[]),
            ],
            &m,
        );
        assert_eq!(g.groups.len(), 2);
        assert_eq!(g.groups[0].key, "origin:o1");
        assert_eq!(g.groups[0].origin_label, "a");
        assert_eq!(g.groups[0].tracks.len(), 2);
    }

    #[test]
    fn untagged_grouped_by_folder() {
        let m = manifest(vec![
            origin("o1", "Alb/01.flac", FileClass::Audio),
            origin("o2", "Alb/02.flac", FileClass::Audio),
            origin("o3", "Other/01.flac", FileClass::Audio),
        ]);
        let g = group(
            &[
                sf("o1", "Alb/01.flac", FileClass::Audio, &[]),
                sf("o2", "Alb/02.flac", FileClass::Audio, &[]),
                sf("o3", "Other/01.flac", FileClass::Audio, &[]),
            ],
            &m,
        );
        assert_eq!(g.groups.len(), 2);
        assert_eq!(g.groups[0].key, "folder:alb");
        assert_eq!(g.groups[0].origin_label, "Alb");
        assert_eq!(g.groups[0].tracks.len(), 2);
    }

    #[test]
    fn loose_untagged_is_single() {
        let m = manifest(vec![
            origin("o1", "a.flac", FileClass::Audio),
            origin("o2", "b.flac", FileClass::Audio),
        ]);
        let g = group(
            &[
                sf("o1", "a.flac", FileClass::Audio, &[]),
                sf("o2", "b.flac", FileClass::Audio, &[]),
            ],
            &m,
        );
        assert_eq!(g.groups.len(), 2);
        assert_eq!(g.groups[0].key, "file:o1");
    }

    #[test]
    fn merge_applied() {
        let mut m = manifest(vec![
            origin("o1", "a.flac", FileClass::Audio),
            origin("o2", "b.flac", FileClass::Audio),
        ]);
        m.merges = vec![vec!["file:o1".into(), "file:o2".into(), "file:zz".into()]];
        let g = group(
            &[
                sf("o1", "a.flac", FileClass::Audio, &[]),
                sf("o2", "b.flac", FileClass::Audio, &[]),
            ],
            &m,
        );
        assert_eq!(g.groups.len(), 1);
        assert_eq!(g.groups[0].key, "file:o1");
        assert_eq!(g.groups[0].tracks.len(), 2);
        assert_eq!(g.groups[0].origins, vec!["a.flac", "b.flac"]);
    }

    #[test]
    fn booklet_in_subfolder_attached_when_single_group() {
        let m = manifest(vec![origin("o1", "m.zip", FileClass::Zip)]);
        let t = [("ALBUM", "B")];
        let g = group(
            &[
                sf("o1", "m.zip/01.flac", FileClass::Audio, &t),
                sf("o1", "m.zip/Booklet/book.pdf", FileClass::Companion, &[]),
            ],
            &m,
        );
        assert_eq!(g.groups[0].companions.len(), 1);
        assert_eq!(g.groups[0].companions[0].publish_name, "book.pdf");
        assert!(g.unassigned_companions.is_empty());
    }

    #[test]
    fn companion_unassigned_when_many_groups() {
        let m = manifest(vec![origin("o1", "m.zip", FileClass::Zip)]);
        let g = group(
            &[
                sf("o1", "m.zip/A/01.flac", FileClass::Audio, &[("ALBUM", "A")]),
                sf("o1", "m.zip/B/01.flac", FileClass::Audio, &[("ALBUM", "B")]),
                sf("o1", "m.zip/A/cover.jpg", FileClass::Companion, &[]),
                sf("o1", "m.zip/Scans/x.pdf", FileClass::Companion, &[]),
            ],
            &m,
        );
        assert_eq!(g.groups.len(), 2);
        assert_eq!(g.groups[0].companions.len(), 1);
        assert_eq!(g.groups[1].companions.len(), 0);
        assert_eq!(g.unassigned_companions, vec!["m.zip/Scans/x.pdf"]);
    }

    #[test]
    fn readme_ignored() {
        let m = manifest(vec![origin("o1", "m.zip", FileClass::Zip)]);
        let g = group(
            &[
                sf("o1", "m.zip/01.flac", FileClass::Audio, &[("ALBUM", "A")]),
                sf("o1", "m.zip/readme.txt", FileClass::Ignored, &[]),
            ],
            &m,
        );
        assert_eq!(g.ignored, vec!["m.zip/readme.txt"]);
        assert_eq!(g.groups[0].tracks.len(), 1);
    }

    #[test]
    fn sidecar_name_never_used() {
        let m = manifest(vec![origin("o1", "m.zip", FileClass::Zip)]);
        let g = group(
            &[
                sf("o1", "m.zip/01.flac", FileClass::Audio, &[("ALBUM", "A")]),
                sf("o1", "m.zip/Spindle-Inbox.JSON", FileClass::Companion, &[]),
            ],
            &m,
        );
        assert_eq!(
            names(&g.groups[0]),
            vec!["01.flac", "Spindle-Inbox (2).JSON"]
        );
    }

    #[test]
    fn counter_after_parent_prefix_collision() {
        let m = manifest(vec![origin("o1", "m.zip", FileClass::Zip)]);
        let t = [("ALBUM", "A")];
        let g = group(
            &[
                sf("o1", "m.zip/a/Disc/01.flac", FileClass::Audio, &t),
                sf("o1", "m.zip/b/Disc/01.flac", FileClass::Audio, &t),
                sf("o1", "m.zip/c/Disc/01.flac", FileClass::Audio, &t),
            ],
            &m,
        );
        assert_eq!(
            names(&g.groups[0]),
            vec!["01.flac", "Disc - 01.flac", "Disc - 01 (2).flac"]
        );
    }
}
