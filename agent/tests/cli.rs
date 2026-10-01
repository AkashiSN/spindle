use std::process::Command;

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_spindle-agent"))
}

#[test]
fn usage_exits_2() {
    let out = bin().output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    let out = bin().arg("bogus").output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    let out = bin().args(["resolve", "abc"]).output().unwrap();
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn status_without_pair_exits_1() {
    let d = tempfile::tempdir().unwrap();
    let out = bin()
        .arg("status")
        .env("SPINDLE_AGENT_STATE_DIR", d.path().join("state"))
        .env("SPINDLE_AGENT_ROOT", d.path().join("root"))
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("pair"));
}

#[test]
fn pair_refuses_plain_http() {
    let d = tempfile::tempdir().unwrap();
    let out = bin()
        .args(["pair", "http://nas:8080", "abc.def"])
        .env("SPINDLE_AGENT_STATE_DIR", d.path().join("state"))
        .env("SPINDLE_AGENT_ROOT", d.path().join("root"))
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("HTTPS"), "{err}");
    assert!(!err.contains("abc.def"), "コードを出さない: {err}");
}
