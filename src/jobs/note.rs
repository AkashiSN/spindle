//! GC・スキャン・バックアップのジョブの結果を `jobs.note` に残す文面（D-106）。1 行目を
//! ジョブ画面の「結果」列に、全文を行の展開に出す。ジョブ行と一緒に保持期間で消える

use crate::gc::Summary;
use crate::import::scanner::{ScanReport, SkipReason};

/// `note` に並べるパスの上限
pub const NOTE_LIST_MAX: usize = 20;

/// 1024 進の短い表記（画面の formatBytes と同じ）
pub fn human_bytes(n: u64) -> String {
    if n < 1024 {
        return format!("{n} B");
    }
    let units = ["KB", "MB", "GB", "TB"];
    let mut v = n as f64 / 1024.0;
    let mut i = 0;
    while v >= 1024.0 && i < units.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    format!("{v:.1} {}", units[i])
}

pub fn gc_note(s: &Summary, jobs: (usize, usize), plans: usize) -> String {
    let total = s.total_deleted();
    if total == 0 && jobs == (0, 0) && plans == 0 {
        return "消すものは無かった".to_owned();
    }
    let mut lines = vec![format!(
        "{total} 件・{} を削除",
        human_bytes(s.total_bytes())
    )];
    let rows: [(&str, &crate::gc::Counts, bool); 7] = [
        ("欠落トラック", &s.tracks, false),
        ("空のアルバム", &s.albums, false),
        ("退避ファイル", &s.archived, true),
        ("Derived", &s.derived, true),
        ("アートワークの行", &s.artwork_rows, false),
        ("サムネイル", &s.artwork_dirs, true),
        ("Inbox", &s.inbox, true),
    ];
    let (mut skipped, mut failed) = (0, 0);
    for (label, c, bytes) in rows {
        skipped += c.skipped;
        failed += c.failed;
        if c.deleted == 0 {
            continue;
        }
        lines.push(if bytes {
            format!("{label} {}（{}）", c.deleted, human_bytes(c.bytes))
        } else {
            format!("{label} {}", c.deleted)
        });
    }
    if skipped + failed > 0 {
        lines.push(format!("見送り {skipped} / 失敗 {failed}"));
    }
    match jobs {
        (0, 0) => {}
        (d, 0) => lines.push(format!("ジョブ行 完了 {d}")),
        (0, f) => lines.push(format!("ジョブ行 失敗 {f}")),
        (d, f) => lines.push(format!("ジョブ行 完了 {d}・失敗 {f}")),
    }
    if plans > 0 {
        lines.push(format!("同期の計画 {plans}"));
    }
    lines.join("\n")
}

fn skip_label(r: &SkipReason) -> &'static str {
    match r {
        SkipReason::Symlink => "symlink",
        SkipReason::InvalidName => "使えない名前",
        SkipReason::DuplicateKey => "大小文字だけ違う重複",
    }
}

pub fn scan_note(r: &ScanReport) -> String {
    let mut lines = vec![format!(
        "files {} / 新規 {} / 更新 {} / 移動 {} / missing {} / エラー {}",
        r.files_seen, r.new, r.updated, r.moved, r.missing_marked, r.errors
    )];
    if !r.error_paths.is_empty() {
        lines.push("読めなかったファイル:".to_owned());
        lines.extend(r.error_paths.iter().map(|p| format!("  {p}")));
        let rest = r.errors.saturating_sub(r.error_paths.len() as u64);
        if rest > 0 {
            lines.push(format!("  …ほか {rest} 件"));
        }
    }
    if !r.skipped.is_empty() {
        lines.push("対象外:".to_owned());
        lines.extend(
            r.skipped
                .iter()
                .take(NOTE_LIST_MAX)
                .map(|s| format!("  {}（{}）", s.path, skip_label(&s.reason))),
        );
        if r.skipped.len() > NOTE_LIST_MAX {
            lines.push(format!("  …ほか {} 件", r.skipped.len() - NOTE_LIST_MAX));
        }
    }
    lines.join("\n")
}

pub fn backup_note(file: &str, bytes: u64, retention: usize, removed: usize) -> String {
    format!(
        "{file}（{}）。最新 {retention} 世代を保持、{removed} 件を削除",
        human_bytes(bytes)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gc::{Counts, Summary};
    use crate::import::scanner::{ScanReport, SkipReason, Skipped};

    #[test]
    fn human_bytes_uses_binary_units() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1536), "1.5 KB");
        assert_eq!(human_bytes(3 * 1024 * 1024 * 1024), "3.0 GB");
    }

    #[test]
    fn gc_note_lists_only_nonzero_kinds() {
        let s = Summary {
            archived: Counts {
                deleted: 3,
                bytes: 2048,
                ..Counts::default()
            },
            tracks: Counts {
                deleted: 1,
                ..Counts::default()
            },
            ..Summary::default()
        };
        let n = gc_note(&s, (5, 0), 0);
        let lines: Vec<&str> = n.lines().collect();
        assert_eq!(lines[0], "4 件・2.0 KB を削除");
        assert!(lines.contains(&"欠落トラック 1"));
        assert!(lines.contains(&"退避ファイル 3（2.0 KB）"));
        assert!(lines.contains(&"ジョブ行 完了 5"));
        assert!(!n.contains("Derived"), "0 件の種類は出さない: {n}");
    }

    #[test]
    fn gc_note_says_nothing_to_do() {
        assert_eq!(
            gc_note(&Summary::default(), (0, 0), 0),
            "消すものは無かった"
        );
    }

    #[test]
    fn scan_note_has_counts_then_capped_paths() {
        let mut r = ScanReport {
            files_seen: 10,
            new: 2,
            errors: 25,
            ..ScanReport::default()
        };
        r.error_paths = (0..20).map(|i| format!("A/{i}.flac")).collect();
        r.skipped = vec![Skipped {
            path: "B/x".to_owned(),
            reason: SkipReason::Symlink,
        }];
        let n = scan_note(&r);
        let lines: Vec<&str> = n.lines().collect();
        assert_eq!(
            lines[0],
            "files 10 / 新規 2 / 更新 0 / 移動 0 / missing 0 / エラー 25"
        );
        assert_eq!(lines[1], "読めなかったファイル:");
        assert_eq!(lines[2], "  A/0.flac");
        assert!(n.contains("  …ほか 5 件"));
        assert!(n.contains("対象外:\n  B/x（symlink）"));
    }

    #[test]
    fn backup_note_is_one_line() {
        assert_eq!(
            backup_note("spindle-20261015T000000Z.db", 3 * 1024 * 1024, 7, 1),
            "spindle-20261015T000000Z.db（3.0 MB）。最新 7 世代を保持、1 件を削除"
        );
    }
}
