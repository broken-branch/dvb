//! Recorded REST responses, only against wiremock on loopback. Test names carry
//! socket_ so sandbox validation skips execution while compiling every test.
mod support;
use support::{gateway, repo};

use dvb_core::event::ChecksStatus;
use dvb_core::forge::{CheckObservation, Forge, MergeError, PrState};
use serde_json::{Value, json};
use wiremock::matchers::{body_json, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn response(server: &MockServer, verb: &str, route: &str, status: u16, body: Value) {
    Mock::given(method(verb))
        .and(path(route))
        .respond_with(ResponseTemplate::new(status).set_body_json(body))
        .expect(1)
        .mount(server)
        .await;
}
async fn metadata(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/repos/example-org/project"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"private":true,"default_branch":"trunk"})),
        )
        .mount(server)
        .await;
}
fn pr(head: &str, merged: bool) -> Value {
    json!({
        "number":23,
        "html_url":"https://github.invalid/example-org/project/pull/23",
        "state":if merged {
            "closed"
        } else {
            "open"
        },
        "merged":merged,
        "merged_at":if merged {
            Some("2026-01-01T00:00:00Z")
        } else {
            None
        },
        "merge_commit_sha":if merged {
            Some("merge-commit")
        } else {
            None
        },
        "head":{
            "ref":"dvb/t-1",
            "sha":head
        },
        "base":{
            "ref":"trunk"
        }
    })
}

#[tokio::test]
async fn socket_fetch_includes_tags_outside_branches_and_updates_moved_tags() {
    use dvb_core::git::Git;
    use std::path::Path;

    async fn git(dir: &Path, args: &[&str]) -> String {
        Git::new(dir)
            .run(
                args,
                &[
                    ("GIT_AUTHOR_NAME", "fixture"),
                    ("GIT_AUTHOR_EMAIL", "fixture@example.invalid"),
                    ("GIT_COMMITTER_NAME", "fixture"),
                    ("GIT_COMMITTER_EMAIL", "fixture@example.invalid"),
                ],
            )
            .await
            .unwrap()
    }

    let server = MockServer::start().await;
    let forge = gateway(&server).await;
    let tmp = tempfile::tempdir().unwrap();
    let origin = tmp.path().join("origin");
    let mirror = tmp.path().join("mirror.git");
    git(
        tmp.path(),
        &["init", "--quiet", "-b", "main", origin.to_str().unwrap()],
    )
    .await;
    git(
        &origin,
        &["commit", "--quiet", "--allow-empty", "-m", "base"],
    )
    .await;
    let base = git(&origin, &["rev-parse", "HEAD"]).await;
    git(
        tmp.path(),
        &[
            "clone",
            "--quiet",
            "--bare",
            origin.to_str().unwrap(),
            mirror.to_str().unwrap(),
        ],
    )
    .await;
    // Redirect this mirror's GitHub transport to local Git; HTTP is recorded above.
    git(
        &mirror,
        &[
            "config",
            &format!("url.{}.insteadOf", origin.display()),
            "https://github.com/example-org/project.git",
        ],
    )
    .await;
    let tagged = git(&origin, &["commit-tree", "HEAD^{tree}", "-m", "tag only"]).await;
    git(&origin, &["tag", "-a", "v1", &tagged, "-m", "release"]).await;

    assert_eq!(forge.fetch(&repo(), &mirror, "main").await.unwrap(), base);
    assert_eq!(git(&mirror, &["rev-parse", "v1^{commit}"]).await, tagged);
    git(
        &origin,
        &["tag", "-f", "-a", "v1", &base, "-m", "moved release"],
    )
    .await;
    assert_eq!(forge.fetch(&repo(), &mirror, "main").await.unwrap(), base);
    assert_eq!(git(&mirror, &["rev-parse", "v1^{commit}"]).await, base);
    let requests = server.received_requests().await.unwrap();
    let token = requests
        .iter()
        .find(|r| r.url.path().ends_with("/access_tokens"))
        .unwrap();
    assert_eq!(
        token.body_json::<Value>().unwrap(),
        json!({"repositories":["project"]})
    );
}

#[tokio::test]
async fn socket_checks_read_both_apis_and_use_the_latest_status() {
    let server = MockServer::start().await;
    let forge = gateway(&server).await;
    Mock::given(method("GET"))
        .and(path("/repos/example-org/project/commits/h1/check-runs"))
        .and(query_param("filter", "latest"))
        .and(query_param("per_page", "100"))
        .and(query_param("page", "1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "total_count":4,
            "check_runs":[
                {
                    "name":"rust",
                    "head_sha":"h1",
                    "status":"completed",
                    "conclusion":"success"
                },
                {
                    "id":17,
                    "name":"skipped",
                    "head_sha":"h1",
                    "status":"completed",
                    "conclusion":"skipped",
                    "html_url":"https://ci.invalid/run/17"
                },
                {
                    "name":"pending",
                    "head_sha":"h1",
                    "status":"in_progress",
                    "conclusion":null
                },
                {
                    "name":"other-head",
                    "head_sha":"old",
                    "status":"completed",
                    "conclusion":"success"
                }
            ]
        })))
        .expect(1)
        .mount(&server)
        .await;
    response(
        &server,
        "GET",
        "/repos/example-org/project/commits/h1/statuses",
        200,
        json!([
            {"context":"legacy","state":"pending"},{"context":"legacy","state":"success"},
            {"context":"deploy","state":"success"},{"context":"unknown","state":"new-state"}
        ]),
    )
    .await;
    response(
        &server,
        "GET",
        "/repos/example-org/project/actions/runs",
        200,
        json!({"workflow_runs":[]}),
    )
    .await;
    let seen = forge.checks(&repo(), "h1").await.unwrap();
    assert_eq!(
        seen.iter().find(|c| c.name == "rust").unwrap().status,
        ChecksStatus::Success
    );
    assert_eq!(
        seen.iter().find(|c| c.name == "legacy").unwrap().status,
        ChecksStatus::Pending
    );
    assert_eq!(seen.iter().filter(|c| c.name == "legacy").count(), 1);
    assert_eq!(
        seen.iter().find(|c| c.name == "skipped").unwrap().status,
        ChecksStatus::Missing
    );
    assert_eq!(
        seen.iter().find(|c| c.name == "other-head").unwrap().status,
        ChecksStatus::Unreadable
    );
    let skipped = seen.iter().find(|c| c.name == "skipped").unwrap();
    assert_eq!(skipped.run, None);
    assert_eq!(skipped.conclusion.as_deref(), Some("skipped"));
    assert_eq!(skipped.url.as_deref(), Some("https://ci.invalid/run/17"));
    let requests = server.received_requests().await.unwrap();
    assert!(
        requests
            .iter()
            .filter(|request| request.method == "GET")
            .all(|request| {
                let auth: Vec<_> = request.headers.get_all("authorization").iter().collect();
                auth.len() == 1 && auth[0] == "Bearer recorded-installation-token"
            })
    );
    let token = requests
        .iter()
        .find(|r| r.url.path().ends_with("access_tokens"))
        .unwrap()
        .body_json::<Value>()
        .unwrap();
    assert_eq!(token, json!({"repositories":["project"]}));
}

