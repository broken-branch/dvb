mod support;
use dvb_core::api::GithubCheckDiscovery;
use dvb_core::forge::Forge;
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn get(server: &MockServer, route: &str, status: u16, body: serde_json::Value) {
    Mock::given(method("GET"))
        .and(path(route))
        .respond_with(ResponseTemplate::new(status).set_body_json(body))
        .mount(server)
        .await;
}

#[tokio::test]
async fn socket_discovery_reports_latest_results_and_confirmed_absence() {
    for runs in [
        json!([]),
        json!([
            {"id":2,"name":"rust","head_sha":"abc","status":"completed","conclusion":"failure"},
            {"id":3,"name":"rust","head_sha":"abc","status":"in_progress","conclusion":null},
            {"id":4,"name":"lint","head_sha":"abc","status":"completed","conclusion":"success"},
            {"id":5,"name":"skipped","head_sha":"abc","status":"completed","conclusion":"skipped"}
        ]),
    ] {
        let server = MockServer::start().await;
        let forge = support::gateway(&server).await;
        get(
            &server,
            "/repos/example-org/project",
            200,
            json!({"private":true,"default_branch":"trunk"}),
        )
        .await;
        get(
            &server,
            "/repos/example-org/project/commits/trunk",
            200,
            json!({"sha":"abc"}),
        )
        .await;
        get(
            &server,
            "/repos/example-org/project/commits/abc/check-runs",
            200,
            json!({"check_runs":runs}),
        )
        .await;
        let result = forge.discover_checks(&support::repo()).await;
        let GithubCheckDiscovery::Discovered {
            default_branch,
            head,
            checks,
        } = result
        else {
            panic!("{result:?}")
        };
        assert_eq!(default_branch, "trunk");
        assert_eq!(head, "abc");
        if runs.as_array().unwrap().is_empty() {
            assert!(checks.is_empty());
        } else {
            assert_eq!(checks.len(), 3);
            let skipped = checks.iter().find(|c| c.name == "skipped").unwrap();
            assert_eq!(skipped.status, dvb_core::event::ChecksStatus::Missing);
            let rust = checks.iter().find(|c| c.name == "rust").unwrap();
            assert_eq!(rust.status, dvb_core::event::ChecksStatus::Pending);
            assert_eq!(rust.conclusion, None);
        }
    }
}

#[tokio::test]
async fn socket_discovery_never_turns_access_or_response_failures_into_absence() {
    for status in [403, 404, 500] {
        let server = MockServer::start().await;
        let forge = support::gateway(&server).await;
        get(
            &server,
            "/repos/example-org/project",
            status,
            json!({"message":"secret-echo"}),
        )
        .await;
        let result = forge.discover_checks(&support::repo()).await;
        if status == 500 {
            assert!(matches!(result, GithubCheckDiscovery::Failed { .. }));
        } else {
            assert!(matches!(result, GithubCheckDiscovery::Inaccessible { .. }));
        }
        assert!(
            !serde_json::to_string(&result)
                .unwrap()
                .contains("secret-echo")
        );
    }
}

#[tokio::test]
async fn socket_repositories_are_installation_access_not_the_organization_catalogue() {
    let server = MockServer::start().await;
    let forge = support::gateway(&server).await;
    get(
        &server,
        "/installation/repositories",
        200,
        json!({"repositories":[
            {"full_name":"EXAMPLE-ORG/selected","private":true,"default_branch":"main"}
        ]}),
    )
    .await;
    let access = forge.accessible_repositories().await.unwrap();
    assert_eq!(access.organization, "example-org");
    assert_eq!(
        access.repository_access_url,
        "https://github.com/organizations/example-org/settings/installations/7"
    );
    assert_eq!(access.repositories.len(), 1);
    assert_eq!(access.repositories[0].repo, "EXAMPLE-ORG/selected");
}

#[tokio::test]
async fn socket_discovery_failures_after_metadata_are_not_empty_results() {
    for body in [
        json!({"message":"private-echo"}),
        json!({"check_runs":[{"id":1,"head_sha":"wrong","name":"rust","status":"completed","conclusion":"success"}]}),
    ] {
        let server = MockServer::start().await;
        let forge = support::gateway(&server).await;
        get(
            &server,
            "/repos/example-org/project",
            200,
            json!({"default_branch":"release/next"}),
        )
        .await;
        get(
            &server,
            "/repos/example-org/project/commits/release%2Fnext",
            200,
            json!({"sha":"abc"}),
        )
        .await;
        get(
            &server,
            "/repos/example-org/project/commits/abc/check-runs",
            200,
            body,
        )
        .await;
        let result = forge.discover_checks(&support::repo()).await;
        assert!(matches!(result, GithubCheckDiscovery::Failed { .. }));
        assert!(
            !serde_json::to_string(&result)
                .unwrap()
                .contains("private-echo")
        );
    }
}

#[tokio::test]
async fn socket_repository_access_reads_all_pages_and_propagates_page_errors() {
    use wiremock::matchers::query_param;
    for fail_second_page in [false, true] {
        let server = MockServer::start().await;
        let forge = support::gateway(&server).await;
        let first: Vec<_> = (0..100).map(|n| json!({"full_name":format!("example-org/repo-{n}"),"private":true,"default_branch":"main"})).collect();
        Mock::given(method("GET"))
            .and(path("/installation/repositories"))
            .and(query_param("page", "1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"repositories":first})))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET")).and(path("/installation/repositories")).and(query_param("page", "2"))
            .respond_with(if fail_second_page { ResponseTemplate::new(500).set_body_json(json!({"message":"secret-echo"})) }
                else { ResponseTemplate::new(200).set_body_json(json!({"repositories":[{"full_name":"example-org/last","private":false,"default_branch":"trunk"}]})) })
            .expect(1).mount(&server).await;
        let result = forge.accessible_repositories().await;
        if fail_second_page {
            let error = result.unwrap_err();
            assert!(error.retryable);
            assert!(!error.detail.contains("secret-echo"));
        } else {
            let repos = result.unwrap().repositories;
            assert_eq!(repos.len(), 101);
            assert_eq!(repos[0].repo, "example-org/last");
            assert!(!repos[0].private);
        }
    }
}
