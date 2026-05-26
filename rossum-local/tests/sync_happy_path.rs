use rossum_local::connection::{AuthKind, Connection, ConnectionStatus};
use rossum_local::keychain::{fake::InMemoryKeychain, Keychain, TokenEntry};
use rossum_local::sync::run_sync;
use ulid::Ulid;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn run_sync_writes_organization_json_and_index_md() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": 1, "name": "t", "url": format!("{}/organizations/1", server.uri())
        })))
        .mount(&server)
        .await;

    for kind in [
        "workspaces", "queues", "schemas", "inboxes", "hooks", "rules",
        "labels", "engines", "engine_fields", "workflows", "workflow_steps",
        "email_templates",
    ] {
        Mock::given(method("GET"))
            .and(path(format!("/{}", kind)))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "results": [], "pagination": {"next": null, "total_pages": 1, "total": 0}
            })))
            .mount(&server)
            .await;
    }

    let tmp = tempfile::tempdir().unwrap();
    let kc = InMemoryKeychain::default();
    let id = Ulid::new();
    kc.put_token(
        id,
        &TokenEntry {
            token: "t".into(),
            expires_at_unix: Some(i64::MAX),
        },
    )
    .unwrap();

    let conn = Connection {
        id,
        name: "t".into(),
        slug: "t".into(),
        api_base: server.uri(),
        org_id: 1,
        folder: tmp.path().join("t"),
        auth_kind: AuthKind::Token,
        last_sync_unix: None,
        last_status: ConnectionStatus::Never,
        file_count: 0,
    };

    let outcome = run_sync(&conn, &kc).await.unwrap();
    assert!(outcome.file_count >= 1);
    assert!(conn.folder.join("envs/main/organization.json").exists());
    assert!(conn.folder.join("envs/main/_index.md").exists());
    assert!(conn.folder.join("rdc.toml").exists());
}