#[tokio::test]
async fn socket_pr_checks_prefer_pull_request_suites_on_the_exact_head() {
    // A workflow with a push run and a PR run on the head reads the PR run.
    for event in [
        "workflow_dispatch",
        "schedule",
        "push",
        "pull_request_target",
    ] {
        for (conclusion, expected) in [
            ("success", ChecksStatus::Success),
            ("skipped", ChecksStatus::Missing),
            ("failure", ChecksStatus::Failed),
        ] {
            let server = MockServer::start().await;
            let forge = gateway(&server).await;
            let pr_check = json!({
                "id":17,"name":"identity","head_sha":"h1","status":"completed",
                "conclusion":conclusion,"app":{"slug":"github-actions"},"check_suite":{"id":7}
            });
            let other_conclusion = match event {
                "schedule" => "failure",
                "push" => "success",
                _ => "skipped",
            };
            let mut commit_checks = vec![
                json!({
                    "id":18,"name":"identity","head_sha":"h1","status":"completed",
                    "conclusion":other_conclusion,"app":{"slug":"github-actions"},"check_suite":{"id":8}
                }),
                json!({
                    "id":19,"name":"external","head_sha":"h1","status":"completed",
                    "conclusion":"success","app":{"slug":"external-ci"}
                }),
            ];
            // Also cover latest responses in which the other event hides the PR check.
            if event == "workflow_dispatch" {
                commit_checks.push(pr_check.clone());
            }
            Mock::given(method("GET"))
                .and(path("/repos/example-org/project/commits/h1/check-runs"))
                .and(query_param("filter", "latest"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "check_runs":commit_checks
                })))
                .expect(1)
                .mount(&server)
                .await;
            Mock::given(method("GET"))
                .and(path("/repos/example-org/project/actions/runs"))
                .and(query_param("head_sha", "h1"))
                .respond_with(
                    ResponseTemplate::new(200).set_body_json(json!({"workflow_runs":[
                        {"id":80,"workflow_id":1,"event":event,"head_sha":"h1","check_suite_id":8},
                        {"id":70,"workflow_id":1,"event":"pull_request","head_sha":"h1","check_suite_id":7},
                        {"id":60,"workflow_id":1,"event":"pull_request","head_sha":"old","check_suite_id":6}
                    ]})),
                )
                .expect(1)
                .mount(&server)
                .await;
            Mock::given(method("GET"))
                .and(path("/repos/example-org/project/check-suites/7/check-runs"))
                .and(query_param("filter", "latest"))
                .respond_with(
                    ResponseTemplate::new(200).set_body_json(json!({"check_runs":[pr_check]})),
                )
                .expect(1)
                .mount(&server)
                .await;
            response(
                &server,
                "GET",
                "/repos/example-org/project/commits/h1/statuses",
                200,
                json!([{"context":"legacy","state":"success"}]),
            )
            .await;
            let seen = forge.checks(&repo(), "h1").await.unwrap();
            assert_eq!(seen.len(), 3, "{event}: {conclusion}");
            let identity = seen.iter().find(|c| c.name == "identity").unwrap();
            assert_eq!(identity.status, expected, "{event}: {conclusion}");
            assert_eq!(identity.conclusion.as_deref(), Some(conclusion));
            assert_eq!(
                identity.run,
                (expected == ChecksStatus::Failed).then_some(17)
            );
            assert!(
                seen.iter()
                    .filter(|c| c.name != "identity")
                    .all(|c| c.status == ChecksStatus::Success)
            );
        }
    }
}

#[tokio::test]
async fn socket_pr_checks_use_the_newest_pr_run_per_workflow() {
    for (conclusion, expected) in [
        ("success", ChecksStatus::Success),
        ("skipped", ChecksStatus::Missing),
        ("failure", ChecksStatus::Failed),
    ] {
        let server = MockServer::start().await;
        let forge = gateway(&server).await;
        response(
            &server,
            "GET",
            "/repos/example-org/project/commits/h1/check-runs",
            200,
            json!({"check_runs":[]}),
        )
        .await;
        Mock::given(method("GET"))
            .and(path("/repos/example-org/project/actions/runs"))
            .and(query_param("head_sha", "h1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"workflow_runs":[
                {"id":90,"workflow_id":1,"event":"workflow_dispatch","head_sha":"h1","check_suite_id":9},
                {"id":80,"workflow_id":1,"event":"pull_request","head_sha":"h1","check_suite_id":8},
                {"id":70,"workflow_id":2,"event":"pull_request","head_sha":"h1","check_suite_id":7},
                {"id":60,"workflow_id":1,"event":"pull_request","head_sha":"h1","check_suite_id":6}
            ]})))
            .expect(1)
            .mount(&server)
            .await;
        for (suite, name, result) in [
            (8, "rust", conclusion),
            (7, "ui", "success"),
            (6, "rust", "failure"),
        ] {
            Mock::given(method("GET"))
                .and(path(format!(
                    "/repos/example-org/project/check-suites/{suite}/check-runs"
                )))
                .and(query_param("filter", "latest"))
                .respond_with(
                    ResponseTemplate::new(200).set_body_json(json!({"check_runs":[{
                        "id":suite,"name":name,"head_sha":"h1","status":"completed",
                        "conclusion":result,"app":{"slug":"github-actions"}
                    }]})),
                )
                .mount(&server)
                .await;
        }
        response(
            &server,
            "GET",
            "/repos/example-org/project/commits/h1/statuses",
            200,
            json!([]),
        )
        .await;

        let seen = forge.checks(&repo(), "h1").await.unwrap();
        assert_eq!(seen.len(), 2, "{conclusion}: {seen:?}");
        let rust = seen.iter().find(|c| c.name == "rust").unwrap();
        assert_eq!(rust.status, expected);
        assert_eq!(rust.conclusion.as_deref(), Some(conclusion));
        assert_eq!(rust.run, (expected == ChecksStatus::Failed).then_some(8));
        assert_eq!(
            seen.iter().find(|c| c.name == "ui").unwrap().status,
            ChecksStatus::Success
        );
        assert!(server.received_requests().await.unwrap().iter().all(|r| {
            !r.url.path().contains("/check-suites/6/") && !r.url.path().contains("/check-suites/9/")
        }));
    }
}

