use rossum_local::slug::derive_slug;
use std::collections::HashSet;

#[test]
fn slug_strips_non_ascii() {
    let used = HashSet::new();
    assert_eq!(derive_slug("Faktura č. 1", &used), "faktura-1");
}

#[test]
fn slug_collision_appends_suffix() {
    let mut used = HashSet::new();
    used.insert("acme-corp-production".to_string());
    assert_eq!(
        derive_slug("Acme Corp — Production", &used),
        "acme-corp-production-2"
    );
}

#[test]
fn slug_empty_input_falls_back() {
    let used = HashSet::new();
    assert_eq!(derive_slug("!!!", &used), "_unnamed");
}
