use rdc::api::RossumClient;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn fixture(name: &str) -> serde_json::Value {
    let raw = std::fs::read_to_string(format!("testdata/fixtures/{name}")).unwrap();
    serde_json::from_str(&raw).unwrap()
}

#[tokio::test]
async fn list_hooks_paginates_until_done() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/v1/hooks"))
        .and(header("Authorization", "token TEST_TOKEN"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("hooks_list.json")))
        .mount(&server)
        .await;

    let client = RossumClient::new(format!("{}/api/v1", server.uri()), "TEST_TOKEN".into()).unwrap();
    let hooks = client.list_hooks().await.unwrap();
    assert_eq!(hooks.len(), 2);
    assert_eq!(hooks[0].name, "Validator: invoices");
    assert_eq!(hooks[1].name, "SFTP import");
}

#[tokio::test]
async fn auth_failure_surfaces_status_error() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/v1/hooks"))
        .respond_with(ResponseTemplate::new(401).set_body_string("unauthorized"))
        .mount(&server)
        .await;

    let client = RossumClient::new(format!("{}/api/v1", server.uri()), "BAD".into()).unwrap();
    let err = client.list_hooks().await.unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("401"), "error should mention 401, got: {msg}");
}

#[tokio::test]
async fn get_organization_returns_org() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/285704"))
        .and(header("Authorization", "token TEST_TOKEN"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&server)
        .await;

    let client = RossumClient::new(format!("{}/api/v1", server.uri()), "TEST_TOKEN".into()).unwrap();
    let org = client.get_organization(285704).await.unwrap();
    assert_eq!(org.id, 285704);
    assert_eq!(org.name, "Acme Test Org");
}

#[tokio::test]
async fn list_queues_returns_queues() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/queues"))
        .and(header("Authorization", "token TEST_TOKEN"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("queues_list.json")))
        .mount(&server)
        .await;

    let client = RossumClient::new(format!("{}/api/v1", server.uri()), "TEST_TOKEN".into()).unwrap();
    let queues = client.list_queues().await.unwrap();
    assert_eq!(queues.len(), 3);
    assert_eq!(queues[0].name, "Cost Invoices");
}

#[tokio::test]
async fn get_inbox_returns_inbox() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/inboxes/300"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("inbox_1.json")))
        .mount(&server)
        .await;
    let client = RossumClient::new(format!("{}/api/v1", server.uri()), "TEST_TOKEN".into()).unwrap();
    let inbox = client.get_inbox(300).await.unwrap();
    assert_eq!(inbox.id, 300);
    assert_eq!(inbox.email, "cost-invoices@mock.rossum.app");
}

#[tokio::test]
async fn get_schema_returns_schema() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/schemas/200"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("schema_1.json")))
        .mount(&server)
        .await;
    let client = RossumClient::new(format!("{}/api/v1", server.uri()), "TEST_TOKEN".into()).unwrap();
    let schema = client.get_schema(200).await.unwrap();
    assert_eq!(schema.id, 200);
    assert_eq!(schema.content.len(), 1);
}

#[tokio::test]
async fn list_workspaces_returns_workspaces() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/workspaces"))
        .and(header("Authorization", "token TEST_TOKEN"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("workspaces_list.json")))
        .mount(&server)
        .await;

    let client = RossumClient::new(format!("{}/api/v1", server.uri()), "TEST_TOKEN".into()).unwrap();
    let workspaces = client.list_workspaces().await.unwrap();
    assert_eq!(workspaces.len(), 2);
    assert_eq!(workspaces[0].name, "Invoices AP");
    assert_eq!(workspaces[1].name, "Purchase Orders");
}