/// Check suites answering one check run each: (suite, name, conclusion).
async fn suites(server: &MockServer, runs: &[(u64, &str, &str)]) {
    for &(suite, name, result) in runs {
        Mock::given(method("GET"))
            .and(path(format!(
                "/repos/example-org/project/check-suites/{suite}/check-runs"
            )))
            .and(query_param("filter", "latest"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"check_runs":[{
                    "id":suite,"name":name,"head_sha":"h1","status":"completed",
                    "conclusion":result,"app":{"slug":"github-actions"}
                }]})),
            )
            .mount(server)
            .await;
    }
}

#[tokio::test]
async fn socket_pr_checks_use_the_newest_push_run_of_a_push_only_workflow() {
    for (conclusion, expected) in [
        ("success", ChecksStatus::Success),
        ("skipped", ChecksStatus::Missing),
        ("failure", ChecksStatus::Failed),
    ] {
        let server = MockServer::start().await;
        let forge = gateway(&server).await;
        response(
            &server,
            "GET",
            "/repos/example-org/project/commits/h1/check-runs",
            200,
            json!({"check_runs":[]}),
        )
        .await;
        Mock::given(method("GET"))
            .and(path("/repos/example-org/project/actions/runs"))
            .and(query_param("head_sha", "h1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"workflow_runs":[
                {"id":90,"workflow_id":1,"event":"workflow_dispatch","head_sha":"h1","check_suite_id":9},
                {"id":85,"workflow_id":1,"event":"push","head_sha":"old","check_suite_id":5},
                {"id":80,"workflow_id":1,"event":"push","head_sha":"h1","check_suite_id":8},
                {"id":70,"workflow_id":2,"event":"schedule","head_sha":"h1","check_suite_id":7},
                {"id":60,"workflow_id":1,"event":"push","head_sha":"h1","check_suite_id":6}
            ]})))
            .expect(1)
            .mount(&server)
            .await;
        suites(
            &server,
            &[
                (8, "rust", conclusion),
                (9, "rust", "success"),
                (7, "nightly", "success"),
                (6, "rust", "failure"),
                (5, "rust", "success"),
            ],
        )
        .await;
        response(
            &server,
            "GET",
            "/repos/example-org/project/commits/h1/statuses",
            200,
            json!([]),
        )
        .await;

        let seen = forge.checks(&repo(), "h1").await.unwrap();
        assert_eq!(seen.len(), 1, "{conclusion}: {seen:?}");
        assert_eq!(seen[0].name, "rust");
        assert_eq!(seen[0].status, expected);
        assert_eq!(seen[0].conclusion.as_deref(), Some(conclusion));
        assert_eq!(seen[0].run, (expected == ChecksStatus::Failed).then_some(8));
        assert!(server.received_requests().await.unwrap().iter().all(|r| {
            ![
                "/check-suites/5/",
                "/check-suites/6/",
                "/check-suites/7/",
                "/check-suites/9/",
            ]
            .iter()
            .any(|suite| r.url.path().contains(suite))
        }));
    }
}

#[tokio::test]
async fn socket_pr_checks_ignore_a_skipped_manual_run_of_another_workflow() {
    let server = MockServer::start().await;
    let forge = gateway(&server).await;
    response(
        &server,
        "GET",
        "/repos/example-org/project/commits/h1/check-runs",
        200,
        json!({"check_runs":[{
            "id":20,"name":"release","head_sha":"h1","status":"completed",
            "conclusion":"skipped","app":{"slug":"github-actions"},"check_suite":{"id":9}
        }]}),
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/repos/example-org/project/actions/runs"))
        .and(query_param("head_sha", "h1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"workflow_runs":[
            {"id":90,"workflow_id":2,"event":"workflow_dispatch","head_sha":"h1","check_suite_id":9},
            {"id":80,"workflow_id":1,"event":"pull_request","head_sha":"h1","check_suite_id":8}
        ]})))
        .expect(1)
        .mount(&server)
        .await;
    suites(
        &server,
        &[(8, "rust", "success"), (9, "release", "skipped")],
    )
    .await;
    response(
        &server,
        "GET",
        "/repos/example-org/project/commits/h1/statuses",
        200,
        json!([]),
    )
    .await;

    let seen = forge.checks(&repo(), "h1").await.unwrap();
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert_eq!(seen[0].name, "rust");
    assert_eq!(seen[0].status, ChecksStatus::Success);
    assert!(
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .all(|r| !r.url.path().contains("/check-suites/9/"))
    );
}

