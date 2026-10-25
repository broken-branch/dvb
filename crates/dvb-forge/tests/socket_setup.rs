//! Manifest conversion and App installation recordings; all traffic is loopback.
use dvb_core::api::GithubAccountType;
use dvb_forge::github_setup::{self as setup, PendingApp};
use serde_json::{Value, json};
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn pending() -> PendingApp {
    PendingApp {
        organization: "example-org".into(),
        state: "a".repeat(32),
        expires_at: 3_600_000,
        owner_name: "Example Owner".into(),
        owner_email: "owner@example.invalid".into(),
        app_id: None,
        slug: None,
        pem: None,
        account_type: GithubAccountType::Organization,
    }
}
fn personal() -> PendingApp {
    PendingApp {
        organization: "example-user".into(),
        account_type: GithubAccountType::User,
        ..pending()
    }
}
fn client(server: &MockServer) -> setup::Client {
    setup::client(
        &server.uri(),
        std::sync::Arc::new(dvb_forge::Wait::default()),
    )
    .unwrap()
}
fn test_key() -> String {
    // Ephemeral test key; no private key fixture or real account credential in source.
    let out = Command::new("openssl")
        .args([
            "genpkey",
            "-algorithm",
            "RSA",
            "-pkeyopt",
            "rsa_keygen_bits:2048",
        ])
        .stderr(Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success());
    String::from_utf8(out.stdout).unwrap()
}
fn installation() -> Value {
    json!({
        "id":7,
        "app_id":42,
        "account":{
            "login":"example-org",
            "type":"Organization"
        },
        "suspended_at":null,
        "permissions":{
            "contents":"write",
            "pull_requests":"write",
            "issues":"write",
            "workflows":"write",
            "checks":"read",
            "statuses":"read",
            "actions":"write",
            "metadata":"read",
            "administration":"write",
            "organization_projects":"write"
        }
    })
}
#[tokio::test]
async fn socket_manifest_conversion_persists_private_key_and_recovers_lost_local_write() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let key = test_key();
    let mut p = pending();
    Mock::given(method("POST"))
        .and(path("/app-manifests/recorded-code/conversions"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "id":42,
            "slug":"dvb-example-org",
            "owner":{
                "login":"example-org",
                "type":"Organization"
            },
            "pem":key,
            "client_secret":"discarded-recording",
            "webhook_secret":"discarded-recording"
        })))
        .expect(1)
        .mount(&server)
        .await;
    setup::exchange(
        &setup::client(
            &server.uri(),
            std::sync::Arc::new(dvb_forge::Wait::default()),
        )
        .unwrap(),
        dir.path(),
        &mut p,
        "recorded-code",
    )
    .await
    .unwrap();
    assert_eq!(p.app_id, Some(42));
    assert_eq!(
        std::fs::read_to_string(dir.path().join("github-app.pem")).unwrap(),
        key
    );
    assert_eq!(
        std::fs::metadata(dir.path().join("github-app.pem"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    let connection = serde_json::to_string(&setup::connection(dir.path())).unwrap();
    assert!(!connection.contains("PRIVATE KEY"));
    assert!(!connection.contains("discarded-recording"));
    assert!(!connection.contains("Example Owner"));
    std::fs::remove_file(dir.path().join("github-app.pem")).unwrap();
    let mut recovered = setup::pending(dir.path()).unwrap();
    setup::exchange(
        &setup::client(
            &server.uri(),
            std::sync::Arc::new(dvb_forge::Wait::default()),
        )
        .unwrap(),
        dir.path(),
        &mut recovered,
        "recorded-code",
    )
    .await
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.path().join("github-app.pem")).unwrap(),
        key
    );
    // Installation also restores the pending PEM before the shared loader runs.
    std::fs::remove_file(dir.path().join("github-app.pem")).unwrap();
    Mock::given(method("GET"))
        .and(path("/app/installations/7"))
        .respond_with(ResponseTemplate::new(200).set_body_json(installation()))
        .expect(1)
        .mount(&server)
        .await;
    let cfg = setup::verify_installation(
        &setup::client(
            &server.uri(),
            std::sync::Arc::new(dvb_forge::Wait::default()),
        )
        .unwrap(),
        &server.uri(),
        dir.path(),
        &recovered,
        7,
    )
    .await
    .unwrap();
    assert_eq!(cfg.organization, "example-org");
    assert_eq!(cfg.installation_id, 7);
    let requests = server.received_requests().await.unwrap();
    assert!(requests.iter().filter(|r| r.method == "GET").all(|r| {
        r.headers
            .get("authorization")
            .is_some_and(|v| v.to_str().unwrap().starts_with("Bearer "))
    }));
}
#[tokio::test]
async fn socket_installation_accepts_write_for_required_read_permissions() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let mut p = pending();
    p.app_id = Some(42);
    p.pem = Some(test_key());
    let mut granted = installation();
    for permission in ["checks", "statuses", "metadata"] {
        granted["permissions"][permission] = json!("write");
    }
    Mock::given(method("GET"))
        .and(path("/app/installations/7"))
        .respond_with(ResponseTemplate::new(200).set_body_json(granted))
        .expect(1)
        .mount(&server)
        .await;
    setup::verify_installation(
        &setup::client(
            &server.uri(),
            std::sync::Arc::new(dvb_forge::Wait::default()),
        )
        .unwrap(),
        &server.uri(),
        dir.path(),
        &p,
        7,
    )
    .await
    .unwrap();
}
#[tokio::test]
async fn socket_installation_requires_the_selected_organization_and_permissions() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let mut p = pending();
    p.app_id = Some(42);
    p.slug = Some("dvb-example-org".into());
    p.pem = Some(test_key());
    let mut bad = installation();
    bad["account"]["type"] = json!("User");
    let wrong_org = "installation does not belong to this App and account";
    for (field, value, expected) in [
        ("personal", bad, wrong_org),
        (
            "org",
            {
                let mut v = installation();
                v["account"]["login"] = json!("other-org");
                v
            },
            wrong_org,
        ),
        (
            "permission",
            {
                let mut v = installation();
                v["permissions"]["organization_projects"] = json!("read");
                v
            },
            "installation is missing required App permission organization_projects:write",
        ),
    ] {
        server.reset().await;
        Mock::given(method("GET"))
            .and(path("/app/installations/7"))
            .respond_with(ResponseTemplate::new(200).set_body_json(value))
            .expect(1)
            .mount(&server)
            .await;
        let error = setup::verify_installation(
            &setup::client(
                &server.uri(),
                std::sync::Arc::new(dvb_forge::Wait::default()),
            )
            .unwrap(),
            &server.uri(),
            dir.path(),
            &p,
            7,
        )
        .await
        .err()
        .unwrap();
        assert!(!error.detail.contains("PRIVATE KEY"), "{field}");
        assert!(!error.detail.contains(p.pem.as_ref().unwrap()), "{field}");
        assert_eq!(error.detail, expected, "{field}");
        assert!(!error.retryable, "{field}");
        server.verify().await;
    }
}
#[tokio::test]
async fn socket_installation_rejects_read_only_actions_permission() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let mut p = pending();
    p.app_id = Some(42);
    p.slug = Some("dvb-example-org".into());
    p.pem = Some(test_key());
    let mut read_only_actions = installation();
    read_only_actions["permissions"]["actions"] = json!("read");
    Mock::given(method("GET"))
        .and(path("/app/installations/7"))
        .respond_with(ResponseTemplate::new(200).set_body_json(read_only_actions))
        .expect(1)
        .mount(&server)
        .await;
    let error = setup::verify_installation(
        &setup::client(
            &server.uri(),
            std::sync::Arc::new(dvb_forge::Wait::default()),
        )
        .unwrap(),
        &server.uri(),
        dir.path(),
        &p,
        7,
    )
    .await
    .err()
    .expect("setup must reject an App with only actions:read");
    assert!(error.detail.contains("actions:write"), "{}", error.detail);
}
#[tokio::test]
async fn socket_conversion_failures_never_echo_remote_credentials_or_accept_wrong_owner() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let mut p = pending();
    Mock::given(method("POST"))
        .and(path("/app-manifests/code/conversions"))
        .respond_with(
            ResponseTemplate::new(422).set_body_json(json!({"message":"recorded-private-value"})),
        )
        .mount(&server)
        .await;
    let error = setup::exchange(
        &setup::client(
            &server.uri(),
            std::sync::Arc::new(dvb_forge::Wait::default()),
        )
        .unwrap(),
        dir.path(),
        &mut p,
        "code",
    )
    .await
    .unwrap_err();
    assert!(!error.detail.contains("recorded-private-value"));
    assert!(!dir.path().join("github-app.pem").exists());
    server.reset().await;
    Mock::given(method("POST"))
        .and(path("/app-manifests/code/conversions"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "id":42,
            "slug":"dvb-example-org",
            "owner":{
                "login":"other-org",
                "type":"Organization"
            },
            "pem":test_key()
        })))
        .mount(&server)
        .await;
    assert_eq!(
        setup::exchange(
            &setup::client(
                &server.uri(),
                std::sync::Arc::new(dvb_forge::Wait::default())
            )
            .unwrap(),
            dir.path(),
            &mut p,
            "code"
        )
        .await
        .unwrap_err()
        .detail,
        "converted App does not match the private setup for this account"
    );
    assert!(!dir.path().join("github-app.pem").exists());
    assert_eq!(
        setup::exchange(
            &setup::client(
                &server.uri(),
                std::sync::Arc::new(dvb_forge::Wait::default())
            )
            .unwrap(),
            dir.path(),
            &mut p,
            "../escape"
        )
        .await
        .unwrap_err()
        .detail,
        "invalid manifest code"
    );
}

