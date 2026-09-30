//! 端末側のシェルへ渡す文字列の検査とクォート（仕様 ⑤「接続の構成」）。
//! 端末側に渡すパス・root は必ず [`sh_quote`] を通す（クォートはここ 1 か所だけで行う）

use crate::domain::relpath::RelPath;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum QuoteError {
    #[error("NUL を含む文字列は端末へ渡せない")]
    Nul,
    #[error("改行を含む文字列は端末へ渡せない")]
    Newline,
}

/// POSIX の単引用符クォート。`'` は `'\''` に展開する。NUL と改行（CR を含む）は拒否
pub fn sh_quote(s: &str) -> Result<String, QuoteError> {
    if s.contains('\0') {
        return Err(QuoteError::Nul);
    }
    if s.contains('\n') || s.contains('\r') {
        return Err(QuoteError::Newline);
    }
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    Ok(out)
}

/// adb のシリアル: `[A-Za-z0-9._:-]+`（128 文字まで）
pub fn valid_serial(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '-'))
}

/// 登録で使う root（ボリューム内の相対パス。仕様 ⑤「登録」2）
pub const DEFAULT_ROOT: &str = "Music/spindle";

/// 内部共有ストレージを表すボリューム名
pub const VOLUME_EMULATED: &str = "emulated";

/// ボリューム: `emulated` か、SD カードの UUID（16 進 4 桁 `-` 16 進 4 桁）
pub fn valid_volume(s: &str) -> bool {
    if s == VOLUME_EMULATED {
        return true;
    }
    let b = s.as_bytes();
    b.len() == 9
        && b.iter().enumerate().all(|(i, c)| {
            if i == 4 {
                *c == b'-'
            } else {
                c.is_ascii_hexdigit()
            }
        })
}

/// root: ボリューム内の相対パス（`..` と先頭 `/` を拒否）
pub fn valid_root(s: &str) -> bool {
    RelPath::parse(s).is_ok()
}

/// ボリュームの絶対パス（`emulated` は `/storage/emulated/0`）
pub fn volume_dir(volume: &str) -> Option<String> {
    if !valid_volume(volume) {
        return None;
    }
    Some(if volume == VOLUME_EMULATED {
        "/storage/emulated/0".to_owned()
    } else {
        format!("/storage/{volume}")
    })
}

/// 端末上の root の絶対パス（`db::devices::root_prefix_utf16` と同じ形）
pub fn root_abs(volume: &str, root: &str) -> Option<String> {
    if !valid_root(root) {
        return None;
    }
    volume_dir(volume).map(|v| format!("{v}/{root}"))
}
