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

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MusicBrainzConfig {
    pub user_agent: String,
    pub rate_limit_per_sec: u32,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct YtmusicConfig {
    pub enabled: bool,
    pub rules: PathBuf,
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
        let cfg: Config = toml::from_str(text)?;
        cfg.validate_values()?;
        Ok(cfg)
    }

    /// ファイルから読み込み、値の検証に加えて `[paths]` の各ルートが存在することを確認する。
    /// マウント忘れで空の Library を走査すると全曲が missing になるため、ここで落とす
    pub fn load(path: &Path) -> Result<Config, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        let cfg = Config::parse(&text)?;
        cfg.validate_roots_exist()?;
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

        // [layout]: ルート相対の `/` 区切り。プレースホルダの規則は P0-11 のテンプレート展開が持つ
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

        Ok(())
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
