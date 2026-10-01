//! 本物の Keychain でトークンの保存・読み出し・上書き・削除を確かめる（macOS だけ、`#[ignore]`）。
//! 試験用のサービス名（`spindle-agent-test-<乱数>`）を使い、最後に消す。
//! 実行: `cargo test -p spindle-agent --test keychain_real -- --ignored`
#![cfg(target_os = "macos")]

use spindle_agent::secrets::keychain::KeychainSecrets;
use spindle_agent::secrets::Secrets;

fn random_hex() -> String {
    let mut b = [0u8; 6];
    getrandom::fill(&mut b).unwrap();
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[test]
#[ignore]
fn set_get_overwrite_delete() {
    let s = KeychainSecrets::new(&format!("spindle-agent-test-{}", random_hex()), "token");
    assert_eq!(s.get().unwrap(), None);
    s.set("first-token").unwrap();
    assert_eq!(s.get().unwrap().as_deref(), Some("first-token"));
    s.set("second-token").unwrap();
    assert_eq!(s.get().unwrap().as_deref(), Some("second-token"));
    s.delete().unwrap();
    assert_eq!(s.get().unwrap(), None);
    // 無いものを消しても失敗しない
    s.delete().unwrap();
}

#[test]
#[ignore]
fn check_writable_leaves_no_items() {
    let service = format!("spindle-agent-test-{}", random_hex());
    let s = KeychainSecrets::new(&service, "token");
    s.check_writable().unwrap();
    // トークンの項目も試しの項目も残らない
    assert_eq!(s.get().unwrap(), None);
    assert_eq!(KeychainSecrets::new(&service, "probe").get().unwrap(), None);
}
