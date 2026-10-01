#![cfg(feature = "fake")]
mod support;

use spindle_agent::exec;
use spindle_agent::recover::{recover, resolve, Resolution};
use spindle_agent::Error;
use support::Env;

/// コピー ON で add 直後に落ち、回復が候補を出して止まった状態を作る。op_id と候補の pid を返す
fn stuck(env: &mut Env) -> (String, String) {
    env.paired();
    env.music.set_copy_on(true);
    env.server.put_track(1, "A/a.m4a", b"aaa");
    env.fp.arm("music.add:after", 1);
    let (_p, r, m) = env.confirm_all();
    assert!(env.with_ctx(|cx| exec::run(cx, &r, &m)).is_err());
    assert!(matches!(env.with_ctx(recover), Err(Error::Stop(_))));
    let s = env.state();
    let op = &s.pending_ops[0];
    (
        op.op_id.clone(),
        op.candidates.as_ref().unwrap()[0].persistent_id.clone(),
    )
}

#[test]
fn delete_track_removes_the_copy_and_the_intent() {
    let mut env = Env::new();
    let (op_id, pid) = stuck(&mut env);
    env.with_ctx(|cx| resolve(cx, &op_id, Resolution::DeleteTrack(pid)))
        .unwrap();
    assert!(env.music.tracks().is_empty());
    assert!(env.state().pending_ops.is_empty());
    assert!(!env.root.exists("A/a.m4a").unwrap());
}

#[test]
fn no_copy_created_keeps_music_untouched() {
    let mut env = Env::new();
    let (op_id, _pid) = stuck(&mut env);
    env.with_ctx(|cx| resolve(cx, &op_id, Resolution::NoCopyCreated))
        .unwrap();
    assert_eq!(env.music.tracks().len(), 1);
    assert!(env.state().pending_ops.is_empty());
}

#[test]
fn changed_candidates_are_refused() {
    let mut env = Env::new();
    let (op_id, pid) = stuck(&mut env);
    // 表示の後にユーザが同じ曲をもう 1 つ足した
    let copy = env.music.tracks()[0].location.clone().unwrap();
    let extra = env.base.join("Media/extra.m4a");
    std::fs::copy(&copy, &extra).unwrap();
    env.music.insert_track(&extra);
    let res = env.with_ctx(|cx| resolve(cx, &op_id, Resolution::DeleteTrack(pid)));
    assert!(matches!(res, Err(Error::Stop(m)) if m.contains("変わり")));
    assert_eq!(env.music.tracks().len(), 2);
    assert_eq!(
        env.state().pending_ops[0]
            .candidates
            .as_ref()
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn pid_outside_candidates_is_refused() {
    let mut env = Env::new();
    let (op_id, _pid) = stuck(&mut env);
    let res = env.with_ctx(|cx| {
        resolve(
            cx,
            &op_id,
            Resolution::DeleteTrack("NOT-A-CANDIDATE".into()),
        )
    });
    assert!(matches!(res, Err(Error::Stop(_))));
    assert_eq!(env.music.tracks().len(), 1);
}

#[test]
fn unknown_op_id_is_refused() {
    let mut env = Env::new();
    env.paired();
    assert!(matches!(
        env.with_ctx(|cx| resolve(cx, "nope", Resolution::NoCopyCreated)),
        Err(Error::Stop(_))
    ));
}

/// 候補を出して止まった後で置き先のファイルが差し替えられた: resolve はそれを消さない
#[test]
fn resolve_keeps_replaced_file_at_destination() {
    let mut env = Env::new();
    let (op_id, _pid) = stuck(&mut env);
    env.write_local("A/a.m4a", b"mine");
    env.with_ctx(|cx| resolve(cx, &op_id, Resolution::NoCopyCreated))
        .unwrap();
    assert_eq!(
        std::fs::read(env.root.abs("A/a.m4a").unwrap()).unwrap(),
        b"mine"
    );
    assert!(env.state().pending_ops.is_empty());
}
