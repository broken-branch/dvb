//! Issue reads for the engine's trust rule, with the repository's installation
//! token. Runs never see a token.
use crate::github::{GitHub, api_error, refused, stamp_of, text};
use dvb_core::forge::{
    Association, ForgeError, Issue, IssueComment, IssueError, IssueState, IssueSummary, Repo,
    Visibility,
};
use octocrab::FromResponse;
use serde_json::Value;

/// An unknown or missing association counts as no standing.
fn association(v: &Value) -> Association {
    serde_json::from_value(v["author_association"].clone()).unwrap_or(Association::None)
}

fn login(user: &Value) -> String {
    user["login"].as_str().unwrap_or("ghost").to_owned()
}

fn labels(v: &Value) -> Vec<String> {
    v["labels"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|l| l["name"].as_str().or_else(|| l.as_str()))
        .map(str::to_owned)
        .collect()
}

fn is_pull_request(v: &Value) -> bool {
    v.get("pull_request").is_some_and(|p| !p.is_null())
}

fn number(v: &Value) -> Result<u64, ForgeError> {
    v["number"]
        .as_u64()
        .ok_or_else(|| refused("GitHub response missing number"))
}

impl GitHub {
    pub(super) async fn read_issue(
        &self,
        repo: &Repo,
        number: u64,
    ) -> Result<Option<Issue>, IssueError> {
        let api = self.client(repo).await?;
        let response = api
            .get_raw(self.route(repo, &format!("/issues/{number}"))?)
            .await
            .map_err(api_error)?;
        if matches!(response.status().as_u16(), 404 | 410) {
            return Ok(None);
        }
        let response = octocrab::map_github_error(response)
            .await
            .map_err(api_error)?;
        let v = Value::from_response(response).await.map_err(api_error)?;
        if is_pull_request(&v) {
            return Err(IssueError::PullRequest { number });
        }
        let comments = self
            .list(
                &api,
                &self.route(repo, &format!("/issues/{number}/comments"))?,
                None,
            )
            .await?
            .iter()
            .map(|c| IssueComment {
                author: login(&c["user"]),
                association: association(c),
                body: c["body"].as_str().unwrap_or_default().to_owned(),
            })
            .collect();
        Ok(Some(Issue {
            number,
            url: text(&v, "html_url")?.to_owned(),
            title: text(&v, "title")?.to_owned(),
            body: v["body"].as_str().unwrap_or_default().to_owned(),
            state: if v["state"] == "closed" {
                IssueState::Closed
            } else {
                IssueState::Open
            },
            labels: labels(&v),
            author: login(&v["user"]),
            association: association(&v),
            comments,
        }))
    }

    /// The repository metadata's `visibility` field, not `private`, which is
    /// also true for an internal repository. Only `private` is Private; any
    /// other or a missing value gets the public rules. Unlike other metadata
    /// reads, a public repository is reported, not refused.
    pub(super) async fn read_visibility(&self, repo: &Repo) -> Result<Visibility, ForgeError> {
        let api = self.client(repo).await?;
        let v: Value = api.get(self.route(repo, "")?).await.map_err(api_error)?;
        Ok(if v["visibility"] == "private" {
            Visibility::Private
        } else {
            Visibility::Public
        })
    }

