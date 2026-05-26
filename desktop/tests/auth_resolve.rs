use rossum_local::auth::{resolve_token_for_sync, ResolveError, TokenSource};
use rossum_local::connection::{AuthKind, Connection, ConnectionStatus};
use rossum_local::keychain::{fake::InMemoryKeychain, Keychain, TokenEntry};
use std::path::PathBuf;
use ulid::Ulid;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn make_conn(id: Ulid, api_base: &str, auth: AuthKind) -> Connection {
    Connection {
        id,
        name: "t".into(),
        slug: "t".into(),
        api_base: api_base.to_string(),
        org_id: 1,
        folder: PathBuf::from("/tmp/t"),
        auth_kind: auth,
        last_sync_unix: None,
        last_status: ConnectionStatus::Never,
        file_count: 0,
    }
}

#[tokio::test]
async fn token_unexpired_is_returned_as_is() {
    let kc = InMemoryKeychain::default();
    let id = Ulid::new();
    kc.put_token(
        id,
        &TokenEntry {
            token: "valid".into(),
            expires_at_unix: Some(i64::MAX),
        },
    )
    .unwrap();
    let conn = make_conn(id, "http://unused", AuthKind::Token);
    let TokenSource { token, .. } = resolve_token_for_sync(&conn, &kc).await.unwrap();
    assert_eq!(token, "valid");
}

#[tokio::test]
async fn expired_password_token_triggers_silent_relogin() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/auth/login"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "key": "fresh-token",
            "domain": "x"
        })))
        .mount(&server)
        .await;

    let kc = InMemoryKeychain::default();
    let id = Ulid::new();
    kc.put_token(
        id,
        &TokenEntry {
            token: "stale".into(),
            expires_at_unix: Some(0), // long expired
        },
    )
    .unwrap();
    kc.put_credentials(id, "alice@acme.com", "swordfish").unwrap();

    let conn = make_conn(id, &server.uri(), AuthKind::Password);
    let TokenSource { token, refreshed } = resolve_token_for_sync(&conn, &kc).await.unwrap();
    assert_eq!(token, "fresh-token");
    assert!(refreshed);

    // Cache updated:
    let cached = kc.get_token(id).unwrap().unwrap();
    assert_eq!(cached.token, "fresh-token");
}

#[tokio::test]
async fn expired_token_only_returns_error() {
    let kc = InMemoryKeychain::default();
    let id = Ulid::new();
    kc.put_token(
        id,
        &TokenEntry {
            token: "stale".into(),
            expires_at_unix: Some(0),
        },
    )
    .unwrap();
    let conn = make_conn(id, "http://unused", AuthKind::Token);
    let err = resolve_token_for_sync(&conn, &kc).await.unwrap_err();
    assert!(matches!(err, ResolveError::SignInExpired));
}

#[tokio::test]
async fn missing_token_password_mode_logs_in() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/auth/login"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "key": "fresh", "domain": "x"
        })))
        .mount(&server)
        .await;

    let kc = InMemoryKeychain::default();
    let id = Ulid::new();
    kc.put_credentials(id, "alice", "pw").unwrap();
    let conn = make_conn(id, &server.uri(), AuthKind::Password);
    let TokenSource { token, .. } = resolve_token_for_sync(&conn, &kc).await.unwrap();
    assert_eq!(token, "fresh");
}

#[tokio::test]
async fn near_future_expiry_within_skew_falls_through_to_relogin() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/auth/login"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "key": "fresh", "domain": "x"
        })))
        .mount(&server)
        .await;

    // Token "expires in 30 seconds" — inside the 60s skew window.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let kc = InMemoryKeychain::default();
    let id = Ulid::new();
    kc.put_token(
        id,
        &TokenEntry { token: "stale".into(), expires_at_unix: Some(now + 30) },
    )
    .unwrap();
    kc.put_credentials(id, "u", "p").unwrap();

    let conn = make_conn(id, &server.uri(), AuthKind::Password);
    let TokenSource { token, refreshed } = resolve_token_for_sync(&conn, &kc).await.unwrap();
    assert_eq!(token, "fresh");
    assert!(refreshed, "skew should force re-login");
}

#[tokio::test]
async fn password_mode_without_creds_returns_missing() {
    let kc = InMemoryKeychain::default();
    let id = Ulid::new();
    // No cached token AND no credentials.
    let conn = make_conn(id, "http://unused", AuthKind::Password);
    let err = resolve_token_for_sync(&conn, &kc).await.unwrap_err();
    assert!(matches!(err, ResolveError::Missing), "got: {err:?}");
}

#[tokio::test]
async fn login_401_returns_bad_password() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/auth/login"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;

    let kc = InMemoryKeychain::default();
    let id = Ulid::new();
    kc.put_credentials(id, "u", "wrong").unwrap();
    let conn = make_conn(id, &server.uri(), AuthKind::Password);
    let err = resolve_token_for_sync(&conn, &kc).await.unwrap_err();
    assert!(matches!(err, ResolveError::BadPassword), "got: {err:?}");
}

#[tokio::test]
async fn login_500_returns_other() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/auth/login"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;

    let kc = InMemoryKeychain::default();
    let id = Ulid::new();
    kc.put_credentials(id, "u", "p").unwrap();
    let conn = make_conn(id, &server.uri(), AuthKind::Password);
    let err = resolve_token_for_sync(&conn, &kc).await.unwrap_err();
    assert!(matches!(err, ResolveError::Other(_)), "got: {err:?}");
}

#[tokio::test]
async fn login_unreachable_host_returns_network_error() {
    let kc = InMemoryKeychain::default();
    let id = Ulid::new();
    kc.put_credentials(id, "u", "p").unwrap();
    // Use a refused-port URL on localhost.
    let conn = make_conn(id, "http://127.0.0.1:1", AuthKind::Password);
    let err = resolve_token_for_sync(&conn, &kc).await.unwrap_err();
    assert!(matches!(err, ResolveError::Network(_)), "got: {err:?}");
}
