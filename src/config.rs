//! `config.toml` の読み込みと検証（SPEC §13）。
//!
//! 構造は `deploy/config.example.toml` と一致させる。未知のキーは typo とみなして拒否し、
//! 値の妥当性は [`Config::parse`] で、ルートディレクトリの存在は [`Config::load`] で検証する。
//! 起動時にここで落ちるのは正しい挙動（fail-fast）。

use std::net::SocketAddr;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use ipnet::IpNet;
use serde::Deserialize;

/// 設定の読み込み・検証エラー
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("設定ファイル {path} を読めない: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("設定ファイルの構文または型が不正: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("設定値が不正: {0}")]
    Invalid(String),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// 読み込んだ TOML の原文（設定画面 `GET /api/config` が返す。SPEC §12.6）。秘密は config に無い
    #[serde(skip)]
    pub source: String,
    /// 読み込んだファイルのパス（`parse` だけなら None）
    #[serde(skip)]
    pub source_path: Option<PathBuf>,
    #[serde(default)]
    pub server: ServerConfig,
    pub paths: PathsConfig,
    pub layout: LayoutConfig,
    pub rip: RipConfig,
    pub encode: EncodeConfig,
    pub replaygain: ReplayGainConfig,
    pub normalize: NormalizeConfig,
    pub scan: ScanConfig,
    pub gc: GcConfig,
    pub auth: AuthConfig,
    pub backup: BackupConfig,
    pub export: ExportConfig,
    pub musicbrainz: MusicBrainzConfig,
    /// 遡及照合 / リップ検証の照会先（P2-9）。省略時は公式サーバ
    #[serde(default)]
    pub verify: VerifyConfig,
    /// Inbox の検出（P2-10、D-68）。省略時は 60 秒
    #[serde(default)]
    pub inbox: InboxConfig,
    /// 偽ハイレゾ検出（P3-5、D-71）。省略時は既定値
    #[serde(default)]
    pub hires: HiresConfig,
    pub ytmusic: YtmusicConfig,
    pub bin: BinConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// 待ち受けアドレス。コンテナ内では既定の `0.0.0.0:8080` で良い
    #[serde(default = "default_listen")]
    pub listen: SocketAddr,
}

