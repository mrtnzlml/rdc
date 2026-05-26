use rossum_local::commands::AddConnectionInput;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn add_connection_rejects_invalid_token_with_401() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/organizations/1"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;

    let input = AddConnectionInput {
        name: "X".into(),
        api_base: server.uri(),
        org_id: 1,
        auth_kind: "token".into(),
        token: Some("bad".into()),
        username: None,
        password: None,
        folder: None,
    };

    let err = rossum_local::commands::validate_add_input_against_rossum(&input)
        .await
        .unwrap_err();
    assert!(err.to_lowercase().contains("sign-in"));
}

#[tokio::test]
async fn add_connection_accepts_valid_token() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": 1, "name": "t", "url": format!("{}/organizations/1", server.uri())
        })))
        .mount(&server)
        .await;

    let input = AddConnectionInput {
        name: "X".into(),
        api_base: server.uri(),
        org_id: 1,
        auth_kind: "token".into(),
        token: Some("good".into()),
        username: None,
        password: None,
        folder: None,
    };

    let token = rossum_local::commands::validate_add_input_against_rossum(&input)
        .await
        .unwrap();
    assert_eq!(token, "good");
}
