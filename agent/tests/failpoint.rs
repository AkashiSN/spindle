use spindle_agent::failpoint::Failpoints;
use spindle_agent::Error;

#[test]
fn none_never_fails() {
    let fp = Failpoints::none();
    for _ in 0..3 {
        fp.hit("x").unwrap();
    }
    assert!(fp.seen().is_empty());
}

#[test]
fn armed_fails_on_nth_hit_once() {
    let fp = Failpoints::armed();
    fp.arm("state.saved", 2);
    fp.hit("state.saved").unwrap();
    fp.hit("other").unwrap();
    assert!(matches!(fp.hit("state.saved"), Err(Error::Crash(n)) if n == "state.saved"));
    // 一度発火したら解除される
    fp.hit("state.saved").unwrap();
    assert_eq!(
        fp.seen(),
        vec!["state.saved", "other", "state.saved", "state.saved"]
    );
}

#[test]
fn clones_share_state() {
    let fp = Failpoints::armed();
    let c = fp.clone();
    c.arm("a", 1);
    assert!(fp.hit("a").is_err());
}
