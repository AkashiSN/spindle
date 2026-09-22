//! 学習した読み取りオフセット（`drive_offsets`。P2-5、D-83）。鍵はドライブの型番（INQUIRY の
//! vendor + product）。照合が通った盤で見つかったオフセットを覚え、次の盤の吸い出しに使う

use rusqlite::{params, Connection, OptionalExtension as _};

use super::Result;

/// 覚えたオフセットの出所（照合の手法）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OffsetMethod {
    Ctdb,
    AccurateRip,
}

impl OffsetMethod {
    pub fn as_str(self) -> &'static str {
        match self {
            OffsetMethod::Ctdb => "ctdb",
            OffsetMethod::AccurateRip => "accuraterip",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LearnedOffset {
    pub offset: i32,
    pub method: String,
    pub confidence: u32,
    pub detected_at: i64,
}

pub fn get(conn: &Connection, drive: &str) -> Result<Option<LearnedOffset>> {
    Ok(conn
        .query_row(
            "SELECT offset, method, confidence, detected_at FROM drive_offsets WHERE drive = ?1",
            [drive],
            |r| {
                Ok(LearnedOffset {
                    offset: r.get(0)?,
                    method: r.get(1)?,
                    confidence: r.get(2)?,
                    detected_at: r.get(3)?,
                })
            },
        )
        .optional()?)
}

/// 覚える（同じドライブは最後に照合が通った盤の値で置き換える）
pub fn set(
    conn: &Connection,
    drive: &str,
    offset: i32,
    method: OffsetMethod,
    confidence: u32,
    now: i64,
) -> Result<()> {
    conn.execute(
        "INSERT INTO drive_offsets (drive, offset, method, confidence, detected_at)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT (drive) DO UPDATE SET offset = excluded.offset, method = excluded.method,
                                           confidence = excluded.confidence,
                                           detected_at = excluded.detected_at",
        params![drive, offset, method.as_str(), confidence, now],
    )?;
    Ok(())
}
