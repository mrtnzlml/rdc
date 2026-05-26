use rossum_local::url_normalize::normalize_api_base;

#[test]
fn strips_trailing_slash() {
    assert_eq!(
        normalize_api_base("https://x.rossum.ai/api/v1/").unwrap(),
        "https://x.rossum.ai/api/v1"
    );
}

#[test]
fn appends_api_v1_when_missing() {
    assert_eq!(
        normalize_api_base("https://x.rossum.ai").unwrap(),
        "https://x.rossum.ai/api/v1"
    );
    assert_eq!(
        normalize_api_base("https://x.rossum.ai/").unwrap(),
        "https://x.rossum.ai/api/v1"
    );
}

#[test]
fn preserves_explicit_api_v1() {
    assert_eq!(
        normalize_api_base("https://x.rossum.ai/api/v1").unwrap(),
        "https://x.rossum.ai/api/v1"
    );
}

#[test]
fn rejects_non_http_scheme() {
    assert!(normalize_api_base("ftp://x.rossum.ai/api/v1").is_err());
}

#[test]
fn rejects_garbage() {
    assert!(normalize_api_base("not-a-url").is_err());
}
