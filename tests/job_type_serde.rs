//! JobType の JSON 名は DB の値（as_str）と同じでなければならない（Web の lib/jobs.ts が引く）

use spindle::db::jobs::JobType;

#[test]
fn serde_name_matches_as_str_for_every_type() {
    for t in JobType::ALL {
        let v = serde_json::to_value(t).unwrap();
        assert_eq!(v, serde_json::Value::String(t.as_str().to_owned()), "{t:?}");
        let back: JobType = serde_json::from_value(v).unwrap();
        assert_eq!(back, t);
    }
}
