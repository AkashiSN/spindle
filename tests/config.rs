//! `config.toml` の読み込みと検証。
//! 仕様: docs/SPEC.md §13。実体は deploy/config.example.toml と一致させる。

use std::net::SocketAddr;

use spindle::config::{Config, ConfigError, DriveOffset};

const EXAMPLE: &str = include_str!("../deploy/config.example.toml");

/// サンプル設定の `[section]` に `body` のキーを上書き（追加）した TOML を作る
fn with_override(section: &str, body: &str) -> String {
    let mut root: toml::Table = toml::from_str(EXAMPLE).unwrap();
    let patch: toml::Table = toml::from_str(body).unwrap();
    let target = root
        .entry(section)
        .or_insert_with(|| toml::Value::Table(toml::Table::new()))
        .as_table_mut()
        .unwrap();
    for (k, v) in patch {
        target.insert(k, v);
    }
    toml::to_string(&root).unwrap()
}

/// サンプル設定の `[section]` を丸ごと `body` で差し替えた TOML を作る
fn replace_section(section: &str, body: &str) -> String {
    let mut root: toml::Table = toml::from_str(EXAMPLE).unwrap();
    let table: toml::Table = toml::from_str(body).unwrap();
    root.insert(section.to_string(), toml::Value::Table(table));
    toml::to_string(&root).unwrap()
}

#[test]
fn example_config_parses_and_validates() {
    let cfg = Config::parse(EXAMPLE).expect("deploy/config.example.toml は妥当であること");
    assert_eq!(cfg.paths.library.as_os_str(), "/library");
    assert_eq!(cfg.paths.data.as_os_str(), "/data");
    assert_eq!(cfg.encode.derived_bitrate, 128);
    assert_eq!(cfg.encode.flac_compression, 8);
    assert_eq!(cfg.replaygain.reference_lufs, -18.0);
    assert_eq!(cfg.rip.drive_offset, DriveOffset::Auto);
    assert_eq!(cfg.auth.session_days, 30);
    assert!(cfg.auth.trusted_cidrs.is_empty());
    assert_eq!(cfg.bin.cdparanoia, "cd-paranoia");
    assert_eq!(cfg.inbox.poll_interval_secs, 60);
}

#[test]
fn inbox_section_is_optional_and_accepts_zero() {
    let mut root: toml::Table = toml::from_str(EXAMPLE).unwrap();
    root.remove("inbox");
    let cfg = Config::parse(&toml::to_string(&root).unwrap()).unwrap();
    assert_eq!(cfg.inbox.poll_interval_secs, 60);
    let cfg = Config::parse(&with_override("inbox", "poll_interval_secs = 0")).unwrap();
    assert_eq!(cfg.inbox.poll_interval_secs, 0);
    assert!(Config::parse(&with_override("inbox", "interval = 5")).is_err());
}

#[test]
fn spec_config_block_matches_example() {
    // SPEC §13 の toml ブロックと deploy/config.example.toml は「一致させる」規約。
    // 構造（キー集合と値）が一致することを固定する。コメントと空白は無視。
    let spec = include_str!("../docs/SPEC.md");
    let start = spec.find("## 13. 設定").expect("SPEC §13 が存在すること");
    let block = &spec[start..];
    let begin = block.find("```toml").expect("§13 に toml ブロック") + "```toml".len();
    let end = block[begin..].find("```").expect("toml ブロックの終端") + begin;
    let spec_toml = &block[begin..end];

    let spec_value: toml::Value = toml::from_str(spec_toml).expect("SPEC の toml が妥当");
    let example_value: toml::Value = toml::from_str(EXAMPLE).expect("example の toml が妥当");
    assert_eq!(
        spec_value, example_value,
        "SPEC §13 と deploy/config.example.toml が食い違っている"
    );
}

/// サンプル設定から `[section]` を取り除いた TOML を作る
fn without_section(section: &str) -> String {
    let mut root: toml::Table = toml::from_str(EXAMPLE).unwrap();
    root.remove(section)
        .expect("取り除く対象のセクションが example にあること");
    toml::to_string(&root).unwrap()
}

