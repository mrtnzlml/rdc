use rdc::api::{DataStorageClient, RossumClient};
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
async fn list_rules_returns_rules() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/rules"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("rules_list.json")))
        .mount(&server)
        .await;
    let client = RossumClient::new(format!("{}/api/v1", server.uri()), "TEST_TOKEN".into()).unwrap();
    let rules = client.list_rules().await.unwrap();
    assert_eq!(rules.len(), 1);
    assert_eq!(rules[0].name, "E-invoice Validation");
}

#[tokio::test]
async fn list_labels_returns_labels() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/labels"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("labels_list.json")))
        .mount(&server)
        .await;
    let client = RossumClient::new(format!("{}/api/v1", server.uri()), "TEST_TOKEN".into()).unwrap();
    let labels = client.list_labels().await.unwrap();
    assert_eq!(labels.len(), 2);
    assert_eq!(labels[1].name, "Needs Review");
}

#[tokio::test]
async fn list_engines_returns_engines() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/engines"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("engines_list.json")))
        .mount(&server)
        .await;
    let client = RossumClient::new(format!("{}/api/v1", server.uri()), "TEST_TOKEN".into()).unwrap();
    let engines = client.list_engines().await.unwrap();
    assert_eq!(engines.len(), 1);
    assert_eq!(engines[0].name, "Invoice Engine");
}

#[tokio::test]
async fn list_engine_fields_returns_fields() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/engine_fields"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("engine_fields_list.json")))
        .mount(&server)
        .await;
    let client = RossumClient::new(format!("{}/api/v1", server.uri()), "TEST_TOKEN".into()).unwrap();
    let fields = client.list_engine_fields().await.unwrap();
    assert_eq!(fields.len(), 2);
    assert_eq!(fields[0].name, "Invoice ID");
}

#[tokio::test]
async fn update_hook_patches_and_returns_response() {
    let server = MockServer::start().await;
    Mock::given(method("PATCH"))
        .and(path("/api/v1/hooks/1"))
        .and(header("Authorization", "token TEST_TOKEN"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("hook_1.json")))
        .mount(&server).await;

    let client = RossumClient::new(format!("{}/api/v1", server.uri()), "TEST_TOKEN".into()).unwrap();
    let hook: rdc::model::Hook = serde_json::from_value(fixture("hook_1.json")).unwrap();
    let updated = client.update_hook(1, &hook).await.unwrap();
    assert_eq!(updated.id, 1);
    assert_eq!(updated.name, "Validator: invoices");
}

#[tokio::test]
async fn list_workflows_returns_workflows() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/workflows"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("workflows_list.json")))
        .mount(&server)
        .await;
    let client = RossumClient::new(format!("{}/api/v1", server.uri()), "TEST_TOKEN".into()).unwrap();
    let workflows = client.list_workflows().await.unwrap();
    assert_eq!(workflows.len(), 1);
    assert_eq!(workflows[0].name, "AP Approval Flow");
}

#[tokio::test]
async fn list_workflow_steps_returns_steps() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/workflow_steps"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("workflow_steps_list.json")))
        .mount(&server)
        .await;
    let client = RossumClient::new(format!("{}/api/v1", server.uri()), "TEST_TOKEN".into()).unwrap();
    let steps = client.list_workflow_steps().await.unwrap();
    assert_eq!(steps.len(), 2);
    assert_eq!(steps[1].name, "Finance Approval");
}

#[tokio::test]
async fn list_email_templates_returns_templates() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/email_templates"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("email_templates_list.json")))
        .mount(&server)
        .await;
    let client = RossumClient::new(format!("{}/api/v1", server.uri()), "TEST_TOKEN".into()).unwrap();
    let templates = client.list_email_templates().await.unwrap();
    assert_eq!(templates.len(), 1);
    assert_eq!(templates[0].subject, "Your invoice was rejected");
}

#[tokio::test]
async fn data_storage_list_collections() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/data/v1/collections"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("mdh_collections.json")))
        .mount(&server)
        .await;
    let client = DataStorageClient::new(format!("{}/data/v1", server.uri()), "TEST_TOKEN".into()).unwrap();
    let cols = client.list_collections().await.unwrap();
    assert_eq!(cols.len(), 2);
    assert_eq!(cols[0].name, "vendors");
}

#[tokio::test]
async fn data_storage_list_indexes() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/data/v1/collections/vendors/indexes"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("mdh_indexes_vendors.json")))
        .mount(&server)
        .await;
    let client = DataStorageClient::new(format!("{}/data/v1", server.uri()), "TEST_TOKEN".into()).unwrap();
    let ix = client.list_indexes("vendors").await.unwrap();
    assert_eq!(ix.len(), 2);
}

#[tokio::test]
async fn data_storage_list_search_indexes() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/data/v1/collections/vendors/search-indexes"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("mdh_search_indexes_vendors.json")))
        .mount(&server)
        .await;
    let client = DataStorageClient::new(format!("{}/data/v1", server.uri()), "TEST_TOKEN".into()).unwrap();
    let s = client.list_search_indexes("vendors").await.unwrap();
    assert_eq!(s.len(), 1);
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