async fn setup_client_timeout(authenticated: bool) {
    use std::time::Duration;
    use tokio::io::AsyncReadExt;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let dir = tempfile::tempdir().unwrap();
    let mut p = pending();
    p.app_id = Some(42);
    p.pem = Some(test_key());
    let client = setup::client(&url, std::sync::Arc::new(dvb_forge::Wait::default())).unwrap();
    let request = tokio::spawn(async move {
        if authenticated {
            setup::verify_installation(&client, &url, dir.path(), &p, 7)
                .await
                .map(|_| ())
        } else {
            p.app_id = None;
            p.pem = None;
            setup::exchange(&client, dir.path(), &mut p, "silent").await
        }
    });
    let (mut stream, _) = listener.accept().await.unwrap();
    let mut bytes = [0; 4096];
    assert!(stream.read(&mut bytes).await.unwrap() > 0);
    tokio::time::advance(Duration::from_secs(31)).await;
    // The peer stays open and sends no bytes; only the client's timeout
    // can complete the request. A second deadline bounds a broken test.
    let result = tokio::time::timeout(Duration::from_secs(1), request).await;
    assert!(
        result.is_ok(),
        "setup client has no read timeout (authenticated={authenticated})"
    );
    assert!(result.unwrap().unwrap().is_err());
}

