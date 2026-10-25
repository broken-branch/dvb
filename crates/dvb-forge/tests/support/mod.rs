use dvb_core::api::GithubAccountType;
use dvb_core::forge::Repo;
use dvb_forge::GitHub;
use octocrab::Octocrab;
use serde_json::json;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

pub async fn gateway(server: &MockServer) -> GitHub {
    gateway_for(server, "example-org", GithubAccountType::Organization).await
}
/// An App installed on `owner`, an organization or a personal account.
pub async fn gateway_for(server: &MockServer, owner: &str, account: GithubAccountType) -> GitHub {
    let app = Octocrab::builder()
        .set_connect_timeout(Some(std::time::Duration::from_secs(10)))
        .set_read_timeout(Some(std::time::Duration::from_secs(30)))
        .set_write_timeout(Some(std::time::Duration::from_secs(30)))
        .add_retry_config(octocrab::service::middleware::retry::RetryConfig::None)
        .base_uri(server.uri())
        .unwrap()
        .build()
        .unwrap()
        .user_access_token("recorded-app-jwt")
        .unwrap();
    Mock::given(method("POST"))
        .and(path("/app/installations/7/access_tokens"))
        .and(header("authorization", "Bearer recorded-app-jwt"))
        .respond_with(ResponseTemplate::new(201).set_body_json(
            json!({"token":"recorded-installation-token","expires_at":"2030-01-01T00:00:00Z"}),
        ))
        .mount(server)
        .await;
    GitHub::with_app_client(
        app,
        7,
        owner.into(),
        account,
        std::sync::Arc::new(dvb_forge::Wait::default()),
    )
    .unwrap()
}
pub fn repo() -> Repo {
    Repo("example-org/project".into())
}