fn default_listen() -> SocketAddr {
    SocketAddr::from(([0, 0, 0, 0], 8080))
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen: default_listen(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PathsConfig {
    pub library: PathBuf,
    pub derived: PathBuf,
    pub archive: PathBuf,
    pub inbox: PathBuf,
    pub playlists: PathBuf,
    pub data: PathBuf,
}

impl PathsConfig {
    /// `(設定キー, パス)` の一覧。検証とログ出力で使う
    fn entries(&self) -> [(&'static str, &Path); 6] {
        [
            ("paths.library", &self.library),
            ("paths.derived", &self.derived),
            ("paths.archive", &self.archive),
            ("paths.inbox", &self.inbox),
            ("paths.playlists", &self.playlists),
            ("paths.data", &self.data),
        ]
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LayoutConfig {
    pub multi_disc: String,
    pub single_disc: String,
    pub unsorted: String,
}

/// ドライブのリードオフセット。`"auto"` か整数（サンプル数）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriveOffset {
    Auto,
    Samples(i32),
}

impl<'de> Deserialize<'de> for DriveOffset {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Int(i32),
            Str(String),
        }
        match Raw::deserialize(deserializer) {
            Ok(Raw::Int(n)) => Ok(DriveOffset::Samples(n)),
            Ok(Raw::Str(s)) if s == "auto" => Ok(DriveOffset::Auto),
            Ok(Raw::Str(s)) => Err(serde::de::Error::custom(format!(
                "drive_offset は \"auto\" または整数（サンプル数）: {s:?}"
            ))),
            Err(_) => Err(serde::de::Error::custom(
                "drive_offset は \"auto\" または整数（サンプル数）",
            )),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RipConfig {
    pub device: PathBuf,
    pub drive_offset: DriveOffset,
    pub retry_on_mismatch: u32,
    pub prefer_ctdb: bool,
}

/// Derived のコーデック。D-9 により Opus のみ
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DerivedCodec {
    Opus,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EncodeConfig {
    pub derived_codec: DerivedCodec,
    pub derived_bitrate: u32,
    pub flac_compression: u8,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplayGainConfig {
    pub reference_lufs: f64,
    pub write_tags: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NormalizeConfig {
    pub wav_to_flac: bool,
    pub flac_verify_on_import: bool,
    pub flac_fix_missing_md5: bool,
    pub flac_recompress_all: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScanConfig {
    pub deep_interval_days: u32,
}

/// Inbox のポーリング（inotify はコンテナ越しに不安定なので周期投入。D-68）
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InboxConfig {
    /// 検出間隔（秒）。0 で自動なし（UI / API の手動だけ）
    #[serde(default = "default_inbox_poll")]
    pub poll_interval_secs: u32,
}

fn default_inbox_poll() -> u32 {
    60
}

impl Default for InboxConfig {
    fn default() -> Self {
        Self {
            poll_interval_secs: default_inbox_poll(),
        }
    }
}

/// 偽ハイレゾ検出のしきい値と自動投入（SPEC §7.10、D-71）
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HiresConfig {
    /// スキャン完了時に未検査の対象（可逆かつ >48 kHz または >16 bit）を自動投入する
    #[serde(default = "default_true")]
    pub check_on_import: bool,
    /// カットオフ周波数（Hz）がこれ以下なら「上げただけ」の疑い
    #[serde(default = "default_hires_cutoff")]
    pub cutoff_hz: u32,
    /// カットオフ前後 1 kHz の落差（dB）がこれ以上なら SRC の崖とみなす
    #[serde(default = "default_hires_cliff")]
    pub cliff_db: f64,
    /// カットオフがこれ以下なら崖に関わらず「上げただけ」（44.1 kHz の Nyquist + 余裕。>48 kHz の
    /// ファイルで 22.5 kHz 以上が空なのは録音として成立しない。D-71 追記）
    #[serde(default = "default_hires_hard_cutoff")]
    pub hard_cutoff_hz: u32,
}

fn default_true() -> bool {
    true
}

fn default_hires_cutoff() -> u32 {
    25_000
}

fn default_hires_cliff() -> f64 {
    10.0
}

fn default_hires_hard_cutoff() -> u32 {
    22_500
}

impl Default for HiresConfig {
    fn default() -> Self {
        Self {
            check_on_import: true,
            cutoff_hz: default_hires_cutoff(),
            cliff_db: default_hires_cliff(),
            hard_cutoff_hz: default_hires_hard_cutoff(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GcConfig {
    pub retention_days: u32,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthConfig {
    pub session_days: u32,
    pub trusted_cidrs: Vec<IpNet>,
    pub trusted_proxies: Vec<IpNet>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackupConfig {
    pub interval_hours: u32,
    pub retention_generations: u32,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExportConfig {
    pub autoexport_debounce_sec: u32,
    pub fb2k_prefix: String,
}

/// AccurateRip / CTDB の照会先。UA は `[musicbrainz].user_agent` を共用する
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerifyConfig {
    /// `dBAR-*.bin` を置いている場所（末尾 `/`）
    #[serde(default = "default_accuraterip_url")]
    pub accuraterip_url: String,
    /// CTDB の `lookup2.php`
    #[serde(default = "default_ctdb_url")]
    pub ctdb_url: String,
}

fn default_accuraterip_url() -> String {
    "http://www.accuraterip.com/accuraterip/".to_owned()
}

fn default_ctdb_url() -> String {
    "http://db.cuetools.net/lookup2.php".to_owned()
}

impl Default for VerifyConfig {
    fn default() -> Self {
        Self {
            accuraterip_url: default_accuraterip_url(),
            ctdb_url: default_ctdb_url(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MusicBrainzConfig {
    pub user_agent: String,
    pub rate_limit_per_sec: u32,
    /// `ws/2/` のベース URL（末尾 `/`）。テストと自前ミラー用に差し替え可
    #[serde(default = "default_musicbrainz_url")]
    pub url: String,
}

fn default_musicbrainz_url() -> String {
    "https://musicbrainz.org/ws/2/".to_owned()
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct YtmusicConfig {
    pub enabled: bool,
    /// メタデータプラグイン（D-69。引数配列。`sh -c` は使わない）。`enabled` なら必須
    #[serde(default)]
    pub metadata_command: Vec<String>,
    #[serde(default = "default_metadata_timeout")]
    pub metadata_timeout_secs: u32,
    /// yt-dlp のダウンロード 1 件の上限秒（D-70）
    #[serde(default = "default_download_timeout")]
    pub download_timeout_secs: u32,
}

fn default_metadata_timeout() -> u32 {
    30
}

fn default_download_timeout() -> u32 {
    900
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BinConfig {
    pub ffmpeg: String,
    pub flac: String,
    pub opusenc: String,
    pub cdparanoia: String,
    pub cdrdao: String,
    pub ytdlp: String,
}

impl BinConfig {
    fn entries(&self) -> [(&'static str, &str); 6] {
        [
            ("bin.ffmpeg", &self.ffmpeg),
            ("bin.flac", &self.flac),
            ("bin.opusenc", &self.opusenc),
            ("bin.cdparanoia", &self.cdparanoia),
            ("bin.cdrdao", &self.cdrdao),
            ("bin.ytdlp", &self.ytdlp),
        ]
    }
}

impl Config {
    /// TOML 文字列を解析し、値の妥当性を検証する。ファイルシステムは見ない
    pub fn parse(text: &str) -> Result<Config, ConfigError> {
        let mut cfg: Config = toml::from_str(text)?;
        cfg.validate_values()?;
        cfg.source = text.to_owned();
        Ok(cfg)
    }

    /// ファイルから読み込み、値の検証に加えて `[paths]` の各ルートが存在することを確認する。
    /// マウント忘れで空の Library を走査すると全曲が missing になるため、ここで落とす
    pub fn load(path: &Path) -> Result<Config, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        let mut cfg = Config::parse(&text)?;
        cfg.validate_roots_exist()?;
        cfg.source_path = Some(path.to_path_buf());
        Ok(cfg)
    }

    fn validate_values(&self) -> Result<(), ConfigError> {
        let invalid = |msg: String| Err(ConfigError::Invalid(msg));

        // [paths]: 絶対パスで、互いに重複せず、入れ子にもならない
        // （Derived が Library の中にあるとスキャナが Derived を原本として拾う）
        let entries = self.paths.entries();
        for (i, (key, path)) in entries.iter().enumerate() {
            if !path.is_absolute() {
                return invalid(format!(
                    "{key} は絶対パスでなければならない: {}",
                    path.display()
                ));
            }
            for (other_key, other) in &entries[..i] {
                if path == other {
                    return invalid(format!(
                        "{key} と {other_key} が同じディレクトリを指している: {}",
                        path.display()
                    ));
                }
                if path.starts_with(other) || other.starts_with(path) {
                    return invalid(format!(
                        "{key} ({}) と {other_key} ({}) が入れ子になっている",
                        path.display(),
                        other.display()
                    ));
                }
            }
        }

        // [layout]: ルート相対の `/` 区切り。プレースホルダは `domain::pathgen::Template` の規則
        for (key, tpl) in [
            ("layout.multi_disc", &self.layout.multi_disc),
            ("layout.single_disc", &self.layout.single_disc),
            ("layout.unsorted", &self.layout.unsorted),
        ] {
            if tpl.is_empty() || tpl.starts_with('/') || tpl.contains('\\') {
                return invalid(format!(
                    "{key} はルート相対の `/` 区切りでなければならない: {tpl:?}"
                ));
            }
            if tpl
                .split('/')
                .any(|c| c.is_empty() || c == "." || c == "..")
            {
                return invalid(format!(
                    "{key} に空・`.`・`..` のコンポーネントがある: {tpl:?}"
                ));
            }
            if let Err(e) = crate::domain::pathgen::Template::parse(tpl) {
                return invalid(format!("{key} のテンプレートが不正: {e}: {tpl:?}"));
            }
        }

        // [encode]
        if self.encode.derived_bitrate == 0 {
            return invalid("encode.derived_bitrate は 1 以上".into());
        }
        if self.encode.flac_compression > 8 {
            return invalid(format!(
                "encode.flac_compression は 0..=8: {}",
                self.encode.flac_compression
            ));
        }

        // [replaygain]: 内部表現は -18 LUFS 基準。正の値・NaN・無限大は意味を持たない
        let lufs = self.replaygain.reference_lufs;
        if !lufs.is_finite() || lufs >= 0.0 {
            return invalid(format!(
                "replaygain.reference_lufs は有限の負の値: {}",
                self.replaygain.reference_lufs
            ));
        }

        // [normalize]: 一括再エンコードは禁止事項（D-11）
        if self.normalize.flac_recompress_all {
            return invalid("normalize.flac_recompress_all は false 固定（D-11）".into());
        }

        // [gc]
        if self.gc.retention_days == 0 {
            return invalid("gc.retention_days は 1 以上".into());
        }

        // [auth]
        if self.auth.session_days == 0 {
            return invalid("auth.session_days は 1 以上".into());
        }

        // [backup]
        if self.backup.interval_hours == 0 {
            return invalid("backup.interval_hours は 1 以上".into());
        }
        if self.backup.retention_generations == 0 {
            return invalid("backup.retention_generations は 1 以上".into());
        }

        // [musicbrainz]: UA 必須・1req/s
        if self.musicbrainz.user_agent.trim().is_empty() {
            return invalid("musicbrainz.user_agent は空にできない".into());
        }
        if self.musicbrainz.rate_limit_per_sec != 1 {
            return invalid(format!(
                "musicbrainz.rate_limit_per_sec は 1 固定（MusicBrainz の規約）: {}",
                self.musicbrainz.rate_limit_per_sec
            ));
        }

        // [bin]
        for (key, value) in self.bin.entries() {
            if value.trim().is_empty() {
                return invalid(format!("{key} は空にできない"));
            }
        }

        // [ytmusic]: 有効ならプラグインのコマンドが要る
        if self.ytmusic.enabled {
            match self.ytmusic.metadata_command.first() {
                None => {
                    return invalid(
                        "ytmusic.metadata_command が空（ytmusic.enabled のときは必須）".into(),
                    )
                }
                Some(p) if p.trim().is_empty() => {
                    return invalid("ytmusic.metadata_command の先頭（プログラム）が空".into())
                }
                Some(_) => {}
            }
            if self.ytmusic.metadata_timeout_secs == 0 {
                return invalid("ytmusic.metadata_timeout_secs は 1 以上".into());
            }
            if self.ytmusic.download_timeout_secs == 0 {
                return invalid("ytmusic.download_timeout_secs は 1 以上".into());
            }
        }

        // [hires]
        if self.hires.cutoff_hz == 0 {
            return invalid("hires.cutoff_hz は 1 以上".into());
        }
        if !self.hires.cliff_db.is_finite() || self.hires.cliff_db < 0.0 {
            return invalid("hires.cliff_db は 0 以上の有限値".into());
        }
        if self.hires.hard_cutoff_hz > self.hires.cutoff_hz {
            return invalid("hires.hard_cutoff_hz は hires.cutoff_hz 以下".into());
        }

        Ok(())
    }

    /// 起動時診断（D-70）: `[bin]` の各プログラムと、`[ytmusic].enabled` なら `metadata_command[0]` が
    /// 実行できるかを確かめ、できないものを人間向けの文字列で返す。無くても起動は通す（ジョブが
    /// 使うときに失敗する）ので、呼び出し側は警告を出すだけ
    pub fn probe_executables(&self) -> Vec<String> {
        let mut candidates: Vec<(String, &str)> = self
            .bin
            .entries()
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v))
            .collect();
        if self.ytmusic.enabled {
            if let Some(program) = self.ytmusic.metadata_command.first() {
                candidates.push(("ytmusic.metadata_command".to_owned(), program.as_str()));
            }
        }
        candidates
            .into_iter()
            .filter(|(_, program)| !executable_exists(program))
            .map(|(key, program)| format!("{key} が実行できない: {program}"))
            .collect()
    }

    fn validate_roots_exist(&self) -> Result<(), ConfigError> {
        // 字面が違っても symlink で同じ実体・入れ子になり得るので、canonicalize したパスと
        // (dev, inode) の両方で比べる。bind mount は canonicalize では見えない（D-34 の保証範囲外）
        let mut seen: Vec<(&str, PathBuf, (u64, u64))> = Vec::new();
        for (key, path) in self.paths.entries() {
            let meta = match std::fs::metadata(path) {
                Ok(meta) if meta.is_dir() => meta,
                _ => {
                    return Err(ConfigError::Invalid(format!(
                        "{key} がディレクトリとして存在しない: {}",
                        path.display()
                    )));
                }
            };
            let canonical = std::fs::canonicalize(path).map_err(|e| {
                ConfigError::Invalid(format!("{key} を解決できない: {}: {e}", path.display()))
            })?;
            let ident = (meta.dev(), meta.ino());
            for (other_key, other_canonical, other_ident) in &seen {
                if *other_ident == ident {
                    return Err(ConfigError::Invalid(format!(
                        "{key} と {other_key} が同じディレクトリを指している（symlink または bind mount）: {}",
                        path.display()
                    )));
                }
                if canonical.starts_with(other_canonical) || other_canonical.starts_with(&canonical)
                {
                    return Err(ConfigError::Invalid(format!(
                        "{key} ({}) と {other_key} ({}) が symlink 解決後に入れ子になっている",
                        canonical.display(),
                        other_canonical.display()
                    )));
                }
            }
            seen.push((key, canonical, ident));
        }
        Ok(())
    }
}

/// `program` が実行できるか。`/` を含めばそのパス、含まなければ PATH を順に見る（`Command` と同じ規則）
fn executable_exists(program: &str) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    let is_exec = |p: &Path| {
        std::fs::metadata(p)
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    };
    if program.contains('/') {
        return is_exec(Path::new(program));
    }
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|dir| is_exec(&dir.join(program))))
        .unwrap_or(false)
}