#[tokio::test(start_paused = true)]
async fn socket_manifest_client_times_out_on_a_silent_github() {
    setup_client_timeout(false).await;
}

#[tokio::test(start_paused = true)]
async fn socket_installation_client_times_out_on_a_silent_github() {
    setup_client_timeout(true).await;
}

#[tokio::test]
async fn socket_the_account_type_comes_from_a_public_read_of_the_account() {
    let server = MockServer::start().await;
    for (login, kind) in [
        ("example-user", "User"),
        ("example-org", "Organization"),
        ("example-bot", "Bot"),
    ] {
        Mock::given(method("GET"))
            .and(path(format!("/users/{login}")))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"login":login,"type":kind})),
            )
            .mount(&server)
            .await;
    }
    Mock::given(method("GET"))
        .and(path("/users/nobody-here"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({"message":"Not Found"})))
        .mount(&server)
        .await;
    let api = client(&server);
    for (login, expected) in [
        ("example-user", Some(GithubAccountType::User)),
        ("example-org", Some(GithubAccountType::Organization)),
        ("example-bot", None),
        ("nobody-here", None),
        ("personal/name", None),
    ] {
        assert_eq!(
            setup::account_type(&api, login).await.unwrap(),
            expected,
            "{login}"
        );
    }
    let requests = server.received_requests().await.unwrap();
    // The read needs no token, and an invalid name is never sent.
    assert_eq!(requests.len(), 4);
    assert!(
        requests
            .iter()
            .all(|r| r.headers.get("authorization").is_none())
    );
    server.reset().await;
    Mock::given(method("GET"))
        .and(path("/users/example-user"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    assert!(
        setup::account_type(&api, "example-user")
            .await
            .unwrap_err()
            .retryable
    );
}

#[tokio::test]
async fn socket_a_personal_accounts_app_converts_and_installs_without_the_board_permission() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let mut p = personal();
    Mock::given(method("POST"))
        .and(path("/app-manifests/recorded-code/conversions"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "id":42,
            "slug":"dvb-example-user",
            "owner":{"login":"Example-User","type":"User"},
            "public":false,
            "pem":test_key()
        })))
        .expect(1)
        .mount(&server)
        .await;
    let mut user = installation();
    user["account"] = json!({"login":"example-user","type":"User"});
    // GitHub grants no organization permission on a personal account.
    user["permissions"]
        .as_object_mut()
        .unwrap()
        .remove("organization_projects");
    Mock::given(method("GET"))
        .and(path("/app/installations/7"))
        .respond_with(ResponseTemplate::new(200).set_body_json(user))
        .expect(1)
        .mount(&server)
        .await;
    setup::exchange(&client(&server), dir.path(), &mut p, "recorded-code")
        .await
        .unwrap();
    assert_eq!(p.app_id, Some(42));
    let connection = setup::connection(dir.path());
    assert_eq!(connection.account_type, Some(GithubAccountType::User));
    assert_eq!(connection.organization.as_deref(), Some("example-user"));
    let cfg = setup::verify_installation(&client(&server), &server.uri(), dir.path(), &p, 7)
        .await
        .unwrap();
    assert_eq!(cfg.organization, "example-user");
    assert_eq!(cfg.account_type, GithubAccountType::User);
    setup::write_private(
        dir.path(),
        "github-app.json",
        &serde_json::to_vec(&cfg).unwrap(),
    )
    .unwrap();
    let connection = setup::connection(dir.path());
    assert!(connection.connected);
    assert_eq!(connection.account_type, Some(GithubAccountType::User));
    assert_eq!(
        connection.repository_access_url.as_deref(),
        Some("https://github.com/settings/installations/7")
    );
}

