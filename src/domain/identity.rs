//! inode / audio_md5 / rel_path_key による同一性解決（SPEC §6「同一性解決の優先順位」、
//! §7.1 Phase 2–3、D-26 / D-29 / D-30）。
//!
//! どの識別子も「トラック実体の一意 ID」ではないので、各段で候補を検証する。判定は
//! **inventory 全体が揃ってから**下す（走査途中では「移動元が消えた」を判定できない）。
//! この関数は DB を触らない純関数で、スキャナ（P0-6）が Phase 1 で固定した inventory と
//! 既存行のスナップショットを渡す。
//!
//! 段は **inode → audio_md5 → rel_path_key** の順に、それぞれ全エントリを `rel_path_key`
//! 昇順で処理する。エントリごとに段をまたぐと、先に来たエントリの path 一致が後のエントリの
//! inode 一致から行を奪う（rename 後の旧パスに別ファイルが置かれた場合）。取り合いは
//! `rel_path_key` 昇順で決着するので、並列に stat しても結果は決定的。

use std::collections::{HashMap, HashSet};

/// inventory の 1 エントリ（Phase 1 で stat した結果）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// `canonical_key(rel_path)`。inventory 内で一意
    pub key: String,
    pub dev: u64,
    pub inode: u64,
    pub nlink: u64,
    pub size: u64,
    pub mtime_ns: i64,
    pub ctime_ns: i64,
}

/// 既存行（active と missing の両方）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub id: i64,
    pub key: String,
    pub dev: Option<u64>,
    pub inode: Option<u64>,
    pub size: u64,
    pub mtime_ns: i64,
    pub ctime_ns: i64,
    pub audio_md5: Option<[u8; 16]>,
    /// `missing_since IS NOT NULL`。採用されたら復活させる
    pub missing: bool,
}

