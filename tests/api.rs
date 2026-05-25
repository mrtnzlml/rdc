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
    let hooks = client.list_hooks(None).await.unwrap();
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
    let err = client.list_hooks(None).await.unwrap_err();
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
    let org = client.get_organization(285704, None).await.unwrap();
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
    let queues = client.list_queues(None).await.unwrap();
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
    let inbox = client.get_inbox(300, None).await.unwrap();
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
    let schema = client.get_schema(200, None).await.unwrap();
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
    let rules = client.list_rules(None).await.unwrap();
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
    let labels = client.list_labels(None).await.unwrap();
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
    let engines = client.list_engines(None).await.unwrap();
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
    let fields = client.list_engine_fields(None).await.unwrap();
    assert_eq!(fields.len(), 2);
    assert_eq!(fields[0].name, "Invoice ID");
}

#[tokio::test]
async fn update_rule_patches_and_returns_response() {
    let server = MockServer::start().await;
    Mock::given(method("PATCH"))
        .and(path("/api/v1/rules/2597"))
        .and(header("Authorization", "token TEST_TOKEN"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": 2597,
            "url": "https://mock.rossum.app/api/v1/rules/2597",
            "name": "E-invoice Validation",
            "queues": []
        })))
        .mount(&server).await;

    let client = RossumClient::new(format!("{}/api/v1", server.uri()), "TEST_TOKEN".into()).unwrap();
    let rule: rdc::model::Rule = serde_json::from_value(serde_json::json!({
        "id": 2597,
        "url": "https://mock.rossum.app/api/v1/rules/2597",
        "name": "E-invoice Validation",
        "queues": []
    })).unwrap();
    let updated = client.update_rule(2597, &rule, None).await.unwrap();
    assert_eq!(updated.id, 2597);
}

#[tokio::test]
async fn update_label_patches_and_returns_response() {
    let server = MockServer::start().await;
    Mock::given(method("PATCH"))
        .and(path("/api/v1/labels/11"))
        .and(header("Authorization", "token TEST_TOKEN"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": 11,
            "url": "https://mock.rossum.app/api/v1/labels/11",
            "name": "Priority High",
            "organization": "https://mock.rossum.app/api/v1/organizations/285704"
        })))
        .mount(&server).await;

    let client = RossumClient::new(format!("{}/api/v1", server.uri()), "TEST_TOKEN".into()).unwrap();
    let label: rdc::model::Label = serde_json::from_value(serde_json::json!({
        "id": 11,
        "url": "https://mock.rossum.app/api/v1/labels/11",
        "name": "Priority High",
        "organization": "https://mock.rossum.app/api/v1/organizations/285704"
    })).unwrap();
    let updated = client.update_label(11, &label, None).await.unwrap();
    assert_eq!(updated.id, 11);
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
    let updated = client.update_hook(1, &hook, None).await.unwrap();
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
    let workflows = client.list_workflows(None).await.unwrap();
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
    let steps = client.list_workflow_steps(None).await.unwrap();
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
    let templates = client.list_email_templates(None).await.unwrap();
    assert_eq!(templates.len(), 1);
    assert_eq!(templates[0].subject, "Your invoice was rejected");
}

#[tokio::test]
async fn data_storage_list_collections() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/svc/data-storage/api/v1/collections/list"))
        .and(header("Authorization", "Bearer TEST_TOKEN"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("mdh_collections.json")))
        .mount(&server)
        .await;
    let client = DataStorageClient::new(
        format!("{}/svc/data-storage/api", server.uri()), "TEST_TOKEN".into(),
    ).unwrap();
    let cols = client.list_collections(None).await.unwrap();
    assert_eq!(cols.len(), 2);
    assert_eq!(cols[0].name, "vendors");
}

#[tokio::test]
async fn data_storage_list_indexes() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/svc/data-storage/api/v1/indexes/list"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("mdh_indexes_vendors.json")))
        .mount(&server)
        .await;
    let client = DataStorageClient::new(
        format!("{}/svc/data-storage/api", server.uri()), "TEST_TOKEN".into(),
    ).unwrap();
    let ix = client.list_indexes("vendors", None).await.unwrap();
    assert_eq!(ix.len(), 2);
}