#[tokio::test]
async fn socket_edit_pr_body_patches_with_the_repository_token() {
    let server = MockServer::start().await;
    let forge = gateway(&server).await;
    Mock::given(method("PATCH"))
        .and(path("/repos/example-org/project/pulls/23"))
        .and(body_json(
            json!({"body": "Changed and checked.\n\nTask t, round 2, built by run b2."}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .expect(1)
        .mount(&server)
        .await;
    forge
        .edit_pr_body(
            &repo(),
            23,
            "Changed and checked.\n\nTask t, round 2, built by run b2.",
        )
        .await
        .unwrap();
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[0].body_json::<Value>().unwrap(),
        json!({"repositories": ["project"]})
    );
}

#[tokio::test]
async fn socket_pagination_does_not_drop_an_expected_check() {
    let server = MockServer::start().await;
    let forge = gateway(&server).await;
    let runs: Vec<_> = (0..100)
        .map(|i| {
            json!({
                "name":format!("check-{i}"),
                "head_sha":"h1",
                "status":"completed",
                "conclusion":"success"
            })
        })
        .collect();
    Mock::given(method("GET"))
        .and(path("/repos/example-org/project/commits/h1/check-runs"))
        .and(query_param("page", "1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"check_runs":runs})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/example-org/project/commits/h1/check-runs"))
        .and(query_param("page", "2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "check_runs":[
                {
                    "name":"last",
                    "head_sha":"h1",
                    "status":"completed",
                    "conclusion":"failure"
                }
            ]
        })))
        .expect(1)
        .mount(&server)
        .await;
    response(
        &server,
        "GET",
        "/repos/example-org/project/commits/h1/statuses",
        200,
        json!([]),
    )
    .await;
    response(
        &server,
        "GET",
        "/repos/example-org/project/actions/runs",
        200,
        json!({"workflow_runs":[]}),
    )
    .await;
    let observed = forge.checks(&repo(), "h1").await.unwrap();
    assert_eq!(observed.len(), 101);
    assert_eq!(observed.last().unwrap().status, ChecksStatus::Failed);
}

#[tokio::test]
async fn socket_403_names_rate_limit_or_missing_permission_without_the_body() {
    for (message, detail) in [
        (
            "You have exceeded a secondary rate limit. secret-xyz",
            "GitHub HTTP 403 (rate limit)",
        ),
        (
            "Resource not accessible by integration secret-xyz",
            "GitHub HTTP 403 (App permission missing)",
        ),
        ("something else secret-xyz", "GitHub HTTP 403"),
    ] {
        let server = MockServer::start().await;
        let forge = gateway(&server).await;
        response(
            &server,
            "GET",
            "/repos/example-org/project/commits/h1/check-runs",
            403,
            json!({"message":message}),
        )
        .await;
        let error = forge.checks(&repo(), "h1").await.unwrap_err();
        assert_eq!(error.detail, detail);
        assert!(!error.detail.contains("secret"));
        assert!(error.retryable);
    }
}

#[tokio::test]
async fn socket_open_pr_uses_the_repository_default() {
    let server = MockServer::start().await;
    let forge = gateway(&server).await;
    metadata(&server).await;
    Mock::given(method("POST"))
        .and(path("/repos/example-org/project/pulls"))
        .and(body_json(
            json!({"head":"dvb/t-1","base":"trunk","title":"Change","body":"Reviewed"}),
        ))
        .respond_with(ResponseTemplate::new(201).set_body_json(pr("h1", false)))
        .expect(1)
        .mount(&server)
        .await;
    assert_eq!(
        forge
            .open_pr(&repo(), "dvb/t-1", "Change", "Reviewed")
            .await
            .unwrap()
            .number,
        23
    );
    let requests = server.received_requests().await.unwrap();
    let tokens: Vec<Value> = requests
        .iter()
        .filter(|r| r.url.path().ends_with("access_tokens"))
        .map(|r| r.body_json::<Value>().unwrap())
        .collect();
    assert_eq!(tokens, vec![json!({"repositories":["project"]})]);
}

#[tokio::test]
async fn socket_merge_is_sha_pinned_and_a_moved_head_is_a_distinct_refusal() {
    let server = MockServer::start().await;
    let forge = gateway(&server).await;
    Mock::given(method("PUT"))
        .and(path("/repos/example-org/project/pulls/23/merge"))
        .and(body_json(json!({
            "sha":"h1",
            "merge_method":"squash",
            "commit_title":"Change (#23)",
            "commit_message":"Reviewed publication h1."
        })))
        .respond_with(ResponseTemplate::new(409).set_body_json(json!({
            "message":"Head branch was modified"
        })))
        .expect(1)
        .mount(&server)
        .await;
    assert_eq!(
        forge.merge(&repo(), 23, "h1", "Change").await,
        Err(MergeError::HeadMoved)
    );
}

#[tokio::test]
async fn socket_merge_titles_the_squash_with_the_pr_title() {
    let server = MockServer::start().await;
    let forge = gateway(&server).await;
    Mock::given(method("PUT"))
        .and(path("/repos/example-org/project/pulls/23/merge"))
        .and(body_json(json!({
            "sha":"h1",
            "merge_method":"squash",
            "commit_title":"Title squash merges with the PR title (#23)",
            "commit_message":"Reviewed publication h1."
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "merged":true,"sha":"merge-commit"
        })))
        .expect(1)
        .mount(&server)
        .await;
    assert_eq!(
        forge
            .merge(&repo(), 23, "h1", "Title squash merges with the PR title")
            .await
            .unwrap(),
        "merge-commit"
    );
}

#[tokio::test]
async fn socket_merge_success_refusal_and_lost_reply_observation() {
    let server = MockServer::start().await;
    let forge = gateway(&server).await;
    response(
        &server,
        "PUT",
        "/repos/example-org/project/pulls/23/merge",
        200,
        json!({"merged":true,"sha":"merge-commit"}),
    )
    .await;
    assert_eq!(
        forge.merge(&repo(), 23, "h1", "Change").await.unwrap(),
        "merge-commit"
    );
    response(
        &server,
        "GET",
        "/repos/example-org/project/pulls/23",
        200,
        pr("h1", true),
    )
    .await;
    let seen = forge.pr(&repo(), 23).await.unwrap();
    assert_eq!(seen.state, PrState::Merged);
    assert_eq!(seen.merge_commit.as_deref(), Some("merge-commit"));
    response(
        &server,
        "PUT",
        "/repos/example-org/project/pulls/24/merge",
        405,
        json!({"message":"not mergeable"}),
    )
    .await;
    assert!(matches!(
        forge.merge(&repo(), 24, "h1", "Change").await,
        Err(MergeError::Refused(_))
    ));
}

