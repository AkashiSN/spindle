//! 吸い出し（SPEC §7.2、D-12 / D-66 / D-83、P2-5）の部品。
//!
//! - [`shift_pcm`]: 照合で見つかったオフセットを吸い出した PCM に当てる。`CrcTable` の `offset` は
//!   「DB 側の窓が自分のデータのどこから始まるか」なので、見つかった `r` だけ前へ詰め（`r` が正）、
//!   反対の端を無音で埋める。端の `|r|` サンプルは読めなかったものとして 0（EAC の overread 無しと同じ。
//!   AccurateRip / CTDB の除外範囲に収まるので CRC は変わらない）

use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;

use super::crctable::MAX_OFFSET;

/// 1 サンプル（ステレオ 1 フレーム）のバイト数
const FRAME_BYTES: u64 = 4;

/// `src` の PCM（s16le / 2ch）を `r` サンプルずらして `dst` に書く。`r > 0` なら先頭の `r` サンプルを
/// 捨てて末尾に `r` サンプルの無音、`r < 0` なら先頭に `|r|` サンプルの無音を足して末尾を捨てる。
/// 長さは変わらない。`|r|` が [`MAX_OFFSET`] を超える・長さより大きいなら `InvalidInput`
pub fn shift_pcm(src: &Path, dst: &Path, r: i32) -> std::io::Result<()> {
    let mut input = File::open(src)?;
    let len = input.metadata()?.len();
    let shift = u64::from(r.unsigned_abs()) * FRAME_BYTES;
    if r.abs() > MAX_OFFSET || shift > len || len % FRAME_BYTES != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("PCM をずらせない: r = {r}、長さ {len} バイト"),
        ));
    }
    let mut out = BufWriter::with_capacity(1 << 20, File::create(dst)?);
    let zeros = vec![0u8; shift as usize];
    if r > 0 {
        input.seek(SeekFrom::Start(shift))?;
        std::io::copy(&mut BufReader::with_capacity(1 << 20, input), &mut out)?;
        out.write_all(&zeros)?;
    } else {
        out.write_all(&zeros)?;
        std::io::copy(
            &mut BufReader::with_capacity(1 << 20, input).take(len - shift),
            &mut out,
        )?;
    }
    let file = out.into_inner().map_err(|e| e.into_error())?;
    file.sync_all()?;
    Ok(())
}