#[tokio::test]
async fn data_storage_list_search_indexes() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/svc/data-storage/api/v1/search_indexes/list"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("mdh_search_indexes_vendors.json")))
        .mount(&server)
        .await;
    let client = DataStorageClient::new(
        format!("{}/svc/data-storage/api", server.uri()), "TEST_TOKEN".into(),
    ).unwrap();
    let s = client.list_search_indexes("vendors", None).await.unwrap();
    assert_eq!(s.len(), 1);
}

#[tokio::test]
async fn data_storage_returns_error_on_non_ok_envelope() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/svc/data-storage/api/v1/collections/list"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "code": "internal_error",
            "message": "boom",
            "result": serde_json::Value::Null
        })))
        .mount(&server)
        .await;
    let client = DataStorageClient::new(
        format!("{}/svc/data-storage/api", server.uri()), "TEST_TOKEN".into(),
    ).unwrap();
    let err = client.list_collections(None).await.unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("internal_error"), "msg: {msg}");
    assert!(msg.contains("boom"), "msg: {msg}");
}

#[tokio::test]
async fn retries_on_429_then_succeeds() {
    let server = MockServer::start().await;

    // First call → 429 (rate limited). Higher priority + up_to_n_times(1)
    // means it matches once and is then exhausted, so the second mock takes
    // over for subsequent calls.
    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "1"))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&server)
        .await;

    let client = RossumClient::new(format!("{}/api/v1", server.uri()), "T".into()).unwrap();
    let org = client.get_organization(1, None).await.unwrap();
    // The 200 response uses fixture organization id 285704; we just care
    // that the retry succeeded (request didn't surface the 429 to the caller).
    assert_eq!(org.id, 285704);
}

#[tokio::test]
async fn retries_on_503_then_succeeds() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&server)
        .await;

    let client = RossumClient::new(format!("{}/api/v1", server.uri()), "T".into()).unwrap();
    let org = client.get_organization(1, None).await.unwrap();
    assert_eq!(org.id, 285704);
}

#[tokio::test]
async fn does_not_retry_on_500() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(500).set_body_string("real bug"))
        .expect(1) // Must be hit exactly once — no retries.
        .mount(&server)
        .await;

    let client = RossumClient::new(format!("{}/api/v1", server.uri()), "T".into()).unwrap();
    let err = client.get_organization(1, None).await.unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("500"), "msg: {msg}");
}

#[tokio::test]
async fn does_not_retry_on_404() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&server)
        .await;

    let client = RossumClient::new(format!("{}/api/v1", server.uri()), "T".into()).unwrap();
    let err = client.get_organization(1, None).await.unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("404"), "msg: {msg}");
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
    let workspaces = client.list_workspaces(None).await.unwrap();
    assert_eq!(workspaces.len(), 2);
    assert_eq!(workspaces[0].name, "Invoices AP");
    assert_eq!(workspaces[1].name, "Purchase Orders");
}

#[tokio::test]
async fn create_hook_via_install_posts_to_create_endpoint() {
    use rdc::model::Hook;
    use serde_json::json;
    use wiremock::matchers::body_json;

    let server = MockServer::start().await;
    let install_body = json!({
        "name": "Master Data Hub",
        "hook_template": "https://elis.rossum.ai/api/v1/hook_templates/39",
        "events": ["annotation_content.initialize"],
        "queues": [],
        "token_owner": "https://elis.rossum.ai/api/v1/users/938493"
    });
    let server_response = json!({
        "id": 1798871,
        "url": format!("{}/api/v1/hooks/1798871", server.uri()),
        "name": "Master Data Hub",
        "type": "webhook",
        "events": ["annotation_content.initialize"],
        "queues": [],
        "config": { "private": true, "timeout_s": 60 },
        "settings": { "configurations": [] },
        "extension_source": "rossum_store",
        "hook_template": "https://elis.rossum.ai/api/v1/hook_templates/39",
        "token_owner": "https://elis.rossum.ai/api/v1/users/938493"
    });

    Mock::given(method("POST"))
        .and(path("/api/v1/hooks/create"))
        .and(header("Authorization", "token TEST_TOKEN"))
        .and(body_json(&install_body))
        .respond_with(ResponseTemplate::new(201).set_body_json(&server_response))
        .mount(&server)
        .await;

    let client = RossumClient::new(format!("{}/api/v1", server.uri()), "TEST_TOKEN".into()).unwrap();
    let hook: Hook = client.create_hook_via_install(&install_body, None).await.unwrap();
    assert_eq!(hook.id, 1798871);
    assert_eq!(hook.extension_source(), Some("rossum_store"));
}

