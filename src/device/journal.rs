//! 端末側のジャーナル `<root>/.spindle/journal`（仕様 ⑤「ジャーナルと中断からの回復」）。
//! 1 行 1 レコードの NDJSON。各操作は意図を耐久化してから副作用を起こし、終わったら完了を書く。
//! パス変更（移動・更新 + 移動）はバッチとして記録し、相（prepared → vacating → vacated → placing →
//! done）で進める。バッチ以外の意図は `put` と `rm` だけ（D-95 の P5-3a 追記）

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::domain::device::{sha256_hex, EntryKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntentOp {
    Put,
    Rm,
}

/// 意図。`put` は `to` へ書く（`sha256` は期待値）、`rm` は `from` を消す
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Intent {
    pub op_id: String,
    pub generation: i64,
    pub op: IntentOp,
    pub kind: EntryKind,
    pub ref_id: i64,
    pub from: Option<String>,
    pub to: Option<String>,
    pub token: Option<String>,
    pub size: Option<u64>,
    pub sha256: Option<String>,
}

/// バッチの相。並び順が進む順
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Prepared,
    Vacating,
    Vacated,
    Placing,
    Done,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemberOp {
    Move,
    UpdateMove,
}

/// バッチの 1 メンバー（曲だけ）。移動は `from → staging → to`、更新 + 移動は `new` に新しい内容を
/// 置き、`from` を消してから `new → to`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchMember {
    pub batch_id: String,
    pub op_id: String,
    pub op: MemberOp,
    pub track_id: i64,
    pub from: String,
    pub staging: String,
    pub new: Option<String>,
    pub to: String,
    pub token: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Record {
    Intent(Intent),
    Done {
        op_id: String,
    },
    BatchBegin {
        batch_id: String,
    },
    BatchMember(BatchMember),
    BatchSealed {
        batch_id: String,
        count: usize,
        digest: String,
    },
    BatchPhase {
        batch_id: String,
        phase: Phase,
    },
    BatchAbort {
        batch_id: String,
        superseded_by: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum JournalError {
    #[error("ジャーナルの {line} 行目が読めない（端末の .spindle/journal を確認してください）")]
    Corrupt { line: usize },
    #[error("ジャーナルを書けない: {0}")]
    Encode(String),
}

/// 1 行（末尾に改行）
pub fn encode(r: &Record) -> Result<Vec<u8>, JournalError> {
    let mut v = serde_json::to_vec(r).map_err(|e| JournalError::Encode(e.to_string()))?;
    v.push(b'\n');
    Ok(v)
}

pub fn encode_all(rs: &[Record]) -> Result<Vec<u8>, JournalError> {
    let mut out = Vec::new();
    for r in rs {
        out.extend(encode(r)?);
    }
    Ok(out)
}

/// 末尾の 1 行が途中で切れている（改行で終わらない・JSON として読めない）ときはその行を捨てる。
/// それより前の行が読めなければ全体を信用しない
pub fn parse(bytes: &[u8]) -> Result<Vec<Record>, JournalError> {
    let mut lines: Vec<&[u8]> = bytes.split(|b| *b == b'\n').collect();
    // split の最後の要素は最後の改行の後ろ。空でなければ改行で終わらない切れた行なので捨てる
    let tail = lines.pop().unwrap_or(&[]);
    let n = lines.len();
    let mut out = Vec::with_capacity(n);
    for (i, line) in lines.iter().enumerate() {
        if line.is_empty() {
            continue;
        }
        match serde_json::from_slice::<Record>(line) {
            Ok(r) => out.push(r),
            Err(_) if i + 1 == n && tail.is_empty() => break,
            Err(_) => return Err(JournalError::Corrupt { line: i + 1 }),
        }
    }
    Ok(out)
}

/// バッチの digest: 各 `batch_member` を書いた行（正準 JSON + `\n`）を計画の順に連結した SHA-256
pub fn member_digest(members: &[BatchMember]) -> Result<String, JournalError> {
    let mut bytes = Vec::new();
    for m in members {
        bytes.extend(encode(&Record::BatchMember(m.clone()))?);
    }
    Ok(sha256_hex(&bytes))
}

/// 128 bit の乱数を 16 進 32 桁で（op_id・batch_id）
pub fn random_id() -> std::io::Result<String> {
    let mut raw = [0u8; 16];
    getrandom::fill(&mut raw).map_err(|e| std::io::Error::other(e.to_string()))?;
    Ok(raw.iter().map(|b| format!("{b:02x}")).collect())
}

/// ジャーナルから読めた 1 つのバッチ
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchLog {
    pub batch_id: String,
    pub members: Vec<BatchMember>,
    /// `batch_sealed` があり、件数と digest が合う
    pub sealed: bool,
    /// 記録された最も進んだ相（封印済みのときだけ意味を持つ）
    pub phase: Option<Phase>,
    pub aborted: bool,
}

/// 再生の単位。ジャーナルに現れた順（意図はその行、バッチは `batch_begin` の位置）
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    Intent { intent: Intent, done: bool },
    Batch(BatchLog),
}

pub fn replay(records: &[Record]) -> Vec<Step> {
    let done: HashSet<&str> = records
        .iter()
        .filter_map(|r| match r {
            Record::Done { op_id } => Some(op_id.as_str()),
            _ => None,
        })
        .collect();
    let mut batches: HashMap<&str, BatchLog> = HashMap::new();
    let mut sealed_at: HashMap<&str, (usize, &str)> = HashMap::new();
    for r in records {
        match r {
            Record::BatchBegin { batch_id } => {
                batches
                    .entry(batch_id.as_str())
                    .or_insert_with(|| BatchLog {
                        batch_id: batch_id.clone(),
                        members: Vec::new(),
                        sealed: false,
                        phase: None,
                        aborted: false,
                    });
            }
            Record::BatchMember(m) => {
                let open = !sealed_at.contains_key(m.batch_id.as_str());
                if let Some(b) = batches.get_mut(m.batch_id.as_str()) {
                    if open {
                        b.members.push(m.clone());
                    }
                }
            }
            Record::BatchSealed {
                batch_id,
                count,
                digest,
            } => {
                if batches.contains_key(batch_id.as_str()) {
                    sealed_at
                        .entry(batch_id.as_str())
                        .or_insert((*count, digest.as_str()));
                }
            }
            Record::BatchPhase { batch_id, phase } => {
                if let Some(b) = batches.get_mut(batch_id.as_str()) {
                    b.phase = b.phase.max(Some(*phase));
                }
            }
            Record::BatchAbort { batch_id, .. } => {
                if let Some(b) = batches.get_mut(batch_id.as_str()) {
                    b.aborted = true;
                }
            }
            Record::Intent(_) | Record::Done { .. } => {}
        }
    }
    for (id, (count, digest)) in &sealed_at {
        if let Some(b) = batches.get_mut(*id) {
            b.sealed =
                *count == b.members.len() && member_digest(&b.members).is_ok_and(|d| d == *digest);
        }
    }
    let mut out = Vec::new();
    for r in records {
        match r {
            Record::Intent(i) => out.push(Step::Intent {
                intent: i.clone(),
                done: done.contains(i.op_id.as_str()),
            }),
            Record::BatchBegin { batch_id } => {
                if let Some(b) = batches.remove(batch_id.as_str()) {
                    out.push(Step::Batch(b));
                }
            }
            _ => {}
        }
    }
    out
}
