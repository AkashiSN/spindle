//! CD 取り込みと遡及照合（SPEC §7.2 / §7.3、§15 `cd/`）。
//!
//! CRC の計算（[`accuraterip`] / [`ctdb`]）はドライブに依存しない純粋な計算で、
//! 吸い出した PCM（P2-5）にも既存 FLAC のデコード結果（P2-9）にも同じ形で使う。
//! どちらも「トラックの並び（[`TrackLayout`]）を先に決め、サンプルを順に流す」設計で、
//! ディスク全体（最大 80 分 ≒ 850 MB）をメモリに置かない

pub mod accuraterip;
pub mod crctable;
pub mod ctdb;
pub mod device;
pub mod metadata;
pub mod musicbrainz;
pub mod place;
pub mod repair;
pub mod riplog;
pub mod toc;
pub mod verify;

use std::net::SocketAddr;
use std::sync::OnceLock;
use std::time::Duration;

use crate::config::AddressFamily;

/// 照会（AccurateRip / CTDB）の失敗。応答が無い・壊れている・サーバが拒んだ
#[derive(Debug, thiserror::Error)]
pub enum LookupError {
    #[error("HTTP: {0}")]
    Http(#[from] reqwest::Error),
    #[error("HTTP {0}")]
    Status(u16),
    #[error("応答を解釈できない: {0}")]
    Parse(String),
    #[error("エントリにパリティデータが無い")]
    NoParity,
}

/// エラーを原因の末端まで繋いだ 1 行。reqwest の `Display` は「error sending request」までで、
/// TLS の失敗かタイムアウトか接続断かが消えるので、ログと API のメッセージはこれを使う
pub fn error_chain(e: &dyn std::error::Error) -> String {
    let mut parts = vec![e.to_string()];
    let mut src = e.source();
    while let Some(s) = src {
        let text = s.to_string();
        // 上位が原因の文言をそのまま含んでいることがある（二重に出さない）
        if !parts.last().is_some_and(|p| p.contains(&text)) {
            parts.push(text);
        }
        src = s.source();
    }
    parts.join(": ")
}

/// 解決したアドレスから接続に使うものを選ぶ。`Auto` は解決順のまま、`V6` / `V4` はその族だけ。
/// 指定した族が 1 つも無ければ、繋がらないよりはと全部残す
pub fn select_addrs(addrs: Vec<SocketAddr>, family: AddressFamily) -> Vec<SocketAddr> {
    let want_v6 = match family {
        AddressFamily::Auto => return addrs,
        AddressFamily::V6 => true,
        AddressFamily::V4 => false,
    };
    let picked: Vec<SocketAddr> = addrs
        .iter()
        .copied()
        .filter(|a| a.is_ipv6() == want_v6)
        .collect();
    if picked.is_empty() {
        addrs
    } else {
        picked
    }
}

/// `[musicbrainz].address_family` を反映する DNS 解決。reqwest には族を選ぶ設定が無いので、
/// 解決結果を絞って渡す
#[derive(Debug, Clone, Copy)]
struct FamilyResolver(AddressFamily);

impl reqwest::dns::Resolve for FamilyResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let family = self.0;
        Box::pin(async move {
            // ポートは接続時に差し替えられるので何でもよい
            let host = format!("{}:0", name.as_str());
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host(host).await?.collect();
            let picked = select_addrs(addrs, family);
            Ok(Box::new(picked.into_iter()) as Box<dyn Iterator<Item = SocketAddr> + Send>)
        })
    }
}

/// 照会用の HTTP クライアント。UA を必ず付け、接続 10 秒 / 全体 60 秒で諦める。
/// TLS の暗号プロバイダ（ring）はプロセスで 1 度だけ登録する（reqwest は `rustls-no-provider`。
/// aws-lc を避けるため。ビルドに cmake が要らない）
pub fn http_client(user_agent: &str) -> Result<reqwest::Client, reqwest::Error> {
    http_client_with(user_agent, AddressFamily::Auto)
}

/// 族を選べる版。`Auto` 以外なら解決結果をその族に絞る
pub fn http_client_with(
    user_agent: &str,
    family: AddressFamily,
) -> Result<reqwest::Client, reqwest::Error> {
    static PROVIDER: OnceLock<()> = OnceLock::new();
    PROVIDER.get_or_init(|| {
        // 既に別の場所で登録済みなら Err が返るが、それで構わない
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
    let mut b = reqwest::Client::builder()
        .user_agent(user_agent)
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(60));
    if family != AddressFamily::Auto {
        b = b.dns_resolver(std::sync::Arc::new(FamilyResolver(family)));
    }
    b.build()
}

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