#[tokio::test]
async fn socket_private_creation_and_effective_rules_include_organization_checks() {
    let server = MockServer::start().await;
    let forge = gateway(&server).await;
    Mock::given(method("POST"))
        .and(path("/orgs/example-org/repos"))
        .and(body_json(json!({
            "name":"project",
            "private":true,
            "auto_init":false,
            "allow_squash_merge":true,
            "allow_merge_commit":false,
            "allow_rebase_merge":false
        })))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "id":42,
            "full_name":"example-org/project",
            "private":true
        })))
        .expect(1)
        .mount(&server)
        .await;
    assert_eq!(
        forge.create_repo("example-org", "project").await.unwrap(),
        repo()
    );
    assert_eq!(
        forge
            .create_repo("outside-org", "project")
            .await
            .unwrap_err()
            .detail,
        "invalid repository or organization"
    );
    metadata(&server).await;
    response(&server,"GET","/repos/example-org/project/rules/branches/trunk",200,json!([
        {"type":"pull_request"},{"type":"non_fast_forward"},
        {"type":"required_status_checks","parameters":{"required_status_checks":[{"context":"rust"},{"context":"ui"}]}}
    ])).await;
    let rules = forge.ruleset(&repo()).await.unwrap();
    assert!(rules.pr_required && rules.no_force_push);
    assert_eq!(rules.required_checks, vec!["rust", "ui"]);
}

#[tokio::test]
async fn socket_ruleset_has_no_bypass_and_keeps_required_checks() {
    let server = MockServer::start().await;
    let forge = gateway(&server).await;
    response(
        &server,
        "GET",
        "/repos/example-org/project/rulesets",
        200,
        json!([]),
    )
    .await;
    response(
        &server,
        "POST",
        "/repos/example-org/project/rulesets",
        201,
        json!({"id":5}),
    )
    .await;
    forge
        .install_ruleset(&repo(), "trunk", &["rust".into(), "ui".into()])
        .await
        .unwrap();
    let requests = server.received_requests().await.unwrap();
    let payload = requests
        .iter()
        .find(|r| r.url.path().ends_with("/rulesets") && r.method == "POST")
        .unwrap()
        .body_json::<Value>()
        .unwrap();
    assert_eq!(payload["enforcement"], "active");
    assert_eq!(payload["bypass_actors"], json!([]));
    assert_eq!(
        payload["conditions"]["ref_name"]["include"],
        json!(["refs/heads/trunk"])
    );
    assert!(
        payload["rules"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["type"] == "non_fast_forward")
    );
    let checks = payload["rules"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["type"] == "required_status_checks")
        .unwrap();
    assert_eq!(
        checks["parameters"]["required_status_checks"],
        json!([{"context":"rust"},{"context":"ui"}])
    );
}

#[tokio::test]
async fn socket_close_issue_delete_branch_and_find_pr_are_retry_safe() {
    let server = MockServer::start().await;
    let forge = gateway(&server).await;
    Mock::given(method("PATCH"))
        .and(path("/repos/example-org/project/issues/4"))
        .and(body_json(json!({"state":"closed"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"state":"closed"})))
        .expect(1)
        .mount(&server)
        .await;
    forge.close_issue(&repo(), 4).await.unwrap();
    metadata(&server).await;
    // Percent-encoding protects the ref path, including its slash.
    Mock::given(method("DELETE"))
        .and(path("/repos/example-org/project/git/refs/heads/dvb%2Ft-1"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    forge.delete_branch(&repo(), "dvb/t-1").await.unwrap();
    assert!(forge.delete_branch(&repo(), "trunk").await.is_err());
    Mock::given(method("GET"))
        .and(path("/repos/example-org/project/pulls"))
        .and(query_param("state", "all"))
        .and(query_param("head", "example-org:dvb/t-1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([pr("h1", true)])))
        .expect(1)
        .mount(&server)
        .await;
    assert_eq!(
        forge
            .find_pr(&repo(), "dvb/t-1")
            .await
            .unwrap()
            .unwrap()
            .state,
        PrState::Merged
    );
}

#[tokio::test]
async fn socket_push_reuses_one_repository_token_for_metadata_and_git_without_network() {
    let server = MockServer::start().await;
    let forge = gateway(&server).await;
    metadata(&server).await;
    let temp = tempfile::tempdir().unwrap();
    let absent_mirror = temp.path().join("absent"); // git fails at -C, before any network.
    for _ in 0..2 {
        assert!(
            forge
                .push(&repo(), &absent_mirror, "dvb/t-1", "h1")
                .await
                .is_err()
        );
    }
    let requests = server.received_requests().await.unwrap();
    let tokens: Vec<Value> = requests
        .iter()
        .filter(|r| r.url.path().ends_with("access_tokens"))
        .map(|r| r.body_json::<Value>().unwrap())
        .collect();
    assert_eq!(tokens, vec![json!({"repositories":["project"]})]);
    assert_eq!(
        forge
            .push(&repo(), &absent_mirror, "trunk", "h1")
            .await
            .unwrap_err()
            .detail,
        "gateway only pushes dvb task branches"
    );
    assert_eq!(
        forge
            .push_setup(&repo(), &absent_mirror, "trunk", "h1")
            .await
            .unwrap_err()
            .detail,
        "setup push requires a recorded repository creation"
    );
}

#[tokio::test]
async fn socket_setup_authorization_resumes_only_the_recorded_repository_identity() {
    let server = MockServer::start().await;
    let first = gateway(&server).await;
    response(
        &server,
        "POST",
        "/orgs/example-org/repos",
        201,
        json!({"id":42,"full_name":"example-org/project","private":true}),
    )
    .await;
    let repo = first.create_repo("example-org", "project").await.unwrap();
    let receipt = first.created_repo_id(&repo).unwrap();
    assert_eq!(receipt, 42);
    drop(first);
    let resumed = gateway(&server).await;
    assert_eq!(resumed.created_repo_id(&repo), None);
    Mock::given(method("GET"))
        .and(path("/repos/example-org/project"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"id":42,"private":true,"default_branch":"trunk"})),
        )
        .mount(&server)
        .await;
    assert!(resumed.resume_setup(&repo, 99).await.is_err());
    assert_eq!(resumed.created_repo_id(&repo), None);
    resumed.resume_setup(&repo, receipt).await.unwrap();
    assert_eq!(resumed.created_repo_id(&repo), Some(42));
    let temp = tempfile::tempdir().unwrap();
    // The absent mirror prevents Git networking, after checking the resumed receipt.
    assert!(
        resumed
            .push_setup(&repo, &temp.path().join("absent"), "trunk", "h1")
            .await
            .is_err()
    );
    // Two resume reads, then the setup push's own metadata read.
    let requests = server.received_requests().await.unwrap();
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.method == "GET" && r.url.path() == "/repos/example-org/project")
            .count(),
        3
    );
}

