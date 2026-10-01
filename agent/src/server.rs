//! spindle の `/api/agent/*`（D-99）。本物は reqwest の blocking クライアント。
//! トークンはログ・エラーに出さない。401 はトークンの失効として pair のやり直しを案内する

use std::io::{Read, Write};
use std::time::Duration;

use agent_proto::{
    AbandonRequest, ConfirmRequest, ErrorBody, ManifestResponse, PairRequest, PairResponse, Plan,
    ReportRequest,
};
use reqwest::blocking::{Client, Response};
use reqwest::header::{AUTHORIZATION, CONTENT_RANGE, IF_MATCH, RANGE};
use reqwest::StatusCode;

use crate::{Error, Result};

#[cfg(any(test, feature = "fake"))]
pub mod fake;

/// 曲 1 本の取得の上限（止まった接続がいつまでも残らないように）
const FETCH_TIMEOUT: Duration = Duration::from_secs(2 * 60 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fetch {
    /// 本体のストリームが通信エラーなく終わった。サイズと sha256 は呼び出し側が確かめる
    Complete,
    /// 412（版が変わった・送る元が変わった）
    Changed,
    /// 404（もう desired に無い）
    Gone,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Confirmed {
    Plan(Plan),
    Changed { plan_token: String },
    OpenPlanExists,
    PendingReevaluation,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reported {
    Ok,
    Closed,
    GenerationMismatch,
    NoPlan,
    Invalid(String),
}

pub trait Server {
    fn pair(&self, code: &str) -> Result<PairResponse>;
    fn manifest(&self) -> Result<ManifestResponse>;
    /// `size` は期待する総サイズ。再開（206）の `Content-Range` の総サイズと照合する
    fn fetch(
        &self,
        track_id: i64,
        token: &str,
        size: u64,
        offset: u64,
        out: &mut dyn Write,
    ) -> Result<Fetch>;
    fn confirm(&self, plan_token: &str) -> Result<Confirmed>;
    fn open_plan(&self) -> Result<Option<Plan>>;
    fn report(&self, r: &ReportRequest) -> Result<Reported>;
    fn abandon(&self, plan_id: i64, a: &AbandonRequest) -> Result<Reported>;
}

/// `Content-Range: bytes <start>-<end>/<total>` の開始が `offset` なら総サイズを返す
pub fn check_content_range(header: &str, offset: u64) -> Option<u64> {
    let rest = header.trim().strip_prefix("bytes ")?;
    let (range, total) = rest.split_once('/')?;
    let (start, end) = range.split_once('-')?;
    let (start, end, total): (u64, u64, u64) =
        (start.parse().ok()?, end.parse().ok()?, total.parse().ok()?);
    (start == offset && start <= end && end < total).then_some(total)
}

/// 再開の 206 の `Content-Range` が、開始位置 `offset` と期待する総サイズ `size` の両方に合うか
pub fn check_resume_range(header: &str, offset: u64, size: u64) -> bool {
    check_content_range(header, offset) == Some(size)
}

/// 基底 URL のパスを `/` で終わらせる。`Url::join` は最後の `/` の後ろを置き換えるので、
/// `https://nas/spindle` のままだと `api/...` が `https://nas/api/...` になってしまう
pub fn normalize_base(mut u: reqwest::Url) -> reqwest::Url {
    if !u.path().ends_with('/') {
        let p = format!("{}/", u.path());
        u.set_path(&p);
    }
    u
}

pub struct HttpServer {
    base: reqwest::Url,
    token: Option<String>,
    client: Client,
}

impl HttpServer {
    /// `https://` だけを受け付ける。`insecure_http` なら `http://` も通し、毎回警告する
    pub fn new(url: &str, insecure_http: bool, token: Option<String>) -> Result<Self> {
        let base =
            reqwest::Url::parse(url).map_err(|e| Error::Stop(format!("URL が不正です（{e}）")))?;
        match base.scheme() {
            "https" => {}
            "http" if insecure_http => {
                eprintln!("警告: 平文の HTTP で spindle に接続します（--insecure-http）。トークンが盗聴されえます");
            }
            "http" => return Err(Error::Stop(
                "HTTPS の URL を指定してください（平文 HTTP は --insecure-http を付けたときだけ）"
                    .to_owned(),
            )),
            other => return Err(Error::Stop(format!("使えないスキームです（{other}）"))),
        }
        let _ = rustls::crypto::ring::default_provider().install_default();
        let client = Client::builder()
            .user_agent(concat!("spindle-agent/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(10))
            // blocking の ClientBuilder に read_timeout は無い。API 呼び出しは全体 60 秒、
            // 曲の取得（fetch）だけ要求ごとに長い上限を付け、TCP keepalive で死んだ接続も切る
            .timeout(Duration::from_secs(60))
            .tcp_keepalive(Duration::from_secs(30))
            // リダイレクトは追わない（Authorization を別の宛先へ送らない。HTTPS から HTTP へも落ちない）
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| Error::Server(e.to_string()))?;
        Ok(Self {
            base: normalize_base(base),
            token,
            client,
        })
    }

    fn url(&self, path: &str) -> Result<reqwest::Url> {
        self.base
            .join(path)
            .map_err(|e| Error::Server(format!("URL を組み立てられない: {e}")))
    }

    fn auth(
        &self,
        b: reqwest::blocking::RequestBuilder,
    ) -> Result<reqwest::blocking::RequestBuilder> {
        let t = self.token.as_deref().ok_or_else(|| {
            Error::Stop("トークンがありません。spindle-agent pair を実行してください（pair のときと同じ SPINDLE_AGENT_SECRETS の指定で実行しているかも確かめてください）".to_owned())
        })?;
        Ok(b.header(AUTHORIZATION, format!("Bearer {t}")))
    }

    fn send(&self, b: reqwest::blocking::RequestBuilder) -> Result<Response> {
        let res = b
            .send()
            .map_err(|e| Error::Server(e.without_url().to_string()))?;
        if res.status() == StatusCode::UNAUTHORIZED {
            return Err(Error::Stop(
                "spindle に認証されませんでした。端末タブでペアリングコードを発行し、spindle-agent pair し直してください".to_owned(),
            ));
        }
        Ok(res)
    }
}

fn error_body(res: Response) -> ErrorBody {
    let status = res.status();
    res.json::<ErrorBody>().unwrap_or(ErrorBody {
        error: status.as_str().to_owned(),
        message: None,
        plan_token: None,
    })
}

fn unexpected(res: Response) -> Error {
    let status = res.status();
    let b = error_body(res);
    Error::Server(format!(
        "{status}: {}{}",
        b.error,
        b.message.map(|m| format!("（{m}）")).unwrap_or_default()
    ))
}

fn json<T: serde::de::DeserializeOwned>(res: Response) -> Result<T> {
    res.json::<T>()
        .map_err(|e| Error::Server(format!("応答を読めない: {e}")))
}

impl Server for HttpServer {
    fn pair(&self, code: &str) -> Result<PairResponse> {
        let res = self
            .client
            .post(self.url("api/agent/pair")?)
            .json(&PairRequest {
                code: code.to_owned(),
            })
            .send()
            .map_err(|e| Error::Server(e.without_url().to_string()))?;
        match res.status() {
            StatusCode::OK => json(res),
            StatusCode::UNAUTHORIZED => Err(Error::Stop(
                "ペアリングコードが違うか、期限切れか、使用済みです。端末タブで発行し直してください".to_owned(),
            )),
            _ => Err(unexpected(res)),
        }
    }

    fn manifest(&self) -> Result<ManifestResponse> {
        let res = self.send(self.auth(self.client.get(self.url("api/agent/manifest")?))?)?;
        match res.status() {
            StatusCode::OK => json(res),
            _ => Err(unexpected(res)),
        }
    }

    fn fetch(
        &self,
        track_id: i64,
        token: &str,
        size: u64,
        offset: u64,
        out: &mut dyn Write,
    ) -> Result<Fetch> {
        let mut b = self
            .auth(
                self.client
                    .get(self.url(&format!("api/agent/files/{track_id}"))?),
            )?
            .header(IF_MATCH, format!("\"{token}\""))
            .timeout(FETCH_TIMEOUT);
        if offset > 0 {
            b = b.header(RANGE, format!("bytes={offset}-"));
        }
        let mut res = self.send(b)?;
        match res.status() {
            StatusCode::OK if offset == 0 => {}
            StatusCode::PARTIAL_CONTENT if offset > 0 => {
                let ok = res
                    .headers()
                    .get(CONTENT_RANGE)
                    .and_then(|v| v.to_str().ok())
                    .is_some_and(|v| check_resume_range(v, offset, size));
                if !ok {
                    return Err(Error::Server(
                        "206 の Content-Range が手元と合わない".to_owned(),
                    ));
                }
            }
            StatusCode::PRECONDITION_FAILED => return Ok(Fetch::Changed),
            StatusCode::NOT_FOUND => return Ok(Fetch::Gone),
            _ => return Err(unexpected(res)),
        }
        let mut buf = vec![0u8; 1 << 16];
        loop {
            let n = res
                .read(&mut buf)
                .map_err(|e| Error::Server(format!("受信が切れた: {e}")))?;
            if n == 0 {
                return Ok(Fetch::Complete);
            }
            out.write_all(&buf[..n])?;
        }
    }

    fn confirm(&self, plan_token: &str) -> Result<Confirmed> {
        let res = self.send(
            self.auth(self.client.post(self.url("api/agent/plans")?))?
                .json(&ConfirmRequest {
                    plan_token: plan_token.to_owned(),
                }),
        )?;
        match res.status() {
            StatusCode::OK | StatusCode::CREATED => Ok(Confirmed::Plan(json(res)?)),
            StatusCode::CONFLICT => {
                let b = error_body(res);
                Ok(match b.error.as_str() {
                    "plan_changed" => Confirmed::Changed {
                        plan_token: b.plan_token.unwrap_or_default(),
                    },
                    "open_plan_exists" => Confirmed::OpenPlanExists,
                    "pending_reevaluation" => Confirmed::PendingReevaluation,
                    other => return Err(Error::Server(format!("409: {other}"))),
                })
            }
            _ => Err(unexpected(res)),
        }
    }

    fn open_plan(&self) -> Result<Option<Plan>> {
        let res = self.send(self.auth(self.client.get(self.url("api/agent/plans/open")?))?)?;
        match res.status() {
            StatusCode::OK => Ok(Some(json(res)?)),
            StatusCode::NOT_FOUND => Ok(None),
            _ => Err(unexpected(res)),
        }
    }

    fn report(&self, r: &ReportRequest) -> Result<Reported> {
        let res = self.send(
            self.auth(self.client.post(self.url("api/agent/report")?))?
                .json(r),
        )?;
        reported(res)
    }

    fn abandon(&self, plan_id: i64, a: &AbandonRequest) -> Result<Reported> {
        let res = self.send(
            self.auth(
                self.client
                    .post(self.url(&format!("api/agent/plans/{plan_id}/abandon"))?),
            )?
            .json(a),
        )?;
        reported(res)
    }
}

/// report / abandon の応答。409 は `plan_closed` と `generation_mismatch` だけを区別し、
/// それ以外（`plan_unreadable` など）はエラーにする
fn reported(res: Response) -> Result<Reported> {
    match res.status() {
        StatusCode::OK => Ok(Reported::Ok),
        StatusCode::NOT_FOUND => Ok(Reported::NoPlan),
        StatusCode::BAD_REQUEST => {
            let b = error_body(res);
            Ok(Reported::Invalid(b.message.unwrap_or(b.error)))
        }
        StatusCode::CONFLICT => {
            let b = error_body(res);
            Ok(match b.error.as_str() {
                "generation_mismatch" => Reported::GenerationMismatch,
                "plan_closed" => Reported::Closed,
                other => return Err(Error::Server(format!("409: {other}"))),
            })
        }
        _ => Err(unexpected(res)),
    }
}
