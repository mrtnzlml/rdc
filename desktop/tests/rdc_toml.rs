use rossum_local::rdc_toml::ensure_rdc_toml;

#[test]
fn writes_new_rdc_toml() {
    let tmp = tempfile::tempdir().unwrap();
    ensure_rdc_toml(tmp.path(), "https://x.rossum.ai/api/v1", 42).unwrap();
    let body = std::fs::read_to_string(tmp.path().join("rdc.toml")).unwrap();
    assert!(body.contains("[envs.main]"));
    assert!(body.contains(r#"api_base = "https://x.rossum.ai/api/v1""#));
    assert!(body.contains("org_id = 42"));
}

#[test]
fn updates_existing_rdc_toml_when_api_base_changed() {
    let tmp = tempfile::tempdir().unwrap();
    ensure_rdc_toml(tmp.path(), "https://old/api/v1", 42).unwrap();
    ensure_rdc_toml(tmp.path(), "https://new/api/v1", 42).unwrap();
    let body = std::fs::read_to_string(tmp.path().join("rdc.toml")).unwrap();
    assert!(body.contains("https://new/api/v1"));
    assert!(!body.contains("https://old/api/v1"));
}

#[test]
fn is_idempotent_when_unchanged() {
    let tmp = tempfile::tempdir().unwrap();
    ensure_rdc_toml(tmp.path(), "https://x/api/v1", 42).unwrap();
    let before = std::fs::metadata(tmp.path().join("rdc.toml")).unwrap().modified().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(50));
    ensure_rdc_toml(tmp.path(), "https://x/api/v1", 42).unwrap();
    let after = std::fs::metadata(tmp.path().join("rdc.toml")).unwrap().modified().unwrap();
    assert_eq!(before, after, "should not rewrite when content matches");
}
