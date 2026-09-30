//! ジャーナル（仕様 ⑤「ジャーナルと中断からの回復」）

use spindle::device::journal::*;
use spindle::domain::device::EntryKind;

fn intent(op_id: &str) -> Intent {
    Intent {
        op_id: op_id.into(),
        generation: 1,
        op: IntentOp::Put,
        kind: EntryKind::Track,
        ref_id: 7,
        from: None,
        to: Some("a.opus".into()),
        token: Some("t".into()),
        size: Some(3),
        sha256: Some("h".into()),
    }
}

fn member(batch: &str, op_id: &str, id: i64) -> BatchMember {
    BatchMember {
        batch_id: batch.into(),
        op_id: op_id.into(),
        op: MemberOp::Move,
        track_id: id,
        from: format!("{id}.opus"),
        staging: format!(".spindle/moving/{op_id}"),
        new: None,
        to: format!("{id}-new.opus"),
        token: "t".into(),
        size: 1,
        sha256: "h".into(),
    }
}

fn sealed(batch: &str, members: &[BatchMember]) -> Vec<Record> {
    let mut v = vec![Record::BatchBegin {
        batch_id: batch.into(),
    }];
    v.extend(members.iter().cloned().map(Record::BatchMember));
    v.push(Record::BatchSealed {
        batch_id: batch.into(),
        count: members.len(),
        digest: member_digest(members).unwrap(),
    });
    v
}

#[test]
fn line_format_matches_spec() {
    let line = encode(&Record::Done { op_id: "x".into() }).unwrap();
    assert_eq!(line, b"{\"t\":\"done\",\"op_id\":\"x\"}\n");
    let line = String::from_utf8(encode(&Record::Intent(intent("a"))).unwrap()).unwrap();
    assert!(line.starts_with(
        "{\"t\":\"intent\",\"op_id\":\"a\",\"generation\":1,\"op\":\"put\",\"kind\":\"track\""
    ));
    assert!(line.ends_with("}\n"));
}

#[test]
fn round_trip() {
    let recs = vec![
        Record::Intent(intent("a")),
        Record::Done { op_id: "a".into() },
        Record::BatchPhase {
            batch_id: "b".into(),
            phase: Phase::Vacating,
        },
        Record::BatchAbort {
            batch_id: "b".into(),
            superseded_by: None,
        },
    ];
    assert_eq!(parse(&encode_all(&recs).unwrap()).unwrap(), recs);
}

#[test]
fn truncated_tail_is_dropped() {
    let mut bytes = encode_all(&[Record::Intent(intent("a"))]).unwrap();
    bytes.extend_from_slice(b"{\"t\":\"done\",\"op");
    assert_eq!(parse(&bytes).unwrap(), vec![Record::Intent(intent("a"))]);
}

#[test]
fn complete_tail_without_newline_is_dropped_too() {
    let mut bytes = encode_all(&[Record::Intent(intent("a"))]).unwrap();
    bytes.extend_from_slice(b"{\"t\":\"done\",\"op_id\":\"a\"}");
    assert_eq!(parse(&bytes).unwrap(), vec![Record::Intent(intent("a"))]);
}

#[test]
fn unreadable_last_complete_line_is_dropped() {
    let mut bytes = encode_all(&[Record::Intent(intent("a"))]).unwrap();
    bytes.extend_from_slice(b"{\"t\":\"do\n");
    assert_eq!(parse(&bytes).unwrap(), vec![Record::Intent(intent("a"))]);
}

#[test]
fn unreadable_middle_line_stops() {
    let mut bytes = b"garbage\n".to_vec();
    bytes.extend(encode_all(&[Record::Intent(intent("a"))]).unwrap());
    assert_eq!(
        parse(&bytes).unwrap_err(),
        JournalError::Corrupt { line: 1 }
    );
    // 知らない t も破損
    let bytes = b"{\"t\":\"phase\",\"op_id\":\"a\",\"phase\":\"vacated\"}\n{\"t\":\"done\",\"op_id\":\"a\"}\n";
    assert_eq!(parse(bytes).unwrap_err(), JournalError::Corrupt { line: 1 });
}

#[test]
fn empty_journal_is_empty() {
    assert!(parse(b"").unwrap().is_empty());
}

#[test]
fn replay_keeps_order_and_done_flags() {
    let ms = vec![member("b", "m1", 1)];
    let mut recs = vec![Record::Intent(intent("a"))];
    recs.extend(sealed("b", &ms));
    recs.push(Record::Intent(intent("c")));
    recs.push(Record::Done { op_id: "a".into() });
    let steps = replay(&recs);
    assert_eq!(steps.len(), 3);
    assert!(matches!(&steps[0], Step::Intent { intent, done: true } if intent.op_id == "a"));
    assert!(matches!(&steps[1], Step::Batch(b) if b.batch_id == "b" && b.sealed));
    assert!(matches!(&steps[2], Step::Intent { intent, done: false } if intent.op_id == "c"));
}

#[test]
fn batch_without_seal_or_bad_digest_is_not_sealed() {
    let ms = vec![member("b", "m1", 1), member("b", "m2", 2)];
    let mut unsealed = vec![Record::BatchBegin {
        batch_id: "b".into(),
    }];
    unsealed.extend(ms.iter().cloned().map(Record::BatchMember));
    assert!(matches!(&replay(&unsealed)[0], Step::Batch(b) if !b.sealed));

    let mut bad = sealed("b", &ms);
    if let Some(Record::BatchSealed { digest, .. }) = bad.last_mut() {
        *digest = "0".repeat(64);
    }
    assert!(matches!(&replay(&bad)[0], Step::Batch(b) if !b.sealed));

    let mut short = sealed("b", &ms);
    if let Some(Record::BatchSealed { count, .. }) = short.last_mut() {
        *count = 3;
    }
    assert!(matches!(&replay(&short)[0], Step::Batch(b) if !b.sealed));
}

#[test]
fn batch_phase_and_abort_are_collected() {
    let ms = vec![member("b", "m1", 1)];
    let mut recs = sealed("b", &ms);
    recs.push(Record::BatchPhase {
        batch_id: "b".into(),
        phase: Phase::Prepared,
    });
    recs.push(Record::BatchPhase {
        batch_id: "b".into(),
        phase: Phase::Vacating,
    });
    let steps = replay(&recs);
    assert!(matches!(&steps[0], Step::Batch(b) if b.phase == Some(Phase::Vacating) && !b.aborted));
    recs.push(Record::BatchAbort {
        batch_id: "b".into(),
        superseded_by: Some("c".into()),
    });
    assert!(matches!(&replay(&recs)[0], Step::Batch(b) if b.aborted));
}

#[test]
fn digest_depends_on_order() {
    let a = vec![member("b", "m1", 1), member("b", "m2", 2)];
    let b = vec![member("b", "m2", 2), member("b", "m1", 1)];
    assert_ne!(member_digest(&a).unwrap(), member_digest(&b).unwrap());
}

#[test]
fn random_ids_are_128_bit_hex() {
    let a = random_id().unwrap();
    assert_eq!(a.len(), 32);
    assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    assert_ne!(a, random_id().unwrap());
}