/// どの段で同一と判定したか
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Via {
    Inode,
    AudioMd5,
    Path,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Identity {
    Existing {
        track_id: i64,
        via: Via,
        /// `(dev, inode, size, mtime_ns, ctime_ns)` のいずれかが行と違う（タグ読込と
        /// フィンガープリント再計算が必要）
        changed: bool,
        /// 行が missing だった（`missing_since` を NULL に戻す）
        revived: bool,
    },
    New,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub identity: Identity,
    /// `nlink > 1`、または inventory 内で同じ `(dev, inode)` を複数パスが持つ。
    /// inode 段と md5 段を飛ばして path だけで解決した（警告バッジの対象、D-26）
    pub hardlink: bool,
}

/// inventory の各エントリを既存行に対応づける。戻り値は `inventory` と同じ順・同じ長さ。
///
/// `md5_for(i)` は `inventory[i]` の `audio_md5` を返す（可逆でない・未設定・読めないときは
/// `None`）。必要なときだけ呼ぶので、呼び出し側でキャッシュしてよい
pub fn resolve(
    inventory: &[Entry],
    rows: &[Row],
    md5_for: &mut dyn FnMut(usize) -> Option<[u8; 16]>,
) -> Vec<Decision> {
    let mut order: Vec<usize> = (0..inventory.len()).collect();
    order.sort_by(|&a, &b| inventory[a].key.cmp(&inventory[b].key));

    // hardlink 判定: nlink > 1、または inventory 内で同じ inode を複数パスが持つ
    let mut inode_paths: HashMap<(u64, u64), usize> = HashMap::new();
    for e in inventory {
        *inode_paths.entry((e.dev, e.inode)).or_default() += 1;
    }
    let hardlink: Vec<bool> = inventory
        .iter()
        .map(|e| e.nlink > 1 || inode_paths[&(e.dev, e.inode)] > 1)
        .collect();
    let inventory_keys: HashSet<&str> = inventory.iter().map(|e| e.key.as_str()).collect();

    let mut by_inode: HashMap<(u64, u64), Vec<usize>> = HashMap::new();
    let mut by_md5: HashMap<[u8; 16], Vec<usize>> = HashMap::new();
    let mut by_key: HashMap<&str, usize> = HashMap::new();
    for (r, row) in rows.iter().enumerate() {
        if let (Some(dev), Some(inode)) = (row.dev, row.inode) {
            by_inode.entry((dev, inode)).or_default().push(r);
        }
        if let Some(m) = row.audio_md5 {
            by_md5.entry(m).or_default().push(r);
        }
        by_key.entry(row.key.as_str()).or_insert(r);
    }

    let mut claimed = vec![false; rows.len()];
    let mut decided: Vec<Option<Identity>> = vec![None; inventory.len()];
    let mut md5_cache: Vec<Option<Option<[u8; 16]>>> = vec![None; inventory.len()];
    let mut md5_of = |i: usize, cache: &mut Vec<Option<Option<[u8; 16]>>>| -> Option<[u8; 16]> {
        if let Some(c) = cache[i] {
            return c;
        }
        let v = md5_for(i);
        cache[i] = Some(v);
        v
    };

    // 段 1: (dev, inode)。候補行がちょうど 1 つ・未 claim・nlink = 1・size か mtime が一致。
    // 両方違えば audio_md5 の一致を要求する（inode 再利用の検出）
    for &i in &order {
        if hardlink[i] {
            continue;
        }
        let e = &inventory[i];
        let Some(cands) = by_inode.get(&(e.dev, e.inode)) else {
            continue;
        };
        let [r] = cands.as_slice() else {
            continue; // DB 側で同じ inode を複数行が持つ（過去の hardlink）なら曖昧として飛ばす
        };
        let r = *r;
        if claimed[r] {
            continue;
        }
        let row = &rows[r];
        let fast = e.size == row.size || e.mtime_ns == row.mtime_ns;
        // md5 は行が持つときだけ要求する（持たなければ照合できないので計算しても無駄）
        let verified = fast
            || row
                .audio_md5
                .is_some_and(|b| md5_of(i, &mut md5_cache) == Some(b));
        if !verified {
            continue;
        }
        claimed[r] = true;
        decided[i] = Some(Identity::Existing {
            track_id: row.id,
            via: Via::Inode,
            changed: physically_changed(e, row),
            revived: row.missing,
        });
    }

    // 段 2: audio_md5。候補がちょうど 1 行・未 claim・その行の旧 key が inventory に無い
    // （= 移動元が消えている）。候補が複数、または移動元が残っていれば新規（自動マージしない）。
    // 候補になりうる行（md5 を持ち、未 claim、旧 key が inventory に無い）が 1 つも無ければ
    // md5 を計算しても照合相手が無いので、この段は md5 を要求しない（初回スキャンと、移動の無い
    // 通常の増分スキャンでは可逆ファイルのデコードが丸ごと省ける。P1-0）
    let any_move_candidate = rows.iter().enumerate().any(|(r, row)| {
        row.audio_md5.is_some() && !claimed[r] && !inventory_keys.contains(row.key.as_str())
    });
    for &i in &order {
        if !any_move_candidate {
            break;
        }
        if decided[i].is_some() || hardlink[i] {
            continue;
        }
        let Some(m) = md5_of(i, &mut md5_cache) else {
            continue;
        };
        let Some(cands) = by_md5.get(&m) else {
            continue;
        };
        let [r] = cands.as_slice() else {
            continue;
        };
        let r = *r;
        let row = &rows[r];
        if claimed[r] || inventory_keys.contains(row.key.as_str()) {
            continue;
        }
        claimed[r] = true;
        decided[i] = Some(Identity::Existing {
            track_id: row.id,
            via: Via::AudioMd5,
            changed: true,
            revived: row.missing,
        });
    }

    // 段 3: rel_path_key。未 claim なら採用
    for &i in &order {
        if decided[i].is_some() {
            continue;
        }
        let e = &inventory[i];
        let Some(&r) = by_key.get(e.key.as_str()) else {
            continue;
        };
        if claimed[r] {
            continue;
        }
        let row = &rows[r];
        claimed[r] = true;
        decided[i] = Some(Identity::Existing {
            track_id: row.id,
            via: Via::Path,
            changed: physically_changed(e, row),
            revived: row.missing,
        });
    }

    decided
        .into_iter()
        .zip(hardlink)
        .map(|(identity, hardlink)| Decision {
            identity: identity.unwrap_or(Identity::New),
            hardlink,
        })
        .collect()
}

/// [`resolve`] が md5 を要求しうる inventory のエントリ（`resolve` の要求を含む集合。昇順）。
///
/// 要求は段 1（inode 一致で size も mtime も違い、行が md5 を持つ）と段 2（移動候補があるときの
/// 未決エントリ全部）で起きる。呼び出し側はこの集合を**並列に**計算してから、キャッシュを引く
/// コールバックで `resolve` を呼ぶ（P1-0）。md5 が無い前提で `resolve` を走らせて要求を記録する
/// ので、実際の `resolve` は段 1 でより多く決まる分、要求は減ることはあっても増えない
pub fn md5_requests(inventory: &[Entry], rows: &[Row]) -> Vec<usize> {
    let mut requested: Vec<usize> = Vec::new();
    let _ = resolve(inventory, rows, &mut |i| {
        requested.push(i);
        None
    });
    requested.sort_unstable();
    requested.dedup();
    requested
}

/// 最速パスの比較: `(dev, inode, size, mtime_ns, ctime_ns)` がすべて一致なら変更なし
fn physically_changed(e: &Entry, row: &Row) -> bool {
    !(row.dev == Some(e.dev)
        && row.inode == Some(e.inode)
        && row.size == e.size
        && row.mtime_ns == e.mtime_ns
        && row.ctime_ns == e.ctime_ns)
}