#[tokio::test]
async fn list_hook_templates_paginates() {
    use rdc::model::HookTemplate;
    use serde_json::json;
    let server = MockServer::start().await;
    let page1 = json!({
        "pagination": { "next": format!("{}/api/v1/hook_templates?page=2", server.uri()) },
        "results": [
            {"url": format!("{}/api/v1/hook_templates/39", server.uri()),
             "name": "Master Data Hub", "type": "webhook",
             "extension_source": "rossum_store", "install_action": "copy"}
        ]
    });
    let page2 = json!({
        "pagination": { "next": null },
        "results": [
            {"url": format!("{}/api/v1/hook_templates/27", server.uri()),
             "name": "Email Notifications", "type": "webhook",
             "extension_source": "rossum_store", "install_action": "copy"}
        ]
    });
    // Mount the more-specific page-2 mock first so wiremock (which uses
    // first-match-wins semantics) routes `?page=2` requests here and lets
    // the catch-all page-1 mock handle everything else.
    Mock::given(method("GET"))
        .and(path("/api/v1/hook_templates"))
        .and(wiremock::matchers::query_param("page", "2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&page2))
        .mount(&server).await;
    Mock::given(method("GET"))
        .and(path("/api/v1/hook_templates"))
        .and(header("Authorization", "token TEST_TOKEN"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&page1))
        .mount(&server).await;

    let client = RossumClient::new(format!("{}/api/v1", server.uri()), "TEST_TOKEN".into()).unwrap();
    let templates: Vec<HookTemplate> = client.list_hook_templates(None).await.unwrap();
    assert_eq!(templates.len(), 2);
    assert!(templates.iter().any(|t| t.name == "Master Data Hub"));
}

#[tokio::test]
async fn list_users_paginates() {
    use rdc::model::User;
    use serde_json::json;
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/users"))
        .and(header("Authorization", "token TEST_TOKEN"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&json!({
            "pagination": { "next": null },
            "results": [
                {"id": 938493, "url": format!("{}/api/v1/users/938493", server.uri()),
                 "username": "system_user__a556534d", "is_active": true,
                 "groups": [format!("{}/api/v1/groups/3", server.uri())]},
                {"id": 200001, "url": format!("{}/api/v1/users/200001", server.uri()),
                 "username": "alice@example.org", "is_active": true,
                 "groups": [format!("{}/api/v1/groups/3", server.uri())]}
            ]
        })))
        .mount(&server).await;

    let client = RossumClient::new(format!("{}/api/v1", server.uri()), "TEST_TOKEN".into()).unwrap();
    let users: Vec<User> = client.list_users(None).await.unwrap();
    assert_eq!(users.len(), 2);
    assert!(users.iter().any(|u| u.is_system_user()));
}

#[tokio::test]
async fn login_posts_credentials_and_returns_key() {
    use wiremock::{matchers, Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(matchers::method("POST"))
        .and(matchers::path("/v1/auth/login"))
        .and(matchers::body_json(serde_json::json!({
            "username": "alice@example.com",
            "password": "hunter2",
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "key": "fresh-token-abc",
            "domain": "example",
        })))
        .expect(1)
        .mount(&server)
        .await;

    let api_base = format!("{}/v1", server.uri());
    let token = rdc::api::login(&api_base, "alice@example.com", "hunter2")
        .await
        .expect("login should succeed");
    assert_eq!(token, "fresh-token-abc");
}

#[tokio::test]
async fn login_propagates_401_on_bad_credentials() {
    use wiremock::{matchers, Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(matchers::method("POST"))
        .and(matchers::path("/v1/auth/login"))
        .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
            "detail": "Invalid username/password.",
        })))
        .expect(1)
        .mount(&server)
        .await;

    let api_base = format!("{}/v1", server.uri());
    let err = rdc::api::login(&api_base, "alice@example.com", "wrong")
        .await
        .expect_err("login should fail on 401");
    let msg = format!("{err:#}");
    assert!(msg.contains("401"), "error should mention status: {msg}");
}
