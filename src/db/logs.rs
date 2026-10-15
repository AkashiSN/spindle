//! 記録画面の「ログ」タブ（D-106）。`album_verifications` を「実行 1 回」にまとめる。鍵は
//! `(album_id, source, job_id)`、`job_id` が NULL の行（吸い出しの記録はすべて。Inbox の配置は 1 回の
//! ジョブが複数の件を置くのでジョブ id を持たない）は `(album_id, source, verified_at)`
//! （同じトランザクションで同じ `now` を書く）

use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;

use super::Result;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunKey {
    pub source: String,
    pub album_id: i64,
    pub job_id: Option<i64>,
    /// `job_id` が無いときの `verified_at`
    pub at: Option<i64>,
}

impl RunKey {
    pub fn encode(&self) -> String {
        match (self.job_id, self.at) {
            (Some(j), _) => format!("{}-{}-j{j}", self.source, self.album_id),
            (None, Some(t)) => format!("{}-{}-t{t}", self.source, self.album_id),
            (None, None) => format!("{}-{}-t0", self.source, self.album_id),
        }
    }

    /// `retro-12-j345` / `rip-12-t1700000000`。それ以外は None
    pub fn parse(s: &str) -> Option<Self> {
        let mut it = s.splitn(3, '-');
        let source = it.next()?;
        if source != "retro" && source != "rip" {
            return None;
        }
        let album_id: i64 = it.next()?.parse().ok()?;
        let tail = it.next()?;
        let (job_id, at) = if let Some(j) = tail.strip_prefix('j') {
            (Some(j.parse().ok()?), None)
        } else {
            (None, Some(tail.strip_prefix('t')?.parse().ok()?))
        };
        Some(Self {
            source: source.to_owned(),
            album_id,
            job_id,
            at,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunRow {
    pub disc_no: Option<i64>,
    pub method: String,
    pub result: String,
    pub log_path: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LogAvail {
    /// 本文が読める（はず）
    Latest,
    /// 照合ログは album ごとに最新 1 回分なので、後の照合で上書きされた
    Superseded,
    /// 記録にログが無い
    None,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DiscLine {
    pub disc_no: Option<i64>,
    pub method: String,
    pub result: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LogRun {
    pub key: String,
    pub album_id: i64,
    pub albumartist: Option<String>,
    pub album: Option<String>,
    pub source: String,
    pub job_id: Option<i64>,
    pub verified_at: i64,
    pub discs: Vec<DiscLine>,
    pub log: LogAvail,
}

/// 新しい順に `limit` 件（`offset` 件読み飛ばす）
pub fn list_runs(
    conn: &Connection,
    source: Option<&str>,
    album_id: Option<i64>,
    limit: usize,
    offset: usize,
) -> Result<Vec<LogRun>> {
    let mut st = conn.prepare(
        "SELECT v.album_id, v.source, v.job_id,
                CASE WHEN v.job_id IS NULL THEN v.verified_at END AS t,
                max(v.verified_at) AS at, a.albumartist, a.album
           FROM album_verifications v JOIN albums a ON a.id = v.album_id
          WHERE (?1 IS NULL OR v.source = ?1) AND (?2 IS NULL OR v.album_id = ?2)
          GROUP BY v.album_id, v.source, v.job_id, t
          ORDER BY at DESC, v.album_id DESC, v.job_id DESC, v.source DESC
          LIMIT ?3 OFFSET ?4",
    )?;
    let heads = st
        .query_map(
            params![
                source,
                album_id,
                i64::try_from(limit).unwrap_or(i64::MAX),
                i64::try_from(offset).unwrap_or(i64::MAX)
            ],
            |r| {
                Ok((
                    RunKey {
                        album_id: r.get(0)?,
                        source: r.get(1)?,
                        job_id: r.get(2)?,
                        at: r.get(3)?,
                    },
                    r.get::<_, i64>(4)?,
                    r.get::<_, Option<String>>(5)?,
                    r.get::<_, Option<String>>(6)?,
                ))
            },
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut out = Vec::with_capacity(heads.len());
    for (key, at, albumartist, album) in heads {
        let rows = run_rows(conn, &key)?;
        let log = availability(conn, &key, &rows)?;
        out.push(LogRun {
            key: key.encode(),
            album_id: key.album_id,
            albumartist,
            album,
            source: key.source.clone(),
            job_id: key.job_id,
            verified_at: at,
            discs: rows
                .iter()
                .map(|r| DiscLine {
                    disc_no: r.disc_no,
                    method: r.method.clone(),
                    result: r.result.clone(),
                })
                .collect(),
            log,
        });
    }
    Ok(out)
}

/// 1 回の実行の行（disc, 手法の順）
pub fn run_rows(conn: &Connection, key: &RunKey) -> Result<Vec<RunRow>> {
    let mut st = conn.prepare_cached(
        "SELECT disc_no, method, result, log_path FROM album_verifications
          WHERE album_id = ?1 AND source = ?2
            AND ((?3 IS NOT NULL AND job_id = ?3) OR (?3 IS NULL AND job_id IS NULL AND verified_at = ?4))
          ORDER BY disc_no, method DESC",
    )?;
    let rows = st
        .query_map(params![key.album_id, key.source, key.job_id, key.at], |r| {
            Ok(RunRow {
                disc_no: r.get(0)?,
                method: r.get(1)?,
                result: r.get(2)?,
                log_path: r.get(3)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// 本文が読めるかの目安。照合は album の最新の照合の実行だけ `latest`
pub fn availability(conn: &Connection, key: &RunKey, rows: &[RunRow]) -> Result<LogAvail> {
    if rows.iter().all(|r| r.log_path.is_none()) {
        return Ok(LogAvail::None);
    }
    if key.source != "retro" {
        return Ok(LogAvail::Latest);
    }
    let newest: Option<(Option<i64>, i64)> = conn
        .query_row(
            "SELECT job_id, verified_at FROM album_verifications
              WHERE album_id = ?1 AND source = 'retro'
              ORDER BY verified_at DESC, id DESC LIMIT 1",
            [key.album_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let latest = match (newest, key.job_id) {
        (Some((Some(j), _)), Some(k)) => j == k,
        (Some((None, t)), None) => Some(t) == key.at,
        _ => false,
    };
    Ok(if latest {
        LogAvail::Latest
    } else {
        LogAvail::Superseded
    })
}

/// その album・ディスクの active な曲が今あるディレクトリ（Library 相対）。ディスク番号の一致を優先し、
/// 無ければ album の最初の曲（1 枚物で tracks の disc_no が空のとき）
pub fn track_dir_for_disc(
    conn: &Connection,
    album_id: i64,
    disc_no: Option<i64>,
) -> Result<Option<String>> {
    let rel: Option<String> = conn
        .query_row(
            "SELECT rel_path FROM tracks
              WHERE album_id = ?1 AND missing_since IS NULL
              ORDER BY (disc_no IS ?2) DESC, disc_no, track_no, id LIMIT 1",
            params![album_id, disc_no],
            |r| r.get(0),
        )
        .optional()?;
    Ok(rel.and_then(|r| r.rsplit_once('/').map(|(dir, _)| dir.to_owned())))
}
