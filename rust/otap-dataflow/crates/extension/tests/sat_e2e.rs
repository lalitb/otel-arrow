use kube::config::Config;
use kube::Client;
use otap_df_extension::auth::ServerAuth;
use otap_df_extension::impls::experimental::sat_auth::{
    ExtensionAuthConfig, SatConfig, SatExtension,
};
use std::collections::HashMap;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn test_sat_extension_e2e() {
    // 1. Start Mock Server
    let mock_server = MockServer::start().await;

    // 2. Mock TokenReview response
    let response_body = serde_json::json!({
        "apiVersion": "authentication.k8s.io/v1",
        "kind": "TokenReview",
        "status": {
            "authenticated": true,
            "user": {
                "username": "system:serviceaccount:default:my-sa",
                "groups": ["system:serviceaccounts"]
            },
            "audiences": ["arc-diagnostics:my-resource"]
        }
    });

    Mock::given(method("POST"))
        .and(path("/apis/authentication.k8s.io/v1/tokenreviews"))
        .respond_with(ResponseTemplate::new(200).set_body_json(response_body))
        .mount(&mock_server)
        .await;

    // 3. Create Kube Client pointing to Mock Server
    let mut config = Config::new(mock_server.uri().parse().unwrap());
    config.accept_invalid_certs = true;
    
    let client = Client::try_from(config).unwrap();

    // 4. Configure Extension
    let sat_config = SatConfig {
        allow_no_auth: false,
        extension_auth_configs: vec![ExtensionAuthConfig {
            extension_rid: "my-resource".into(),
            extension_type: "test".into(),
            service_account_namespace: "default".into(),
            service_account_names: vec!["my-sa".into()],
        }],
    };

    // 5. Create Extension with Client
    let ext = SatExtension::new_with_client(sat_config, client);

    // 6. Authenticate
    let mut headers = HashMap::new();
    headers.insert(
        "Authorization".to_string(),
        vec!["Bearer my-token".to_string()],
    );

    let result = ext.authenticate(&headers).await;
    assert!(result.is_ok(), "Authentication failed: {:?}", result.err());
    let auth_info = result.unwrap();
    assert_eq!(
        auth_info.principal,
        Some("system:serviceaccount:default:my-sa".to_string())
    );
}