#[test]
fn server_section_is_optional_and_defaults_to_all_interfaces_8080() {
    let cfg = Config::parse(&without_section("server")).unwrap();
    assert_eq!(
        cfg.server.listen,
        "0.0.0.0:8080".parse::<SocketAddr>().unwrap()
    );
}

#[test]
fn server_listen_can_be_overridden() {
    let cfg = Config::parse(&with_override("server", r#"listen = "127.0.0.1:9000""#)).unwrap();
    assert_eq!(
        cfg.server.listen,
        "127.0.0.1:9000".parse::<SocketAddr>().unwrap()
    );
}

#[test]
fn unknown_key_is_rejected() {
    // typo を黙って無視すると「設定したつもり」になる。未知キーはエラー
    let err = Config::parse(&with_override("scan", "deep_interval_day = 30")).unwrap_err();
    assert!(matches!(err, ConfigError::Parse(_)), "{err}");
}

#[test]
fn missing_section_is_rejected() {
    let toml = replace_section("paths", "");
    let err = Config::parse(&toml).unwrap_err();
    assert!(matches!(err, ConfigError::Parse(_)), "{err}");
}

#[test]
fn relative_path_is_rejected() {
    let toml = replace_section(
        "paths",
        r#"library = "library"
derived = "/derived"
archive = "/archive"
inbox = "/inbox"
playlists = "/playlists"
data = "/data""#,
    );
    let err = Config::parse(&toml).unwrap_err();
    assert!(
        matches!(err, ConfigError::Invalid(ref m) if m.contains("paths.library")),
        "{err}"
    );
}

#[test]
fn duplicate_root_paths_are_rejected() {
    // Library と Derived が同じディレクトリだとスキャナが Derived を原本として拾う
    let toml = replace_section(
        "paths",
        r#"library = "/library"
derived = "/library"
archive = "/archive"
inbox = "/inbox"
playlists = "/playlists"
data = "/data""#,
    );
    let err = Config::parse(&toml).unwrap_err();
    assert!(
        matches!(err, ConfigError::Invalid(ref m) if m.contains("derived")),
        "{err}"
    );
}

#[test]
fn nested_root_paths_are_rejected() {
    // Derived が Library の中にあると、スキャナが Derived を原本として拾う
    let toml = replace_section(
        "paths",
        r#"library = "/media"
derived = "/media/Derived"
archive = "/archive"
inbox = "/inbox"
playlists = "/playlists"
data = "/data""#,
    );
    let err = Config::parse(&toml).unwrap_err();
    assert!(
        matches!(err, ConfigError::Invalid(ref m) if m.contains("paths.derived") && m.contains("paths.library")),
        "{err}"
    );
}

#[test]
fn drive_offset_accepts_auto_and_integer() {
    let cfg = Config::parse(&with_override("rip", "drive_offset = 667")).unwrap();
    assert_eq!(cfg.rip.drive_offset, DriveOffset::Samples(667));
    let cfg = Config::parse(&with_override("rip", "drive_offset = -12")).unwrap();
    assert_eq!(cfg.rip.drive_offset, DriveOffset::Samples(-12));
    let cfg = Config::parse(&with_override("rip", r#"drive_offset = "auto""#)).unwrap();
    assert_eq!(cfg.rip.drive_offset, DriveOffset::Auto);
}

#[test]
fn drive_offset_rejects_other_strings() {
    let err = Config::parse(&with_override("rip", r#"drive_offset = "fast""#)).unwrap_err();
    assert!(matches!(err, ConfigError::Parse(_)), "{err}");
}

#[test]
fn flac_compression_out_of_range_is_rejected() {
    let err = Config::parse(&with_override("encode", "flac_compression = 9")).unwrap_err();
    assert!(
        matches!(err, ConfigError::Invalid(ref m) if m.contains("flac_compression")),
        "{err}"
    );
}

#[test]
fn derived_bitrate_zero_is_rejected() {
    let err = Config::parse(&with_override("encode", "derived_bitrate = 0")).unwrap_err();
    assert!(
        matches!(err, ConfigError::Invalid(ref m) if m.contains("derived_bitrate")),
        "{err}"
    );
}

#[test]
fn derived_codec_other_than_opus_is_rejected() {
    // D-9: Derived は Opus のみ
    let err = Config::parse(&with_override("encode", r#"derived_codec = "mp3""#)).unwrap_err();
    assert!(matches!(err, ConfigError::Parse(_)), "{err}");
}

#[test]
fn reference_lufs_must_be_negative_and_finite() {
    for bad in ["0.0", "5.0", "-inf", "nan"] {
        let toml = with_override("replaygain", &format!("reference_lufs = {bad}"));
        let err = Config::parse(&toml).unwrap_err();
        assert!(
            matches!(err, ConfigError::Invalid(ref m) if m.contains("reference_lufs")),
            "{bad}: {err}"
        );
    }
}

#[test]
fn flac_recompress_all_true_is_rejected() {
    // 禁止事項: 圧縮レベルを揃えるための一括再エンコード。設定で有効化できてはならない
    let err = Config::parse(&with_override("normalize", "flac_recompress_all = true")).unwrap_err();
    assert!(
        matches!(err, ConfigError::Invalid(ref m) if m.contains("flac_recompress_all")),
        "{err}"
    );
}

#[test]
fn gc_retention_days_zero_is_rejected() {
    // 猶予 0 日は missing になった瞬間に物理削除されるのと同じ
    let err = Config::parse(&with_override("gc", "retention_days = 0")).unwrap_err();
    assert!(
        matches!(err, ConfigError::Invalid(ref m) if m.contains("retention_days")),
        "{err}"
    );
}

#[test]
fn trusted_cidrs_are_parsed() {
    let cfg = Config::parse(&with_override(
        "auth",
        r#"trusted_cidrs = ["192.168.1.0/24", "fd00::/8"]"#,
    ))
    .unwrap();
    assert_eq!(cfg.auth.trusted_cidrs.len(), 2);
    assert_eq!(cfg.auth.trusted_cidrs[0].to_string(), "192.168.1.0/24");
}

#[test]
fn invalid_cidr_is_rejected() {
    let err = Config::parse(&with_override(
        "auth",
        r#"trusted_cidrs = ["192.168.1.0/33"]"#,
    ))
    .unwrap_err();
    assert!(matches!(err, ConfigError::Parse(_)), "{err}");
}

#[test]
fn session_days_zero_is_rejected() {
    let err = Config::parse(&with_override("auth", "session_days = 0")).unwrap_err();
    assert!(
        matches!(err, ConfigError::Invalid(ref m) if m.contains("session_days")),
        "{err}"
    );
}

#[test]
fn musicbrainz_rate_limit_must_be_exactly_one() {
    // MusicBrainz の規約は 1req/s（CLAUDE.md）。0 は無意味、2 以上は規約違反
    for bad in ["0", "2"] {
        let toml = with_override("musicbrainz", &format!("rate_limit_per_sec = {bad}"));
        let err = Config::parse(&toml).unwrap_err();
        assert!(
            matches!(err, ConfigError::Invalid(ref m) if m.contains("rate_limit_per_sec")),
            "{bad}: {err}"
        );
    }
}

#[test]
fn backup_retention_generations_zero_is_rejected() {
    let err = Config::parse(&with_override("backup", "retention_generations = 0")).unwrap_err();
    assert!(
        matches!(err, ConfigError::Invalid(ref m) if m.contains("retention_generations")),
        "{err}"
    );
}

#[test]
fn layout_template_must_be_root_relative() {
    for bad in [
        "/abs/{title}",
        "",
        "{album}//{title}",
        "{album}/../{title}",
        "{album}\\{title}",
    ] {
        let toml = with_override("layout", &format!("single_disc = {bad:?}"));
        let err = Config::parse(&toml).unwrap_err();
        assert!(
            matches!(err, ConfigError::Invalid(ref m) if m.contains("single_disc")),
            "{bad:?}: {err}"
        );
    }
    Config::parse(&with_override(
        "layout",
        r#"single_disc = "{album}/{track:02}""#,
    ))
    .unwrap();
}

#[test]
fn layout_template_placeholders_are_validated_at_load() {
    for bad in ["{album}/{genre}", "{album}/{title", "{album}/{track:x}"] {
        let toml = with_override("layout", &format!("multi_disc = {bad:?}"));
        let err = Config::parse(&toml).unwrap_err();
        assert!(
            matches!(err, ConfigError::Invalid(ref m) if m.contains("multi_disc")),
            "{bad:?}: {err}"
        );
    }
}

#[test]
fn empty_bin_path_is_rejected() {
    let err = Config::parse(&with_override("bin", r#"ffmpeg = """#)).unwrap_err();
    assert!(
        matches!(err, ConfigError::Invalid(ref m) if m.contains("bin.ffmpeg")),
        "{err}"
    );
}

#[test]
fn load_reads_file_and_checks_roots_exist() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    for name in [
        "library",
        "derived",
        "archive",
        "inbox",
        "playlists",
        "data",
    ] {
        std::fs::create_dir(root.join(name)).unwrap();
    }
    let paths = format!(
        r#"library = "{r}/library"
derived = "{r}/derived"
archive = "{r}/archive"
inbox = "{r}/inbox"
playlists = "{r}/playlists"
data = "{r}/data""#,
        r = root.display()
    );
    let toml = replace_section("paths", &paths);
    let cfg_path = root.join("config.toml");
    std::fs::write(&cfg_path, toml).unwrap();

    let cfg = Config::load(&cfg_path).expect("全ルートが存在すれば読める");
    assert_eq!(cfg.paths.library, root.join("library"));

    // Library が無い（マウント忘れ）状態で起動すると全曲 missing になるので fail-fast
    std::fs::remove_dir(root.join("library")).unwrap();
    let err = Config::load(&cfg_path).unwrap_err();
    assert!(
        matches!(err, ConfigError::Invalid(ref m) if m.contains("paths.library")),
        "{err}"
    );
}

#[test]
fn load_rejects_roots_that_are_the_same_directory_via_symlink() {
    // 字面が違っても同じ実体（symlink / bind mount）なら (dev, inode) で弾く
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    for name in ["library", "archive", "inbox", "playlists", "data"] {
        std::fs::create_dir(root.join(name)).unwrap();
    }
    std::os::unix::fs::symlink(root.join("library"), root.join("derived")).unwrap();
    let paths = format!(
        r#"library = "{r}/library"
derived = "{r}/derived"
archive = "{r}/archive"
inbox = "{r}/inbox"
playlists = "{r}/playlists"
data = "{r}/data""#,
        r = root.display()
    );
    let cfg_path = root.join("config.toml");
    std::fs::write(&cfg_path, replace_section("paths", &paths)).unwrap();

    let err = Config::load(&cfg_path).unwrap_err();
    assert!(
        matches!(err, ConfigError::Invalid(ref m) if m.contains("paths.derived") && m.contains("paths.library")),
        "{err}"
    );
}

#[test]
fn load_rejects_roots_nested_via_symlink_alias() {
    // 字面では入れ子に見えないが、symlink を解決すると Derived が Library の下にある
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    for name in ["library", "archive", "inbox", "playlists", "data"] {
        std::fs::create_dir(root.join(name)).unwrap();
    }
    std::fs::create_dir(root.join("library/Derived")).unwrap();
    std::os::unix::fs::symlink(root.join("library"), root.join("alias")).unwrap();
    let paths = format!(
        r#"library = "{r}/library"
derived = "{r}/alias/Derived"
archive = "{r}/archive"
inbox = "{r}/inbox"
playlists = "{r}/playlists"
data = "{r}/data""#,
        r = root.display()
    );
    let cfg_path = root.join("config.toml");
    std::fs::write(&cfg_path, replace_section("paths", &paths)).unwrap();

    let err = Config::load(&cfg_path).unwrap_err();
    assert!(
        matches!(err, ConfigError::Invalid(ref m) if m.contains("paths.derived") && m.contains("paths.library")),
        "{err}"
    );
}

#[test]
fn load_reports_missing_file_with_path() {
    let err = Config::load(std::path::Path::new("/nonexistent/spindle/config.toml")).unwrap_err();
    assert!(matches!(err, ConfigError::Read { .. }), "{err}");
    assert!(
        err.to_string().contains("/nonexistent/spindle/config.toml"),
        "{err}"
    );
}

#[test]
fn ytmusic_requires_a_metadata_command_when_enabled() {
    let cfg = Config::parse(&replace_section(
        "ytmusic",
        "enabled = true\nmetadata_command = [\"/usr/local/bin/plugin\", \"metadata\"]",
    ))
    .unwrap();
    assert_eq!(cfg.ytmusic.metadata_timeout_secs, 30);
    assert_eq!(cfg.ytmusic.metadata_command.len(), 2);
    // ダウンロードの上限は既定 900 秒（D-70）。example と一致する
    assert_eq!(cfg.ytmusic.download_timeout_secs, 900);
    assert_eq!(
        Config::parse(EXAMPLE)
            .unwrap()
            .ytmusic
            .download_timeout_secs,
        900
    );
    // 無効なら無くてよい
    assert!(Config::parse(&replace_section("ytmusic", "enabled = false")).is_ok());
    // 有効なのに空 / 先頭が空 / タイムアウト 0
    for body in [
        "enabled = true",
        "enabled = true\nmetadata_command = [\" \"]",
        "enabled = true\nmetadata_command = [\"p\"]\nmetadata_timeout_secs = 0",
        "enabled = true\nmetadata_command = [\"p\"]\ndownload_timeout_secs = 0",
    ] {
        assert!(
            matches!(
                Config::parse(&replace_section("ytmusic", body)),
                Err(ConfigError::Invalid(_))
            ),
            "{body}"
        );
    }
}

#[test]
fn probe_executables_reports_missing_binaries_and_plugin() {
    // 起動時診断（D-70）: [bin] と [ytmusic].metadata_command[0] の実行可否。無くても起動は通す
    let cfg = Config::parse(&replace_section(
        "ytmusic",
        "enabled = true\nmetadata_command = [\"/nonexistent/spindle-ytmusic-meta\", \"metadata\"]",
    ))
    .unwrap();
    let missing = cfg.probe_executables();
    assert!(
        missing
            .iter()
            .any(|m| m.contains("ytmusic.metadata_command")
                && m.contains("/nonexistent/spindle-ytmusic-meta")),
        "{missing:?}"
    );
    // PATH に無い名前も報告する
    let text = replace_section(
        "bin",
        "ffmpeg = \"spindle-no-such-binary\"\nflac = \"sh\"\nopusenc = \"sh\"\ncdparanoia = \"sh\"\ncdrdao = \"sh\"\nytdlp = \"sh\"",
    );
    let mut root: toml::Table = toml::from_str(&text).unwrap();
    root.insert(
        "ytmusic".into(),
        toml::Value::Table(toml::from_str("enabled = false").unwrap()),
    );
    let cfg = Config::parse(&toml::to_string(&root).unwrap()).unwrap();
    let missing = cfg.probe_executables();
    assert_eq!(missing.len(), 1, "{missing:?}");
    assert!(missing[0].contains("bin.ffmpeg"), "{missing:?}");
    // 無効なら metadata_command は見ない
    let cfg = Config::parse(&replace_section(
        "ytmusic",
        "enabled = false\nmetadata_command = [\"/nonexistent/x\"]",
    ))
    .unwrap();
    assert!(cfg
        .probe_executables()
        .iter()
        .all(|m| !m.contains("metadata_command")));
}