/// The default branch `trunk` is at commit `m2`.
async fn main_head(server: &MockServer) {
    response(
        server,
        "GET",
        "/repos/example-org/project/commits/trunk",
        200,
        json!({"sha":"m2"}),
    )
    .await;
}

#[tokio::test]
async fn socket_main_checks_retain_successful_jobs_across_partial_reruns() {
    let server = MockServer::start().await;
    let forge = gateway(&server).await;
    main_head(&server).await;
    response(
        &server,
        "GET",
        "/repos/example-org/project/actions/runs",
        200,
        json!({"workflow_runs":[{
            "id":9,"workflow_id":1,"run_attempt":2,"event":"push","head_branch":"trunk",
            "head_sha":"m2","html_url":"https://github.invalid/runs/9"
        }]}),
    )
    .await;
    let latest = json!({"name":"ui","status":"completed","conclusion":"success","run_attempt":2});
    Mock::given(method("GET"))
        .and(path(
            "/repos/example-org/project/actions/runs/9/attempts/2/jobs",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"jobs":[latest.clone()]})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/example-org/project/actions/runs/9/jobs"))
        .and(query_param("filter", "all"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"jobs":[
            {"name":"rust","status":"completed","conclusion":"success","run_attempt":1},
            {"name":"ui","status":"completed","conclusion":"failure","run_attempt":1},
            latest
        ]})))
        .mount(&server)
        .await;
    let mut checks = forge.main_checks(&repo(), "trunk").await.unwrap();
    checks.sort_by(|a, b| a.name.cmp(&b.name));
    assert_eq!(
        checks,
        ["rust", "ui"].map(|name| CheckObservation {
            name: name.into(),
            status: ChecksStatus::Success,
            conclusion: Some("success".into()),
            url: Some("https://github.invalid/runs/9".into()),
            run: None,
        })
    );
}

#[tokio::test]
async fn socket_main_checks_ignore_manual_scheduled_and_older_commit_runs() {
    // `None`: main's head has no push or pull_request run, so its state is unknown.
    for event in [None, Some("push"), Some("pull_request")] {
        for conclusion in ["failure", "success", "skipped"] {
            let server = MockServer::start().await;
            let forge = gateway(&server).await;
            main_head(&server).await;
            let run = |id: u64, event: &str, sha: &str| {
                json!({"id":id,"workflow_id":1,"run_attempt":1,"event":event,
                    "head_branch":"trunk","head_sha":sha,
                    "html_url":format!("https://github.invalid/runs/{id}")})
            };
            // Newest first: manual and scheduled runs on the head, newer than
            // its own run, and failing runs on an older main commit.
            let mut runs = vec![
                run(13, "workflow_dispatch", "m2"),
                run(12, "schedule", "m2"),
                run(11, "pull_request_target", "m2"),
            ];
            runs.extend(event.map(|event| run(10, event, "m2")));
            runs.extend([run(8, "workflow_dispatch", "m1"), run(7, "push", "m1")]);
            Mock::given(method("GET"))
                .and(path("/repos/example-org/project/actions/runs"))
                .and(query_param("head_sha", "m2"))
                .and(query_param("status", "completed"))
                .respond_with(
                    ResponseTemplate::new(200).set_body_json(json!({"workflow_runs":runs})),
                )
                .expect(1)
                .mount(&server)
                .await;
            for (id, result) in [
                (13, "failure"),
                (12, "failure"),
                (11, "failure"),
                (10, conclusion),
                (8, "failure"),
                (7, "failure"),
            ] {
                for route in [
                    format!("/actions/runs/{id}/jobs"),
                    format!("/actions/runs/{id}/attempts/1/jobs"),
                ] {
                    Mock::given(method("GET"))
                        .and(path(format!("/repos/example-org/project{route}")))
                        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"jobs":[{
                            "name":"rust","status":"completed","conclusion":result,"run_attempt":1
                        }]})))
                        .mount(&server)
                        .await;
                }
            }
            assert_eq!(
                forge.main_checks(&repo(), "trunk").await.unwrap(),
                event
                    .map(|_| CheckObservation {
                        name: "rust".into(),
                        status: match conclusion {
                            "success" => ChecksStatus::Success,
                            "skipped" => ChecksStatus::Missing,
                            _ => ChecksStatus::Failed,
                        },
                        conclusion: Some(conclusion.into()),
                        url: Some("https://github.invalid/runs/10".into()),
                        run: None,
                    })
                    .into_iter()
                    .collect::<Vec<_>>(),
                "{event:?}: {conclusion}: only main's head's push or pull_request run counts"
            );
        }
    }
}

/// Job `id` on h1, of attempt `attempt` of workflow run 5.
async fn job(server: &MockServer, id: u64, attempt: u64) {
    response(
        server,
        "GET",
        &format!("/repos/example-org/project/actions/jobs/{id}"),
        200,
        json!({"id":id,"name":format!("job{id}"),"head_sha":"h1","run_id":5,"run_attempt":attempt}),
    )
    .await;
}

/// Workflow run 5, on `attempt`, `status`.
async fn workflow(server: &MockServer, attempt: u64, status: &str) {
    response(
        server,
        "GET",
        "/repos/example-org/project/actions/runs/5",
        200,
        json!({"id":5,"run_attempt":attempt,"status":status}),
    )
    .await;
}

async fn rerun_post(server: &MockServer, status: u16, body: Value, times: u64) {
    Mock::given(method("POST"))
        .and(path(
            "/repos/example-org/project/actions/runs/5/rerun-failed-jobs",
        ))
        .respond_with(ResponseTemplate::new(status).set_body_json(body))
        .expect(times)
        .mount(server)
        .await;
}

