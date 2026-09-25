//! Cover Art Archive（D-82、P4-20）。MusicBrainz のリリース（またはリリースグループ。D-93）の MBID から
//! ジャケットを 1 枚取る。
//!
//! MB 本体の 1 req/s とは別の相手（CAA は archive.org へ 307 で飛ばす）なので、間隔制御は持たない。
//! 画像の取得で照会のスロットを食うと本末転倒なため、クライアントも分ける。UA は `[musicbrainz]` の
//! ものを共用するが、**接続に使う IP の族は共用しない**（D-82 追記）: この経路は
//! coverartarchive.org（MetaBrainz）から archive.org（Internet Archive）へ飛ぶ二段構えで、
//! archive.org に AAAA が無い。`address_family = "ipv6"` を引き継ぐと飛び先へ届かず、
//! リダイレクトを追えないまま終わる。族はホストごとに解決させる（[`CoverArtClient::new`] は族を
//! 引数で受けず `Auto` に固定する。引数があると呼び出し側が間違えられる）。
//!
//! 中継の境界（D-82）: ベース URL は http(s) + ホスト付きだけを通し、パスは [`reqwest::Url::join`] で
//! 組み立てる。リダイレクトは [`MAX_REDIRECTS`] 回まででかつ HTTPS → HTTP のダウングレードは追わない
//! （[`super::may_follow`]）。本文は [`MAX_BYTES`] まで逐次読みし、超えたら打ち切る。
//! 「画像が無い」（`Ok(None)`）と「上流がおかしい」（`Err`）は混ぜない。

use crate::config::AddressFamily;

use super::LookupError;

/// 取る画像の大きさ（CAA のサムネイル。原寸は数 MB あって画面には要らない）
const SIZE: &str = "front-500";
/// 受け取る上限。これを超える応答は途中で捨てる（CAA は 1200px でも 1 MB 程度）
const MAX_BYTES: usize = 8 * 1024 * 1024;
/// 追うリダイレクトの上限（CAA → archive.org で 1〜2 回）
const MAX_REDIRECTS: usize = 5;

pub struct CoverArtClient {
    base: reqwest::Url,
    http: reqwest::Client,
}

impl CoverArtClient {
    /// 接続に使う IP の族は引数で受けない（`Auto` に固定する）。`[musicbrainz].address_family` を
    /// 引き継げてしまうと、飛び先の archive.org に AAAA が無いぶん壊れる。引数を持たせなければ
    /// 呼び出し側が間違えようがない（この事故は実際に main の配線で起きた）
    pub fn new(base: impl AsRef<str>, user_agent: &str) -> Result<Self, LookupError> {
        let mut raw = base.as_ref().to_owned();
        if !raw.ends_with('/') {
            raw.push('/');
        }
        // 文字列連結ではなく Url::join で組み立てる。`Url::parse` だけでは `ftp:` やホスト無しも
        // 通るので、形まで確かめる（起動時に落ちるのが正しい）
        let base = reqwest::Url::parse(&raw)
            .map_err(|e| LookupError::Parse(format!("cover_art_url を読めない: {e}")))?;
        if !matches!(base.scheme(), "http" | "https") || base.host_str().is_none() {
            return Err(LookupError::Parse(format!(
                "cover_art_url は http(s) の URL（ホスト付き）: {raw:?}"
            )));
        }
        Ok(Self {
            base,
            http: super::http_client_with_redirects(
                user_agent,
                AddressFamily::Auto,
                MAX_REDIRECTS,
            )?,
        })
    }

    /// リリースの front 画像。画像が無ければ `Ok(None)`（404 は普通の結果）。
    /// 上流が壊れている（画像でない / 大きすぎる）ときは `Err`（呼び出し側が 502 にする）
    pub async fn front(&self, release_id: &str) -> Result<Option<(String, Vec<u8>)>, LookupError> {
        self.get_front(&format!("release/{release_id}/{SIZE}"))
            .await
    }

    /// リリースグループの front 画像（D-93）。CAA がグループの代表に選んだリリースの表の画像で、
    /// 通常盤に画像が無くても初回限定盤や BD 付きの版にあれば返る。境界と戻り値は [`Self::front`] と同じ
    pub async fn group_front(
        &self,
        release_group_id: &str,
    ) -> Result<Option<(String, Vec<u8>)>, LookupError> {
        self.get_front(&format!("release-group/{release_group_id}/{SIZE}"))
            .await
    }

    async fn get_front(&self, path: &str) -> Result<Option<(String, Vec<u8>)>, LookupError> {
        let url = self
            .base
            .join(path)
            .map_err(|e| LookupError::Parse(format!("URL を組み立てられない: {e}")))?;
        let mut res = self.http.get(url.clone()).send().await?;
        let status = res.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !status.is_success() {
            return Err(LookupError::Status(status.as_u16()));
        }
        let content_type = res
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_owned();
        if !content_type.starts_with("image/") {
            return Err(LookupError::Parse(format!(
                "Cover Art Archive が画像でないものを返した: {content_type:?}"
            )));
        }
        // Content-Length があれば読む前に断る
        if res.content_length().is_some_and(|n| n > MAX_BYTES as u64) {
            return Err(LookupError::Parse("画像が大きすぎる".to_owned()));
        }
        // chunked には Content-Length が付かないので、逐次読みで上限を守る
        // （`bytes()` で全部受けてから測ると、その時点でメモリに載っている）
        let mut buf: Vec<u8> = Vec::new();
        while let Some(chunk) = res.chunk().await? {
            // buf.len() は常に MAX_BYTES 以下なので引き算で安全に比べられる
            // （buf.len() + chunk.len() は理論上あふれる）
            if chunk.len() > MAX_BYTES - buf.len() {
                return Err(LookupError::Parse("画像が大きすぎる".to_owned()));
            }
            buf.extend_from_slice(&chunk);
        }
        Ok(Some((content_type, buf)))
    }
}
