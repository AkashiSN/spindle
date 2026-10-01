//! エージェントのワンタイムコードとトークン（仕様「認証と境界」、D-99）。
//! どちらも `<selector>.<secret>`。セレクタで行を 1 つ引き、その行のハッシュだけを検証する

use sha2::{Digest, Sha256};

pub const PAIR_SELECTOR_BYTES: usize = 8;
pub const PAIR_SECRET_BYTES: usize = 20;
pub const TOKEN_SELECTOR_BYTES: usize = 8;
pub const TOKEN_SECRET_BYTES: usize = 32;
pub const PAIR_CODE_TTL_SECS: i64 = 600;
pub const PAIR_MAX_ATTEMPTS: i64 = 5;

const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";

/// 小文字 base32（RFC 4648、パディング無し）
pub fn base32_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(5) * 8);
    let (mut buf, mut bits) = (0u32, 0u32);
    for &b in bytes {
        buf = (buf << 8) | u32::from(b);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(ALPHABET[((buf >> bits) & 31) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(ALPHABET[((buf << (5 - bits)) & 31) as usize] as char);
    }
    out
}

/// 大小文字を区別しない。不正な文字・端数ビットが 0 でない入力は None
pub fn base32_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 5 / 8);
    let (mut buf, mut bits) = (0u32, 0u32);
    for c in s.bytes() {
        let v = match c.to_ascii_lowercase() {
            c @ b'a'..=b'z' => c - b'a',
            c @ b'2'..=b'7' => c - b'2' + 26,
            _ => return None,
        };
        buf = (buf << 5) | u32::from(v);
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
        buf &= (1 << bits) - 1;
    }
    // 端数は 5 bit 未満で、すべて 0 でなければならない（正準な表現だけを受け付ける）
    (bits < 5 && buf == 0).then_some(out)
}

/// `<selector>.<secret>` の組（どちらも base32 の文字列のまま持つ）
pub struct Issued {
    pub selector: String,
    pub secret: String,
}

impl Issued {
    /// `"{selector}.{secret}"` を返す
    pub fn text(&self) -> String {
        format!("{}.{}", self.selector, self.secret)
    }
}

/// セレクタとシークレットをランダムに生成し、`Issued` を返す
pub fn issue(selector_bytes: usize, secret_bytes: usize) -> std::io::Result<Issued> {
    let mut selector_raw = vec![0u8; selector_bytes];
    let mut secret_raw = vec![0u8; secret_bytes];

    getrandom::fill(&mut selector_raw).map_err(std::io::Error::other)?;
    getrandom::fill(&mut secret_raw).map_err(std::io::Error::other)?;

    Ok(Issued {
        selector: base32_encode(&selector_raw),
        secret: base32_encode(&secret_raw),
    })
}

/// `<selector>.<secret>` を分ける。セレクタとシークレットが base32 として正しく、
/// 長さが期待どおり（`selector_bytes` / `secret_bytes`）のときだけ Some（シークレットは生バイト）
pub fn split(text: &str, selector_bytes: usize, secret_bytes: usize) -> Option<(String, Vec<u8>)> {
    let (sel_str, secret_str) = text.split_once('.')?;

    // セレクタを小文字に正規化
    let sel_lower = sel_str.to_ascii_lowercase();

    // セレクタをデコード
    let selector_decoded = base32_decode(&sel_lower)?;
    if selector_decoded.len() != selector_bytes {
        return None;
    }

    // シークレットをデコード
    let secret_decoded = base32_decode(secret_str)?;
    if secret_decoded.len() != secret_bytes {
        return None;
    }

    // シークレット文字列に `.` が含まれていないか確認（複数のドットを防ぐ）
    if secret_str.contains('.') {
        return None;
    }

    Some((sel_lower, secret_decoded))
}

/// トークンのシークレットの保存用ハッシュ（SHA-256 の 16 進）
pub fn token_hash(secret: &[u8]) -> String {
    let digest = Sha256::digest(secret);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// 定数時間の比較（長さが違えば false。長さは秘密ではない）
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}