#[tokio::test]
async fn socket_rerun_reruns_the_given_jobs_workflow_once() {
    let server = MockServer::start().await;
    let forge = gateway(&server).await;
    // Two interrupted required jobs of one workflow: one rerun of the run.
    job(&server, 11, 1).await;
    job(&server, 13, 1).await;
    workflow(&server, 1, "completed").await;
    rerun_post(&server, 201, json!({}), 1).await;
    // Another cancelled check on the head (not given) is never read.
    Mock::given(path("/repos/example-org/project/actions/jobs/12"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;
    forge
        .rerun_checks(&repo(), "h1", &[11, 13], false)
        .await
        .unwrap();
}

#[tokio::test]
async fn socket_a_workflow_rerun_or_still_running_is_not_asked_and_a_rerun_job_is_final() {
    for (attempt, status) in [(2, "queued"), (2, "completed"), (1, "in_progress")] {
        let server = MockServer::start().await;
        let forge = gateway(&server).await;
        job(&server, 11, 1).await;
        job(&server, 13, 1).await;
        workflow(&server, attempt, status).await;
        rerun_post(&server, 201, json!({}), 0).await;
        forge
            .rerun_checks(&repo(), "h1", &[11, 13], false)
            .await
            .unwrap();
    }

    // A job of the rerun's attempt, ended cancelled again.
    let server = MockServer::start().await;
    let forge = gateway(&server).await;
    job(&server, 21, 2).await;
    rerun_post(&server, 201, json!({}), 0).await;
    let error = forge
        .rerun_checks(&repo(), "h1", &[21], false)
        .await
        .unwrap_err();
    assert!(!error.retryable);
    assert!(
        error.detail.contains("job21 failed again"),
        "{}",
        error.detail
    );

    // Every job that failed again is named.
    let server = MockServer::start().await;
    let forge = gateway(&server).await;
    job(&server, 21, 2).await;
    job(&server, 22, 2).await;
    rerun_post(&server, 201, json!({}), 0).await;
    let error = forge
        .rerun_checks(&repo(), "h1", &[21, 22], false)
        .await
        .unwrap_err();
    assert_eq!(error.detail, "job21, job22 failed again after its rerun");
}

#[tokio::test]
async fn socket_a_check_excerpt_follows_the_log_redirect_without_credentials() {
    let server = MockServer::start().await;
    let storage = MockServer::start().await;
    let forge = gateway(&server).await;
    Mock::given(method("GET"))
        .and(path("/repos/example-org/project/actions/jobs/21/logs"))
        .respond_with(
            ResponseTemplate::new(302)
                .insert_header("location", format!("{}/logs/21.txt", storage.uri())),
        )
        .expect(1)
        .mount(&server)
        .await;
    let token = format!("ghs_{}", "a".repeat(36));
    Mock::given(method("GET"))
        .and(path("/logs/21.txt"))
        .respond_with(ResponseTemplate::new(200).set_body_string(format!(
            "2026-10-18T04:20:45.1Z FAIL: 2: dvb watch differs from the stream\n\
             2026-10-18T04:20:46.1Z token {token}\n\
             2026-10-18T04:20:47.1Z ##[error]Process completed with exit code 1.\n\
             2026-10-18T04:20:48.1Z Post job cleanup.\n"
        )))
        .expect(1)
        .mount(&storage)
        .await;
    assert_eq!(
        forge.check_excerpt(&repo(), 21).await.unwrap(),
        "FAIL: 2: dvb watch differs from the stream / Process completed with exit code 1."
    );
    let requests = storage.received_requests().await.unwrap();
    assert!(
        requests
            .iter()
            .all(|r| !r.headers.contains_key("authorization"))
    );

    // A missing log is an error for the engine to report, not an excerpt.
    let server = MockServer::start().await;
    let forge = gateway(&server).await;
    response(
        &server,
        "GET",
        "/repos/example-org/project/actions/jobs/22/logs",
        404,
        json!({"message":"Not Found"}),
    )
    .await;
    assert!(forge.check_excerpt(&repo(), 22).await.is_err());
}

#[tokio::test]
async fn socket_rerun_retains_a_second_failure_after_older_jobs_of_the_same_workflow() {
    for runs in [[11, 21], [21, 11]] {
        for status in ["queued", "in_progress", "completed"] {
            let server = MockServer::start().await;
            let forge = gateway(&server).await;
            job(&server, 11, 1).await;
            job(&server, 21, 2).await;
            workflow(&server, 2, status).await;
            rerun_post(&server, 201, json!({}), 0).await;

            let result = forge.rerun_checks(&repo(), "h1", &runs, false).await;
            if status == "completed" {
                let error = result.unwrap_err();
                assert!(!error.retryable);
                assert!(
                    error.detail.contains("job21 failed again"),
                    "{}",
                    error.detail
                );
            } else {
                result.unwrap();
            }
        }
    }
}

#[tokio::test]
async fn socket_rerun_refusals_are_final_and_other_apps_checks_have_no_job() {
    let server = MockServer::start().await;
    let forge = gateway(&server).await;
    response(
        &server,
        "GET",
        "/repos/example-org/project/actions/jobs/9",
        404,
        json!({"message":"Not Found"}),
    )
    .await;
    assert!(
        !forge
            .rerun_checks(&repo(), "h1", &[9], false)
            .await
            .unwrap_err()
            .retryable
    );
}

#[tokio::test]
async fn socket_rerun_keeps_transient_failures_retryable() {
    for (status, body, retryable) in [
        (429, json!({"message":"Too many requests"}), true),
        (503, json!({"message":"Unavailable"}), true),
        (403, json!({"message":"API rate limit exceeded"}), true),
        (
            403,
            json!({"message":"Resource not accessible by integration"}),
            false,
        ),
        (422, json!({"message":"Unprocessable"}), false),
    ] {
        let server = MockServer::start().await;
        let forge = gateway(&server).await;
        job(&server, 11, 1).await;
        workflow(&server, 1, "completed").await;
        rerun_post(&server, status, body, 1).await;
        let error = forge
            .rerun_checks(&repo(), "h1", &[11], false)
            .await
            .unwrap_err();
        assert_eq!(error.retryable, retryable, "status {status}");
    }
    // A throttled read of the job is retryable too.
    let server = MockServer::start().await;
    let forge = gateway(&server).await;
    response(
        &server,
        "GET",
        "/repos/example-org/project/actions/jobs/11",
        503,
        json!({"message":"Unavailable"}),
    )
    .await;
    assert!(
        forge
            .rerun_checks(&repo(), "h1", &[11], false)
            .await
            .unwrap_err()
            .retryable
    );
}

#[tokio::test]
async fn socket_ruleset_updates_only_dvb_and_no_ci_removes_its_checks_rule() {
    let server = MockServer::start().await;
    let forge = gateway(&server).await;
    Mock::given(method("GET"))
        .and(path("/repos/example-org/project/rulesets"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"id":99,"name":"owner rules"}, {"id":5,"name":"dvb default branch"}
        ])))
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/repos/example-org/project/rulesets/5"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":5})))
        .expect(2)
        .mount(&server)
        .await;
    forge
        .install_ruleset(&repo(), "trunk", &["new-check".into()])
        .await
        .unwrap();
    forge.install_ruleset(&repo(), "trunk", &[]).await.unwrap();
    let requests = server.received_requests().await.unwrap();
    let writes: Vec<_> = requests.iter().filter(|r| r.method == "PUT").collect();
    assert_eq!(writes.len(), 2);
    assert!(
        writes
            .iter()
            .all(|r| r.url.path() == "/repos/example-org/project/rulesets/5")
    );
    let before = writes[0].body_json::<Value>().unwrap();
    let after = writes[1].body_json::<Value>().unwrap();
    let check = before["rules"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["type"] == "required_status_checks")
        .unwrap();
    assert_eq!(
        check["parameters"]["required_status_checks"],
        json!([{"context":"new-check"}])
    );
    assert!(
        !after["rules"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["type"] == "required_status_checks")
    );
    for rule in after["rules"].as_array().unwrap() {
        assert!(before["rules"].as_array().unwrap().contains(rule));
    }
}

