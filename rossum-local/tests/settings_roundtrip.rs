use rossum_local::settings::{Settings, UpdateChannel};

#[test]
fn settings_roundtrip() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("settings.json");

    let s = Settings {
        version: 1,
        default_folder_parent: tmp.path().join("Rossum"),
        update_channel: UpdateChannel::Stable,
    };
    s.save(&path).unwrap();

    let loaded = Settings::load(&path).unwrap();
    assert_eq!(loaded.default_folder_parent, s.default_folder_parent);
    assert_eq!(loaded.update_channel, UpdateChannel::Stable);
}

#[test]
fn settings_load_missing_returns_defaults() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("missing.json");
    let s = Settings::load(&path).unwrap();
    assert_eq!(s.update_channel, UpdateChannel::Stable);
}