    pub(super) async fn list_open_issues(
        &self,
        repo: &Repo,
    ) -> Result<Vec<IssueSummary>, ForgeError> {
        let api = self.client(repo).await?;
        let rows = self
            .list(
                &api,
                &self.route(repo, "/issues?state=open&sort=created&direction=asc")?,
                None,
            )
            .await?;
        rows.iter()
            .filter(|v| !is_pull_request(v))
            .map(|v| {
                Ok(IssueSummary {
                    number: number(v)?,
                    url: text(v, "html_url")?.to_owned(),
                    title: text(v, "title")?.to_owned(),
                    labels: labels(v),
                    author: login(&v["user"]),
                    association: association(v),
                    opened: stamp_of(text(v, "created_at")?)?,
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::Wait;
    use bytes::Bytes;
    use dvb_core::Timestamp;
    use dvb_core::forge::Forge;
    use http_body_util::{BodyExt, Full};
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    type Requests = Arc<Mutex<Vec<(String, Value)>>>;

    /// A GitHub forge answering from recorded responses for a repository of
    /// this `visibility` (None: the field is missing), and the requests it
    /// made. Any other request, such as a membership or collaborator check,
    /// panics.
    fn recorded(visibility: Option<&'static str>) -> (GitHub, Requests) {
        let requests: Requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = requests.clone();
        let service = tower::service_fn(move |request: http::Request<octocrab::OctoBody>| {
            let recorded = recorded.clone();
            async move {
                let path = request.uri().path().to_owned();
                let query = request.uri().query().unwrap_or_default().to_owned();
                let bytes = request.into_body().collect().await.unwrap().to_bytes();
                let body = if bytes.is_empty() {
                    Value::Null
                } else {
                    serde_json::from_slice(&bytes).unwrap()
                };
                recorded.lock().unwrap().push((path.clone(), body));
                let page = |rows: Value| {
                    if query.contains("page=1") {
                        rows
                    } else {
                        json!([])
                    }
                };
                let (status, response) = match path.as_str() {
                    p if p.ends_with("/access_tokens") => (
                        201,
                        json!({"token":"offline-issues","expires_at":"2030-01-01T00:00:00Z"}),
                    ),
                    "/repos/org/repo" => {
                        let mut metadata = json!({"full_name":"org/repo",
                            "private":visibility != Some("public"),"default_branch":"main"});
                        if let Some(visibility) = visibility {
                            metadata["visibility"] = json!(visibility);
                        }
                        (200, metadata)
                    }
                    "/repos/org/repo/issues/7" => (
                        200,
                        json!({"number":7,"html_url":"https://github.com/org/repo/issues/7",
                            "title":"Crash","body":null,"state":"open",
                            "labels":[{"name":"bug"},{"name":"dvb"}],
                            "user":{"login":"outsider"},"author_association":"NONE"}),
                    ),
                    "/repos/org/repo/issues/8" => (
                        200,
                        json!({"number":8,"html_url":"https://github.com/org/repo/pull/8",
                            "title":"PR","state":"open","pull_request":{"url":"x"}}),
                    ),
                    "/repos/org/repo/issues/9" => (404, json!({"message":"Not Found"})),
                    "/repos/org/repo/issues/13" => (
                        403,
                        json!({"message":"Resource not accessible by integration"}),
                    ),
                    "/repos/org/repo/issues/7/comments" => (
                        200,
                        page(json!([
                            {"user":{"login":"alice"},"author_association":"MEMBER","body":"Seen"},
                            {"user":{"login":"bob"},"author_association":"SOMETHING_NEW","body":"Hi"}
                        ])),
                    ),
                    "/repos/org/repo/issues" => (
                        200,
                        page(json!([
                            {"number":7,"html_url":"https://github.com/org/repo/issues/7",
                                "title":"Crash","labels":[{"name":"dvb"}],
                                "user":{"login":"outsider"},"author_association":"NONE",
                                "created_at":"2026-10-24T13:35:00Z"},
                            {"number":8,"html_url":"https://github.com/org/repo/pull/8",
                                "title":"PR","labels":[],"author_association":"OWNER","pull_request":{}}
                        ])),
                    ),
                    other => panic!("unexpected request {other}"),
                };
                let body = if response.is_null() {
                    Bytes::new()
                } else {
                    Bytes::from(response.to_string())
                };
                Ok::<_, std::convert::Infallible>(
                    http::Response::builder()
                        .status(status)
                        .body(Full::new(body))
                        .unwrap(),
                )
            }
        });
        let app = octocrab::OctocrabBuilder::new_empty()
            .with_service(service)
            .with_auth(octocrab::AuthState::None)
            .build()
            .unwrap()
            .user_access_token("offline-app")
            .unwrap();
        let wait = Arc::new(Wait {
            clock: Arc::new(dvb_core::FakeClock::at(Timestamp(0))),
            deadline: std::sync::Mutex::new(None),
        });
        let forge = GitHub::with_app_client(
            app,
            7,
            "org".into(),
            dvb_core::api::GithubAccountType::Organization,
            wait,
        )
        .unwrap();
        (forge, requests)
    }

    fn token_scopes(requests: &Requests) -> Vec<Value> {
        let requests = requests.lock().unwrap();
        requests
            .iter()
            .filter(|(path, _)| path.ends_with("/access_tokens"))
            .map(|(_, body)| body.clone())
            .collect()
    }

    #[tokio::test]
    async fn pull_request_number_is_distinguished_and_a_refused_read_stays_an_error() {
        let (forge, requests) = recorded(Some("private"));
        let repo = Repo("org/repo".into());
        let error = forge.issue(&repo, 8).await.unwrap_err();
        assert_eq!(error.to_string(), "#8 is a pull request, not an issue");
        assert!(matches!(forge.issue(&repo, 13).await,
            Err(IssueError::Forge(error)) if error.detail == "GitHub HTTP 403 (App permission missing)"));
        assert_eq!(
            token_scopes(&requests),
            vec![json!({"repositories":["repo"]})]
        );
        assert!(
            !requests
                .lock()
                .unwrap()
                .iter()
                .any(|(path, _)| path.ends_with("/comments"))
        );
    }

    #[tokio::test]
    async fn issue_reads_use_the_one_repository_token() {
        let (forge, requests) = recorded(Some("public"));
        let repo = Repo("org/repo".into());
        assert_eq!(forge.visibility(&repo).await.unwrap(), Visibility::Public);
        let issue = forge.issue(&repo, 7).await.unwrap().unwrap();
        assert_eq!(issue.title, "Crash");
        assert_eq!(issue.body, "");
        assert_eq!(issue.association, Association::None);
        assert_eq!(issue.labels, vec!["bug", "dvb"]);
        assert_eq!(
            issue
                .comments
                .iter()
                .map(|c| (c.author.as_str(), c.association))
                .collect::<Vec<_>>(),
            vec![("alice", Association::Member), ("bob", Association::None)]
        );
        assert_eq!(
            forge.issue(&repo, 8).await,
            Err(IssueError::PullRequest { number: 8 })
        );
        assert_eq!(forge.issue(&repo, 9).await.unwrap(), None);
        let open = forge.open_issues(&repo).await.unwrap();
        assert_eq!(open.iter().map(|i| i.number).collect::<Vec<_>>(), vec![7]);
        assert_eq!(open[0].labels, vec!["dvb"]);
        assert_eq!(open[0].author, "outsider");
        assert_eq!(open[0].opened, Timestamp(1_792_848_900_000));
        assert!(
            !requests
                .lock()
                .unwrap()
                .iter()
                .any(|(path, _)| path.contains("/collaborators/") || path.starts_with("/orgs/")),
            "no membership or collaborator check is made"
        );
        assert_eq!(
            token_scopes(&requests),
            vec![json!({"repositories":["repo"]})]
        );
    }

    #[tokio::test]
    async fn visibility_comes_from_metadata_and_a_public_repository_is_not_refused() {
        for (field, visibility) in [
            (Some("private"), Visibility::Private),
            (Some("public"), Visibility::Public),
            (Some("internal"), Visibility::Public),
            (None, Visibility::Public),
        ] {
            let (forge, requests) = recorded(field);
            let repo = Repo("org/repo".into());
            assert_eq!(
                forge.visibility(&repo).await.unwrap(),
                visibility,
                "{field:?}: an internal repository, which reports private, has public rules"
            );
            assert_eq!(
                forge.issue(&repo, 7).await.unwrap().unwrap().title,
                "Crash",
                "issue reads are not refused for a public repository"
            );
            assert_eq!(
                token_scopes(&requests),
                vec![json!({"repositories":["repo"]})],
                "visibility and issue reads share the repository's token"
            );
        }
    }
}