#[tokio::test]
async fn socket_conversion_and_installation_must_match_the_chosen_account_type() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    // A user's App converted under the organization of the same name, and the reverse.
    for (mut p, owner_type) in [(personal(), "Organization"), (pending(), "User")] {
        server.reset().await;
        Mock::given(method("POST"))
            .and(path("/app-manifests/code/conversions"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({
                "id":42,
                "slug":"dvb-example",
                "owner":{"login":p.organization,"type":owner_type},
                "pem":test_key()
            })))
            .mount(&server)
            .await;
        assert_eq!(
            setup::exchange(&client(&server), dir.path(), &mut p, "code")
                .await
                .unwrap_err()
                .detail,
            "converted App does not match the private setup for this account"
        );
        assert!(!dir.path().join("github-app.pem").exists());
        p.app_id = Some(42);
        p.pem = Some(test_key());
        let mut wrong = installation();
        wrong["account"] = json!({"login":p.organization,"type":owner_type});
        Mock::given(method("GET"))
            .and(path("/app/installations/7"))
            .respond_with(ResponseTemplate::new(200).set_body_json(wrong))
            .mount(&server)
            .await;
        assert_eq!(
            setup::verify_installation(&client(&server), &server.uri(), dir.path(), &p, 7)
                .await
                .err()
                .unwrap()
                .detail,
            "installation does not belong to this App and account"
        );
        std::fs::remove_file(dir.path().join("github-app.pem")).unwrap();
    }
}
