use rossum_local::connection::{AuthKind, Connection, ConnectionStatus};
use rossum_local::registry::Registry;
use ulid::Ulid;

#[test]
fn registry_roundtrips_via_json() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("connections.json");

    let conn = Connection {
        id: Ulid::new(),
        name: "Acme Corp — Production".into(),
        slug: "acme-corp-production".into(),
        api_base: "https://acme.app.rossum.ai/api/v1".into(),
        org_id: 12345,
        folder: tmp.path().join("acme-corp-production"),
        auth_kind: AuthKind::Password,
        last_sync_unix: Some(1763500000),
        last_status: ConnectionStatus::Ok,
        file_count: 287,
    };

    let mut reg = Registry::default();
    reg.upsert(conn.clone());
    reg.save(&path).unwrap();

    let loaded = Registry::load(&path).unwrap();
    assert_eq!(loaded.connections().len(), 1);
    assert_eq!(loaded.connections()[0].name, conn.name);
    assert_eq!(loaded.connections()[0].slug, conn.slug);
    assert_eq!(loaded.connections()[0].api_base, conn.api_base);
    assert_eq!(loaded.connections()[0].org_id, conn.org_id);
    assert_eq!(loaded.connections()[0].auth_kind, AuthKind::Password);
    assert_eq!(loaded.connections()[0].last_sync_unix, Some(1763500000));
    assert_eq!(loaded.connections()[0].file_count, 287);
    assert_eq!(loaded.connections()[0].id, conn.id);
    assert_eq!(loaded.connections()[0].folder, conn.folder);
    assert_eq!(loaded.connections()[0].last_status, conn.last_status);
}

#[test]
fn registry_load_missing_file_returns_empty() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("does-not-exist.json");
    let reg = Registry::load(&path).unwrap();
    assert!(reg.connections().is_empty());
}
