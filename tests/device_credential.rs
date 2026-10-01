use spindle::device::credential::*;

#[test]
fn base32_round_trip_and_rfc_vectors() {
    // RFC 4648 §10（小文字・パディング無し）
    assert_eq!(base32_encode(b""), "");
    assert_eq!(base32_encode(b"f"), "my");
    assert_eq!(base32_encode(b"fo"), "mzxq");
    assert_eq!(base32_encode(b"foobar"), "mzxw6ytboi");
    assert_eq!(base32_decode("MZXW6YTBOI").unwrap(), b"foobar");
    for n in 0..40u8 {
        let v: Vec<u8> = (0..n).collect();
        assert_eq!(base32_decode(&base32_encode(&v)).unwrap(), v);
    }
}

#[test]
fn base32_rejects_bad_input() {
    assert!(base32_decode("my1").is_none(), "1 は base32 に無い");
    assert!(base32_decode("mz").is_none(), "端数ビットが 0 でない");
    assert!(
        base32_decode("m").is_none(),
        "5 bit では 1 バイトにならない"
    );
    assert!(base32_decode("my==").is_none(), "パディングは受け付けない");
}

#[test]
fn issue_and_split() {
    let a = issue(PAIR_SELECTOR_BYTES, PAIR_SECRET_BYTES).unwrap();
    let b = issue(PAIR_SELECTOR_BYTES, PAIR_SECRET_BYTES).unwrap();
    assert_ne!(a.text(), b.text());
    let (sel, secret) = split(
        &a.text().to_uppercase(),
        PAIR_SELECTOR_BYTES,
        PAIR_SECRET_BYTES,
    )
    .unwrap();
    assert_eq!(sel, a.selector);
    assert_eq!(base32_encode(&secret), a.secret);
    assert!(
        split(&a.text(), TOKEN_SELECTOR_BYTES, TOKEN_SECRET_BYTES).is_none(),
        "長さ違い"
    );
    assert!(split("abc", 8, 20).is_none());
    assert!(split(&format!("{}.{}.x", a.selector, a.secret), 8, 20).is_none());
}

#[test]
fn ct_eq_and_hash() {
    assert!(ct_eq(b"abc", b"abc"));
    assert!(!ct_eq(b"abc", b"abd"));
    assert!(!ct_eq(b"abc", b"ab"));
    assert_eq!(token_hash(b"x").len(), 64);
    assert_ne!(token_hash(b"x"), token_hash(b"y"));
}
