//! アップロード全体の予算（設計 §5.2）。
//!
//! 数えるもの: 全 origin の宣言サイズ（受信中・外した・壊れたものも実体が残るので含める）+
//! 全 zip の成功した世代で書いたバイト数 + GC 待ち（`garbage`）のバイト数。展開中の世代は
//! その世代の `garbage` の項だけで 1 度数える。

use crate::config::UploadConfig;

use super::manifest::{Manifest, UploadError};

/// いま予算に数えているバイト数
pub fn used_bytes(m: &Manifest) -> u64 {
    let origins = m.origins.iter().fold(0u64, |acc, o| {
        let written = o.zip.as_ref().map_or(0, |z| z.written_bytes);
        acc.saturating_add(o.total).saturating_add(written)
    });
    m.garbage
        .iter()
        .fold(origins, |acc, g| acc.saturating_add(g.bytes))
}

/// `extra` バイトを足しても `[upload].max_upload_bytes` を超えないか。超えるなら [`UploadError::Limit`]
pub fn check_budget(m: &Manifest, extra: u64, cfg: &UploadConfig) -> Result<(), UploadError> {
    if used_bytes(m).saturating_add(extra) > cfg.max_upload_bytes {
        return Err(UploadError::Limit(format!(
            "アップロード全体の上限 {} バイトを超える",
            cfg.max_upload_bytes
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::import::upload::manifest::{FileClass, Garbage, Origin, ZipState};

    fn manifest() -> Manifest {
        crate::import::upload::manifest::parse_manifest(
            br#"{"version":1,"id":"0123456789abcdef","created_at":1,"updated_at":1}"#,
        )
        .unwrap()
    }

    fn origin(id: &str, total: u64) -> Origin {
        Origin {
            id: id.into(),
            client_path: format!("{id}.flac"),
            class: FileClass::Audio,
            total,
            received: 0,
            complete: false,
            removed: false,
            damaged: false,
            zip: None,
        }
    }

    #[test]
    fn used_bytes_counts_everything_once() {
        let mut m = manifest();
        let mut removed = origin("o1", 5);
        removed.removed = true;
        let mut damaged = origin("o2", 7);
        damaged.damaged = true;
        let mut zip = origin("o3", 11);
        zip.class = FileClass::Zip;
        zip.zip = Some(ZipState {
            generation: 1,
            next_generation: 3,
            written_bytes: 13,
            // 展開中の世代 g2 は garbage の項だけで数える
            unpacking: Some(2),
            ..ZipState::default()
        });
        m.origins = vec![removed, damaged, zip];
        m.garbage = vec![Garbage {
            path: "x/o3/g2".into(),
            bytes: 17,
            added_at: 1,
        }];
        assert_eq!(used_bytes(&m), 5 + 7 + 11 + 13 + 17);

        let cfg = UploadConfig {
            max_upload_bytes: 60,
            ..UploadConfig::default()
        };
        assert!(check_budget(&m, 7, &cfg).is_ok());
        assert!(matches!(
            check_budget(&m, 8, &cfg),
            Err(UploadError::Limit(_))
        ));
    }
}