#[tokio::test]
async fn socket_cancel_stops_only_the_heads_unfinished_workflow_runs() {
    let server = MockServer::start().await;
    let forge = gateway(&server).await;
    Mock::given(method("GET"))
        .and(path("/repos/example-org/project/actions/runs"))
        .and(query_param("head_sha", "h1"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"workflow_runs":[
                {"id":5,"status":"in_progress"},
                {"id":6,"status":"queued"},
                {"id":7,"status":"completed"}
            ]})),
        )
        .expect(1)
        .mount(&server)
        .await;
    for (id, times) in [(5, 1), (6, 1), (7, 0)] {
        Mock::given(method("POST"))
            .and(path(format!(
                "/repos/example-org/project/actions/runs/{id}/cancel"
            )))
            .respond_with(ResponseTemplate::new(202).set_body_json(json!({})))
            .expect(times)
            .mount(&server)
            .await;
    }
    forge.cancel_checks(&repo(), "h1").await.unwrap();

    // A refused cancel (the run ended meanwhile) is reported, not hidden.
    let server = MockServer::start().await;
    let forge = gateway(&server).await;
    response(
        &server,
        "GET",
        "/repos/example-org/project/actions/runs",
        200,
        json!({"workflow_runs":[{"id":5,"status":"in_progress"}]}),
    )
    .await;
    response(
        &server,
        "POST",
        "/repos/example-org/project/actions/runs/5/cancel",
        409,
        json!({"message":"Cannot cancel a workflow run that is completed."}),
    )
    .await;
    assert!(forge.cancel_checks(&repo(), "h1").await.is_err());
}

#[tokio::test]
async fn socket_a_personal_account_lists_and_probes_its_own_repositories_and_creates_none() {
    use dvb_core::api::GithubAccountType;
    use dvb_core::forge::Repo;
    let server = MockServer::start().await;
    let forge = support::gateway_for(&server, "example-user", GithubAccountType::User).await;
    // GitHub serves `POST /user/repos` only to user tokens, so the App creates nothing.
    let repo = Repo("example-user/project".into());
    assert!(!forge.can_create_repo());
    assert!(forge.create_repo("example-user", "project").await.is_err());
    assert_eq!(forge.created_repo_id(&repo), None);
    // A Free plan shows in the refused rulesets read.
    response(
        &server,
        "GET",
        "/repos/example-user/project/rulesets",
        403,
        json!({"message":"Upgrade to GitHub Pro or make this repository public to enable this feature."}),
    )
    .await;
    assert!(!forge.rulesets_allowed(&repo).await.unwrap());
    response(
        &server,
        "GET",
        "/installation/repositories",
        200,
        json!({"repositories":[
            {"full_name":"example-user/project","private":true,"default_branch":"main"}
        ]}),
    )
    .await;
    let access = forge.accessible_repositories().await.unwrap();
    assert_eq!(
        access.repository_access_url,
        "https://github.com/settings/installations/7"
    );
    assert_eq!(access.repositories[0].repo, "example-user/project");
    // No board for a personal account: GitHub gives its App no projects permission.
    forge.mirror_project(&repo).await.unwrap();
    let requests = server.received_requests().await.unwrap();
    assert!(requests.iter().all(|r| {
        let path = r.url.path();
        !path.starts_with("/orgs/")
            && !path.starts_with("/users/")
            && !path.starts_with("/user/")
            && path != "/graphql"
    }));
}

#[tokio::test]
async fn socket_a_renamed_repositorys_token_refusal_is_unreachable_and_its_id_is_still_read() {
    let server = MockServer::start().await;
    let forge = gateway(&server).await;
    Mock::given(method("POST"))
        .and(path("/app/installations/7/access_tokens"))
        .and(body_json(json!({"repositories":["project"]})))
        .respond_with(ResponseTemplate::new(422).set_body_json(json!({
            "message":"There is at least one repository that does not exist or is not accessible to the parent installation."
        })))
        .with_priority(1)
        .mount(&server)
        .await;
    let error = forge.visibility(&repo()).await.unwrap_err();
    assert!(error.unreachable && !error.retryable, "{error:?}");
    assert_eq!(error.detail, "GitHub HTTP 422");

    // The installation-wide token reads both names; GitHub follows the old one.
    response(
        &server,
        "GET",
        "/repos/example-org/project",
        200,
        json!({"id":41}),
    )
    .await;
    response(
        &server,
        "GET",
        "/repos/example-org/missing",
        404,
        json!({"message":"Not Found"}),
    )
    .await;
    assert_eq!(forge.repository_id(&repo()).await.unwrap(), Some(41));
    let missing = dvb_core::forge::Repo("example-org/missing".into());
    assert_eq!(forge.repository_id(&missing).await.unwrap(), None);
    let outside = dvb_core::forge::Repo("other-org/project".into());
    assert_eq!(forge.repository_id(&outside).await.unwrap(), None);
}
