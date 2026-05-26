use rossum_local::keychain::{fake::InMemoryKeychain, Keychain, TokenEntry};
use ulid::Ulid;

#[test]
fn fake_roundtrips_token() {
    let kc = InMemoryKeychain::default();
    let id = Ulid::new();

    let entry = TokenEntry {
        token: "rsk_abc".into(),
        expires_at_unix: Some(1_800_000_000),
    };
    kc.put_token(id, &entry).unwrap();
    let got = kc.get_token(id).unwrap().unwrap();
    assert_eq!(got.token, "rsk_abc");
    assert_eq!(got.expires_at_unix, Some(1_800_000_000));
}

#[test]
fn fake_returns_none_for_missing() {
    let kc = InMemoryKeychain::default();
    let id = Ulid::new();
    assert!(kc.get_token(id).unwrap().is_none());
}

#[test]
fn fake_password_roundtrip() {
    let kc = InMemoryKeychain::default();
    let id = Ulid::new();
    kc.put_credentials(id, "alice@acme.com", "swordfish").unwrap();
    let (u, p) = kc.get_credentials(id).unwrap().unwrap();
    assert_eq!(u, "alice@acme.com");
    assert_eq!(p, "swordfish");
}

#[test]
fn fake_delete_removes_all_entries() {
    let kc = InMemoryKeychain::default();
    let id = Ulid::new();
    kc.put_token(
        id,
        &TokenEntry {
            token: "t".into(),
            expires_at_unix: None,
        },
    )
    .unwrap();
    kc.put_credentials(id, "u", "p").unwrap();
    kc.delete_all(id).unwrap();
    assert!(kc.get_token(id).unwrap().is_none());
    assert!(kc.get_credentials(id).unwrap().is_none());
}

#[test]
fn fake_put_token_overwrites_existing() {
    let kc = InMemoryKeychain::default();
    let id = Ulid::new();
    kc.put_token(
        id,
        &TokenEntry { token: "old".into(), expires_at_unix: Some(100) },
    )
    .unwrap();
    kc.put_token(
        id,
        &TokenEntry { token: "new".into(), expires_at_unix: Some(200) },
    )
    .unwrap();
    let got = kc.get_token(id).unwrap().unwrap();
    assert_eq!(got.token, "new");
    assert_eq!(got.expires_at_unix, Some(200));
}
