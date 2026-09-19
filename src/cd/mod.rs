//! CD 取り込みと遡及照合（SPEC §7.2 / §7.3、§15 `cd/`）。
//!
//! CRC の計算（[`accuraterip`] / [`ctdb`]）はドライブに依存しない純粋な計算で、
//! 吸い出した PCM（P2-5）にも既存 FLAC のデコード結果（P2-9）にも同じ形で使う。
//! どちらも「トラックの並び（[`TrackLayout`]）を先に決め、サンプルを順に流す」設計で、
//! ディスク全体（最大 80 分 ≒ 850 MB）をメモリに置かない

pub mod accuraterip;
pub mod ctdb;
pub mod toc;

/// 1 セクタ（CD フレーム）のサンプル数。1 サンプル = 2ch × 16 bit
pub const SECTOR_SAMPLES: u64 = 588;

/// 音声トラックの並び（サンプル単位）。TOC のセクタ数（P2-2）か、
/// ファイルのサンプル数（P2-9）から作る。先頭トラック前の 150 セクタの pregap は含めない
/// （吸い出しにも FLAC にも入っていない）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackLayout {
    lengths: Vec<u64>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum LayoutError {
    #[error("トラックが 1 本もない")]
    Empty,
    #[error("トラック {index} のサンプル数が 0")]
    ZeroLength { index: usize },
}

impl TrackLayout {
    /// 各トラックのサンプル数から作る。空・0 長のトラックは拒否する
    pub fn from_sample_counts(counts: impl IntoIterator<Item = u64>) -> Result<Self, LayoutError> {
        let lengths: Vec<u64> = counts.into_iter().collect();
        if lengths.is_empty() {
            return Err(LayoutError::Empty);
        }
        if let Some(index) = lengths.iter().position(|&n| n == 0) {
            return Err(LayoutError::ZeroLength { index });
        }
        Ok(Self { lengths })
    }

    pub fn track_count(&self) -> usize {
        self.lengths.len()
    }

    pub fn total_samples(&self) -> u64 {
        self.lengths.iter().sum()
    }

    /// 各トラックのサンプル数
    pub fn lengths(&self) -> &[u64] {
        &self.lengths
    }
}

/// サンプルを流す側の誤り。レイアウトと受け取ったサンプル数が食い違ったとき
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CrcError {
    #[error("レイアウトのサンプル数を超えた: 期待 {expected}、受信 {received}")]
    TooManySamples { expected: u64, received: u64 },
    #[error("サンプルが不足: 期待 {expected}、受信 {received}")]
    TooFewSamples { expected: u64, received: u64 },
    #[error("L だけで終わっている（インターリーブが奇数個）")]
    IncompleteFrame,
}

/// インターリーブ i16 を (L, R) のフレーム列として、トラック境界で区切って `f` に渡す。
/// 奇数個で終わった L は次の呼び出しまで持ち越す。
/// `f(track_index, position_in_track, frames)` の `position_in_track` はそのチャンクの
/// 先頭フレームのトラック内位置（0 始まり）
struct FrameCursor {
    lengths: Vec<u64>,
    track: usize,
    pos_in_track: u64,
    received: u64,
    pending_left: Option<i16>,
}

impl FrameCursor {
    fn new(layout: &TrackLayout) -> Self {
        Self {
            lengths: layout.lengths.clone(),
            track: 0,
            pos_in_track: 0,
            received: 0,
            pending_left: None,
        }
    }

    fn expected(&self) -> u64 {
        self.lengths.iter().sum()
    }

    /// 超過を拒むときは状態を変えない（呼び出し側は正しい残りを流し直せる）
    fn feed(
        &mut self,
        interleaved: &[i16],
        mut f: impl FnMut(usize, u64, &[(i16, i16)]),
    ) -> Result<(), CrcError> {
        // 前回の持ち越し L があれば今回の先頭 R と組む。末尾に L が余れば次回へ持ち越す
        let carried = u64::from(self.pending_left.is_some());
        let frames_total = (carried + interleaved.len() as u64) / 2;
        if self.received + frames_total > self.expected() {
            return Err(CrcError::TooManySamples {
                expected: self.expected(),
                received: self.received + frames_total,
            });
        }
        let mut rest = interleaved;
        if let Some(l) = self.pending_left.take() {
            match rest.split_first() {
                Some((&r, tail)) => {
                    rest = tail;
                    self.dispatch(&[(l, r)], &mut f);
                }
                None => {
                    self.pending_left = Some(l);
                    return Ok(());
                }
            }
        }
        if rest.len() % 2 == 1 {
            if let Some((&l, body)) = rest.split_last() {
                self.pending_left = Some(l);
                rest = body;
            }
        }
        // 2 要素ずつを (L, R) に束ねる。トラック境界で分割してから渡す
        let frames: Vec<(i16, i16)> = rest
            .as_chunks::<2>()
            .0
            .iter()
            .map(|p| (p[0], p[1]))
            .collect();
        let mut offset = 0usize;
        while offset < frames.len() {
            let remaining_in_track = (self.lengths[self.track] - self.pos_in_track) as usize;
            let take = remaining_in_track.min(frames.len() - offset);
            self.dispatch(&frames[offset..offset + take], &mut f);
            offset += take;
        }
        Ok(())
    }

    /// トラックをまたがないチャンクを渡し、位置を進める
    fn dispatch(&mut self, frames: &[(i16, i16)], f: &mut impl FnMut(usize, u64, &[(i16, i16)])) {
        if frames.is_empty() {
            return;
        }
        f(self.track, self.pos_in_track, frames);
        self.pos_in_track += frames.len() as u64;
        self.received += frames.len() as u64;
        if self.pos_in_track == self.lengths[self.track] && self.track + 1 < self.lengths.len() {
            self.track += 1;
            self.pos_in_track = 0;
        }
    }

    fn finish(&self) -> Result<(), CrcError> {
        if self.pending_left.is_some() {
            return Err(CrcError::IncompleteFrame);
        }
        if self.received < self.expected() {
            return Err(CrcError::TooFewSamples {
                expected: self.expected(),
                received: self.received,
            });
        }
        Ok(())
    }
}
