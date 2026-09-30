use spindle::device::track::{parse_frame, FrameReader, TrackedDevice};

fn dev(serial: &str, state: &str, model: Option<&str>) -> TrackedDevice {
    TrackedDevice {
        serial: serial.into(),
        state: state.into(),
        model: model.map(Into::into),
    }
}

#[test]
fn parses_long_format_lines() {
    // 実機（XQ-CC44）の出力そのもの
    let body = "HQ62CQ0219             device usb:1-4 product:XQ-CC44 model:XQ_CC44 device:XQ-CC44 transport_id:1\n";
    assert_eq!(
        parse_frame(body),
        vec![dev("HQ62CQ0219", "device", Some("XQ_CC44"))]
    );
    let body = "HQ62CQ0219             authorizing usb:1-4 transport_id:2\n";
    assert_eq!(
        parse_frame(body),
        vec![dev("HQ62CQ0219", "authorizing", None)]
    );
}

#[test]
fn parses_short_format_and_skips_bad_serials() {
    let body = "SER1\tdevice\nbad serial!\tdevice\nSER2\tunauthorized\n";
    assert_eq!(
        parse_frame(body),
        vec![
            dev("SER1", "device", None),
            dev("SER2", "unauthorized", None)
        ]
    );
    assert_eq!(parse_frame(""), vec![]);
}

#[test]
fn frame_reader_handles_split_and_empty_frames() {
    let mut r = FrameReader::default();
    let body = "SER1\tdevice\n";
    let framed = format!("{:04x}{body}0000", body.len());
    let (a, b) = framed.as_bytes().split_at(7);
    assert_eq!(r.push(a).unwrap(), Vec::<Vec<TrackedDevice>>::new());
    assert_eq!(
        r.push(b).unwrap(),
        vec![vec![dev("SER1", "device", None)], vec![]]
    );
}

#[test]
fn frame_reader_rejects_garbage_length() {
    let mut r = FrameReader::default();
    assert!(r.push(b"zzzzSER1").is_err());
}
