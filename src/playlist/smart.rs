//! スマートプレイリストの評価と materialize（P1-7、D-54）。
//!
//! ルール（`rule_ast`）が正で、`playlist_items` はその評価結果のキャッシュ。評価は
//! `playlist::compile::evaluate` で、結果の並び（ORDER BY / LIMIT 適用後）をそのまま position にする。
//! 表・書き出し・position ソートは手動プレイリストと同じ経路で動く

use rusqlite::Connection;

use crate::db::playlists as dbpl;
use crate::db::{DbError, Result};

use super::compile;
use super::dsl::Rule;

/// 保存済み AST を読む。行が無い・manual・壊れた JSON なら None（壊れた JSON はログ）
pub fn load_rule(conn: &Connection, id: i64) -> Result<Option<Rule>> {
    let Some(json) = dbpl::rule_ast(conn, id)? else {
        return Ok(None);
    };
    match serde_json::from_str::<Rule>(&json) {
        Ok(r) => Ok(Some(r)),
        Err(e) => {
            tracing::warn!(playlist_id = id, error = %e, "rule_ast を読めない");
            Ok(None)
        }
    }
}

/// 評価。ルールに帰責できる実行時の失敗（`regexp()` のバックトラック上限など、ユーザ定義関数の
/// エラー）だけ [`DbError::Rule`]（API は 400）。それ以外の SQLite 障害は [`DbError::Sqlite`] のまま
/// （サーバ側の問題として 500）
pub fn evaluate(conn: &Connection, rule: &Rule) -> Result<Vec<i64>> {
    compile::evaluate(conn, rule).map_err(|e| match e {
        compile::EvalError::Sql(e) if compile::is_regexp_failure(&e) => {
            DbError::Rule(e.to_string())
        }
        compile::EvalError::Sql(e) => DbError::Sqlite(e),
        compile::EvalError::Compile(e) => DbError::Rule(e.to_string()),
    })
}

/// 1 本を再評価して項目を置き換える。`(件数, 書き換わったか)`
pub fn refresh_one(conn: &Connection, id: i64, rule: &Rule, now: i64) -> Result<(usize, bool)> {
    let ids = evaluate(conn, rule)?;
    let changed = dbpl::materialize(conn, id, &ids, now)?;
    Ok((ids.len(), changed))
}

/// 全 smart を再評価する。`(評価した本数, 書き換わった id)`。1 本の失敗は他を止めない
pub fn refresh_all(conn: &Connection, now: i64) -> Result<(usize, Vec<i64>)> {
    let mut evaluated = 0;
    let mut changed = Vec::new();
    for (id, json) in dbpl::smart_rules(conn)? {
        let rule: Rule = match serde_json::from_str(&json) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(playlist_id = id, error = %e, "rule_ast を読めない");
                continue;
            }
        };
        match refresh_one(conn, id, &rule, now) {
            Ok((_, c)) => {
                evaluated += 1;
                if c {
                    changed.push(id);
                }
            }
            Err(e) => {
                tracing::warn!(playlist_id = id, error = %e, "スマートプレイリストの評価に失敗")
            }
        }
    }
    Ok((evaluated, changed))
}
