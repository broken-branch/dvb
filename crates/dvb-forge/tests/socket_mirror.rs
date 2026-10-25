//! Recorded Projects v2/REST shapes. Run only in CI; no real GitHub account or token.
mod support;
use support::{gateway, repo};

use dvb_core::forge::{BoardState, Forge, GoalMirror, TaskMirror};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use wiremock::matchers::{body_json, method, path, query_param};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

fn goal() -> GoalMirror {
    GoalMirror {
        key: "g1".into(),
        title: "Ship feature".into(),
        description: "Two independently reviewed PRs".into(),
        done: false,
    }
}
fn task() -> TaskMirror {
    TaskMirror {
        key: "t1".into(),
        title: "Build feature".into(),
        role: "Backend builder".into(),
        model: "fixture-model".into(),
        state: BoardState::Working,
        pr: None,
        milestone: 3,
    }
}

struct BoardFixture {
    board: bool,
    title: String,
    fields: Vec<Value>,
    items: Vec<Value>,
    writes: Vec<Value>,
    fail_delete: bool,
    forbid_link: bool,
    item_response: Option<Value>,
}
impl Default for BoardFixture {
    fn default() -> Self {
        Self {
            board: false,
            title: "dvb: example-org/project".into(),
            fields: vec![json!({
                "id":"FIELD-Status",
                "name":"Status",
                "options":[
                    {
                        "id":"todo",
                        "name":"Todo"
                    },
                    {
                        "id":"progress",
                        "name":"In progress"
                    },
                    {
                        "id":"done",
                        "name":"Done"
                    }
                ]
            })],
            items: Vec::new(),
            writes: Vec::new(),
            fail_delete: false,
            forbid_link: false,
            item_response: None,
        }
    }
}
#[derive(Clone)]
struct Graph(Arc<Mutex<BoardFixture>>);
impl Respond for Graph {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        let query = body["query"].as_str().unwrap();
        let vars = &body["variables"];
        let mut state = self.0.lock().unwrap();
        let data = if query.contains("organization(login:") {
            json!({
                "organization":{
                    "id":"ORG",
                    "projectsV2":{
                        "nodes":if state.board {
                            vec![
                                json!({
                                    "id":"BOARD",
                                    "title":state.title
                                })
                            ]
                        } else {
                            vec![]
                        },
                        "pageInfo":{
                            "hasNextPage":false,
                            "endCursor":null
                        }
                    }
                }
            })
        } else if query.contains("createProjectV2(input:") {
            state.board = true;
            json!({"createProjectV2":{"projectV2":{"id":"BOARD","title":state.title}}})
        } else if query.contains("on ProjectV2{id title}") {
            json!({"node":{"id":"BOARD","title":state.title}})
        } else if query.contains("on ProjectV2Item{") {
            if let Some(response) = &state.item_response {
                return ResponseTemplate::new(200).set_body_json(response);
            }
            let node = state
                .items
                .iter()
                .find(|item| item["id"] == vars["id"])
                .cloned()
                .map(|mut item| {
                    item["project"] = json!({"id":"BOARD"});
                    item
                })
                .unwrap_or(Value::Null);
            json!({"node":node})
        } else if query.contains("updateProjectV2(input:") {
            state.title = vars["title"].as_str().unwrap().into();
            state.writes.push(body.clone());
            json!({"updateProjectV2":{"projectV2":{"id":"BOARD"}}})
        } else if query.contains("linkProjectV2ToRepository") {
            state.writes.push(body.clone());
            if state.forbid_link {
                return ResponseTemplate::new(200).set_body_json(json!({
                    "data":{"linkProjectV2ToRepository":null},
                    "errors":[{"type":"FORBIDDEN","path":["linkProjectV2ToRepository"],"message":"private-data"}]
                }));
            }
            json!({"linkProjectV2ToRepository":{"repository":{"id":"REPO"}}})
        } else if query.contains("updateProjectV2Field(input:") {
            let input = &vars["input"];
            let field = state
                .fields
                .iter_mut()
                .find(|f| f["id"] == input["fieldId"])
                .unwrap();
            field["options"] = json!(
                input["singleSelectOptions"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|o| json!({
                        "id":format!("OPTION-{}",
                        o["name"].as_str().unwrap()),
                        "name":o["name"]
                    }))
                    .collect::<Vec<_>>()
            );
            let field = field.clone();
            state.writes.push(body.clone());
            json!({"updateProjectV2Field":{"projectV2Field":field}})
        } else if query.contains("fields(first:") {
            json!({"node":{"fields":{"nodes":state.fields}}})
        } else if query.contains("createProjectV2Field") {
            let input = &vars["input"];
            let name = input["name"].as_str().unwrap();
            let id = format!("FIELD-{name}");
            let f = if input["dataType"] == "TEXT" {
                json!({"id":id,"name":name,"dataType":"TEXT"})
            } else {
                json!({
                    "id":id,
                    "name":name,
                    "options":input["singleSelectOptions"].as_array().unwrap().iter().map(|o|json!({
                        "id":format!("OPTION-{}",
                        o["name"].as_str().unwrap()),
                        "name":o["name"]
                    })).collect::<Vec<_>>()
                })
            };
            state.fields.push(f.clone());
            json!({"createProjectV2Field":{"projectV2Field":f}})
        } else if query.contains("items(first:") {
            let items = state
                .items
                .iter()
                .filter(|i| {
                    query.contains("archivedStates:[ARCHIVED,NOT_ARCHIVED]")
                        || i["isArchived"] != true
                })
                .cloned()
                .collect::<Vec<_>>();
            json!({"node":{"items":{"nodes":items,"pageInfo":{"hasNextPage":false,"endCursor":null}}}})
        } else if query.contains("unarchiveProjectV2Item") {
            let item = state
                .items
                .iter_mut()
                .find(|i| i["id"] == vars["item"])
                .unwrap();
            assert_eq!(item["isArchived"], true);
            item["isArchived"] = json!(false);
            state.writes.push(body.clone());
            json!({"unarchiveProjectV2Item":{"item":{"id":vars["item"]}}})
        } else if query.contains("addProjectV2DraftIssue") {
            let item = json!({
                "id":"DRAFT-CARD",
                "isArchived":false,
                "content":{
                    "id":"DRAFT",
                    "body":vars["body"],
                    "title":vars["title"]
                }
            });
            state.items.push(item);
            state.writes.push(body.clone());
            json!({"addProjectV2DraftIssue":{"projectItem":{"id":"DRAFT-CARD"}}})
        } else if query.contains("updateProjectV2DraftIssue") {
            let item = state
                .items
                .iter_mut()
                .find(|i| i["content"]["id"] == vars["id"])
                .unwrap();
            assert_ne!(
                item["isArchived"], true,
                "unarchive before updating the draft"
            );
            item["content"]["title"] = vars["title"].clone();
            item["content"]["body"] = vars["body"].clone();
            state.writes.push(body.clone());
            json!({"updateProjectV2DraftIssue":{"draftIssue":{"id":"DRAFT"}}})
        } else if query.contains("addProjectV2ItemById") {
            state
                .items
                .push(json!({"id":"PR-CARD","isArchived":false,"content":{"id":vars["content"]}}));
            state.writes.push(body.clone());
            json!({"addProjectV2ItemById":{"item":{"id":"PR-CARD"}}})
        } else if query.contains("updateProjectV2ItemFieldValue") {
            let item = state
                .items
                .iter()
                .find(|i| i["id"] == vars["input"]["itemId"])
                .unwrap();
            assert_ne!(item["isArchived"], true, "unarchive before updating fields");
            state.writes.push(body.clone());
            json!({"updateProjectV2ItemFieldValue":{"projectV2Item":{"id":vars["input"]["itemId"]}}})
        } else if query.contains("deleteProjectV2Item") {
            if state.fail_delete {
                state.fail_delete = false;
                return ResponseTemplate::new(500)
                    .set_body_json(json!({"message":"recorded interruption"}));
            }
            state.items.retain(|i| i["id"] != vars["item"]);
            state.writes.push(body.clone());
            json!({"deleteProjectV2Item":{"deletedItemId":vars["item"]}})
        } else {
            panic!("unrecorded GraphQL operation: {query}");
        };
        drop(state);
        ResponseTemplate::new(200).set_body_json(json!({"data":data}))
    }
}

async fn mount_graph(server: &MockServer, fixture: BoardFixture) -> Graph {
    let graph = Graph(Arc::new(Mutex::new(fixture)));
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(graph.clone())
        .mount(server)
        .await;
    graph
}

async fn metadata(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/repos/example-org/project"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"private":true,"node_id":"REPO"})),
        )
        .mount(server)
        .await;
}

async fn mount_pr(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/repos/example-org/project/pulls/23"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"node_id":"PR23"})))
        .mount(server)
        .await;
    Mock::given(method("PATCH"))
        .and(path("/repos/example-org/project/issues/23"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"number":23})))
        .mount(server)
        .await;
}

#[tokio::test]
async fn socket_forbidden_board_link_is_attempted_once_and_cards_keep_mirroring() {
    let server = MockServer::start().await;
    let forge = gateway(&server).await;
    let graph = mount_graph(
        &server,
        BoardFixture {
            forbid_link: true,
            ..BoardFixture::default()
        },
    )
    .await;

    metadata(&server).await;
    for _ in 0..2 {
        forge.mirror_project(&repo()).await.unwrap();
        forge.mirror_task(&repo(), &task()).await.unwrap();
    }
    let fixture = graph.0.lock().unwrap();
    assert_eq!(fixture.items.len(), 1);
    assert_eq!(fixture.fields.len(), 3);
    assert_eq!(
        fixture
            .writes
            .iter()
            .filter(|w| w["query"]
                .as_str()
                .unwrap()
                .contains("linkProjectV2ToRepository"))
            .count(),
        1
    );
    assert_eq!(
        fixture
            .writes
            .iter()
            .filter(|w| w["query"]
                .as_str()
                .unwrap()
                .contains("updateProjectV2ItemFieldValue"))
            .count(),
        6
    );
}

#[tokio::test]
async fn socket_milestone_reuses_an_exact_closed_title_on_a_later_page() {
    let server = MockServer::start().await;
    let forge = gateway(&server).await;
    metadata(&server).await;
    let rows: Vec<_> = (1..=100)
        .map(|n| json!({"number":n,"title":format!("Other {n}")}))
        .collect();
    for (page, rows) in [
        ("1", json!(rows)),
        (
            "2",
            json!([
                {"number":101,"title":"Ship feature","state":"closed","description":"Manual milestone"}
            ]),
        ),
    ] {
        Mock::given(method("GET"))
            .and(path("/repos/example-org/project/milestones"))
            .and(query_param("state", "all"))
            .and(query_param("per_page", "100"))
            .and(query_param("page", page))
            .respond_with(ResponseTemplate::new(200).set_body_json(rows))
            .expect(2)
            .mount(&server)
            .await;
    }
    Mock::given(method("PATCH"))
        .and(path("/repos/example-org/project/milestones/101"))
        .and(body_json(json!({
            "title":"Ship feature",
            "description":"<!-- dvb goal g1 -->\nTwo independently reviewed PRs",
            "state":"open"
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"number":101})))
        .expect(2)
        .mount(&server)
        .await;
    for _ in 0..2 {
        assert_eq!(forge.mirror_goal(&repo(), &goal()).await.unwrap(), 101);
    }
    assert!(
        !server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .any(|r| r.method == "POST" && r.url.path().ends_with("/milestones"))
    );
}

#[tokio::test]
async fn socket_milestone_title_creation_race_looks_up_and_reuses_the_winner() {
    let server = MockServer::start().await;
    let forge = gateway(&server).await;
    metadata(&server).await;
    Mock::given(method("GET"))
        .and(path("/repos/example-org/project/milestones"))
        .and(query_param("state", "all"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .with_priority(1)
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/example-org/project/milestones"))
        .and(query_param("state", "all"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"number":3,"title":"Ship feature","state":"open"}
        ])))
        .with_priority(2)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/repos/example-org/project/milestones"))
        .respond_with(ResponseTemplate::new(422).set_body_json(json!({
            "message":"Validation Failed",
            "errors":[{"resource":"Milestone","field":"title","code":"already_exists"}]
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("PATCH"))
        .and(path("/repos/example-org/project/milestones/3"))
        .and(body_json(json!({
            "title":"Ship feature",
            "description":"<!-- dvb goal g1 -->\nTwo independently reviewed PRs",
            "state":"open"
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"number":3})))
        .expect(1)
        .mount(&server)
        .await;
    assert_eq!(forge.mirror_goal(&repo(), &goal()).await.unwrap(), 3);
}

#[tokio::test]
async fn socket_board_graphql_refusal_reports_its_codes_and_clipped_message() {
    let server = MockServer::start().await;
    let forge = gateway(&server).await;
    metadata(&server).await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {"organization": null},
            "errors": [{
                "type": "FORBIDDEN",
                "path": ["organization", "projectsV2", "private-name"],
                "message": "Resource not accessible: private-token"
            }]
        })))
        .expect(1)
        .mount(&server)
        .await;

    let error = forge.mirror_project(&repo()).await.unwrap_err();
    assert_eq!(
        error.detail,
        "GitHub board response is unreadable (organization FORBIDDEN: Resource not accessible: \
            private-token)"
    );
    assert!(!error.retryable);
}

#[tokio::test]
async fn socket_board_repairs_only_changed_titles() {
    let server = MockServer::start().await;
    let forge = gateway(&server).await;
    let graph = mount_graph(
        &server,
        BoardFixture {
            board: true,
            ..BoardFixture::default()
        },
    )
    .await;

    metadata(&server).await;

    forge.mirror_project(&repo()).await.unwrap();
    assert!(!graph.0.lock().unwrap().writes.iter().any(|w| {
        w["query"]
            .as_str()
            .unwrap()
            .contains("updateProjectV2(input:")
    }));
    graph.0.lock().unwrap().title = "Manual rename".into();
    forge.mirror_project(&repo()).await.unwrap();
    forge.mirror_project(&repo()).await.unwrap();
    {
        let fixture = graph.0.lock().unwrap();
        assert_eq!(fixture.title, "dvb: example-org/project");
        assert_eq!(
            fixture
                .writes
                .iter()
                .filter(|w| w["query"]
                    .as_str()
                    .unwrap()
                    .contains("updateProjectV2(input:"))
                .count(),
            1
        );
    }
}

#[tokio::test]
async fn socket_milestone_creation_retries_by_marker_then_rewrites_and_closes() {
    let server = MockServer::start().await;
    let forge = gateway(&server).await;
    metadata(&server).await;
    Mock::given(method("GET"))
        .and(path("/repos/example-org/project/milestones"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!([{"number":3,"description":"<!-- dvb goal g1 -->\nEdited manually"}]),
        ))
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("PATCH"))
        .and(path("/repos/example-org/project/milestones/3"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"number":3})))
        .expect(2)
        .mount(&server)
        .await;
    assert_eq!(forge.mirror_goal(&repo(), &goal()).await.unwrap(), 3);
    let mut done = goal();
    done.done = true;
    done.description = "Both PRs merged".into();
    assert_eq!(forge.mirror_goal(&repo(), &done).await.unwrap(), 3);
    let requests = server.received_requests().await.unwrap();
    let last = requests.iter().rev().find(|r| r.method == "PATCH").unwrap();
    let body: Value = serde_json::from_slice(&last.body).unwrap();
    assert_eq!(body["state"], "closed");
    assert_eq!(body["description"], "<!-- dvb goal g1 -->\nBoth PRs merged");
    let mint = requests
        .iter()
        .filter(|r| r.url.path().ends_with("access_tokens"))
        .map(|r| r.body_json::<Value>().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(mint, vec![json!({"repositories":["project"]})]);
}
#[tokio::test]
async fn socket_board_replaces_draft_with_pr_recovers_delete_and_overwrites_manual_state() {
    let server = MockServer::start().await;
    let forge = gateway(&server).await;
    let graph = mount_graph(&server, BoardFixture::default()).await;

    metadata(&server).await;
    mount_pr(&server).await;
    forge.mirror_project(&repo()).await.unwrap();
    forge.mirror_task(&repo(), &task()).await.unwrap();
    {
        let mut fixture = graph.0.lock().unwrap();
        assert_eq!(fixture.items.len(), 1);
        fixture.items[0]["content"]["body"] = json!("Hand edited body and removed marker");
        fixture.items[0]["content"]["title"] = json!("Manual title");
    }
    forge.mirror_task(&repo(), &task()).await.unwrap(); // Receipt repairs the removed marker too.
    assert_eq!(
        graph.0.lock().unwrap().items[0]["content"]["title"],
        "Build feature"
    );
    let mut pr = task();
    pr.pr = Some(23);
    pr.state = BoardState::InReview;
    graph.0.lock().unwrap().fail_delete = true;
    assert!(forge.mirror_task(&repo(), &pr).await.is_err());
    forge.mirror_task(&repo(), &pr).await.unwrap();
    pr.state = BoardState::Done;
    forge.mirror_task(&repo(), &pr).await.unwrap();
    {
        let state = graph.0.lock().unwrap();
        assert_eq!(state.items.len(), 1);
        assert_eq!(state.items[0]["id"], "PR-CARD");
        assert_eq!(
            state
                .writes
                .iter()
                .filter(|v| v["query"]
                    .as_str()
                    .unwrap()
                    .contains("addProjectV2ItemById"))
                .count(),
            1
        );
        assert!(state.writes.iter().any(
            |v| v["variables"]["input"]["value"]["singleSelectOptionId"] == "OPTION-In review"
        ));
        assert!(
            state
                .writes
                .iter()
                .any(|v| v["variables"]["input"]["value"]["singleSelectOptionId"] == "OPTION-Done")
        );
        drop(state);
    }
    let requests = server.received_requests().await.unwrap();
    assert!(
        !requests
            .iter()
            .any(|r| r.method == "POST" && r.url.path().ends_with("/issues"))
    );
    // REST and board operations share the repository's one token.
    let mint = requests
        .iter()
        .filter(|r| r.url.path().ends_with("access_tokens"))
        .map(|r| r.body_json::<Value>().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(mint, vec![json!({"repositories":["project"]})]);
    assert!(requests.iter().any(|r| r.url.path() == "/graphql"));
    assert!(
        requests
            .iter()
            .filter(|request| request.url.path() == "/graphql")
            .all(|request| request.headers["authorization"]
                == "Bearer recorded-installation-token")
    );
    assert!(
        requests
            .iter()
            .filter(|r| r.method == "PATCH" && r.url.path().ends_with("/issues/23"))
            .all(|r| serde_json::from_slice::<Value>(&r.body).unwrap()["milestone"] == 3)
    );
}

#[tokio::test]
async fn socket_archived_cards_recover_by_marker_and_receipt_without_duplicates() {
    let server = MockServer::start().await;
    let forge = gateway(&server).await;
    metadata(&server).await;
    let graph = mount_graph(
        &server,
        BoardFixture {
            board: true,
            items: vec![json!({
                "id":"DRAFT-CARD",
                "isArchived":true,
                "content":{
                    "id":"DRAFT",
                    "body":"<!-- dvb task t1 -->\nManual edit",
                    "title":"Manual title"
                }
            })],
            ..BoardFixture::default()
        },
    )
    .await;

    mount_pr(&server).await;

    // No local receipt: recover the archived draft by its marker.
    forge.mirror_task(&repo(), &task()).await.unwrap();
    {
        let mut fixture = graph.0.lock().unwrap();
        assert_eq!(fixture.items.len(), 1);
        assert_eq!(fixture.items[0]["id"], "DRAFT-CARD");
        assert_eq!(fixture.items[0]["isArchived"], false);
        assert_eq!(fixture.items[0]["content"]["title"], "Build feature");
        fixture.items[0]["isArchived"] = json!(true);
        fixture.items[0]["content"]["body"] = json!("Removed marker");
    }
    // The receipt also recovers an archived draft whose marker was removed.
    forge.mirror_task(&repo(), &task()).await.unwrap();
    {
        let mut fixture = graph.0.lock().unwrap();
        assert_eq!(fixture.items.len(), 1);
        assert_eq!(fixture.items[0]["isArchived"], false);
        assert!(
            fixture.items[0]["content"]["body"]
                .as_str()
                .unwrap()
                .starts_with("<!-- dvb task t1 -->")
        );
        fixture.items[0]["isArchived"] = json!(true);
        fixture.fail_delete = true;
    }
    let mut pr = task();
    pr.pr = Some(23);
    pr.state = BoardState::InReview;
    assert!(forge.mirror_task(&repo(), &pr).await.is_err());
    {
        let mut fixture = graph.0.lock().unwrap();
        assert_eq!(fixture.items.len(), 2);
        fixture
            .items
            .iter_mut()
            .find(|i| i["id"] == "PR-CARD")
            .unwrap()["isArchived"] = json!(true);
    }
    // A retry finds the archived PR and removes the archived draft after repair.
    forge.mirror_task(&repo(), &pr).await.unwrap();
    let fixture = graph.0.lock().unwrap();
    assert_eq!(fixture.items.len(), 1);
    assert_eq!(fixture.items[0]["id"], "PR-CARD");
    assert_eq!(fixture.items[0]["isArchived"], false);
    for (operation, count) in [
        ("addProjectV2DraftIssue", 0),
        ("addProjectV2ItemById", 1),
        ("unarchiveProjectV2Item", 3),
    ] {
        assert_eq!(
            fixture
                .writes
                .iter()
                .filter(|w| w["query"].as_str().unwrap().contains(operation))
                .count(),
            count,
            "{operation}"
        );
    }
}
#[tokio::test]
async fn socket_card_receipt_avoids_scans_and_prepared_fields_are_reused() {
    let server = MockServer::start().await;
    let forge = gateway(&server).await;
    let graph = mount_graph(&server, BoardFixture::default()).await;
    metadata(&server).await;
    forge.mirror_project(&repo()).await.unwrap();
    forge.mirror_task(&repo(), &task()).await.unwrap();
    forge.mirror_task(&repo(), &task()).await.unwrap();
    let queries = server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.url.path() == "/graphql")
        .map(|r| {
            serde_json::from_slice::<Value>(&r.body).unwrap()["query"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        queries
            .iter()
            .filter(|q| q.contains("items(first:"))
            .count(),
        1
    );
    assert_eq!(
        queries
            .iter()
            .filter(|q| q.contains("fields(first:"))
            .count(),
        3
    );
    assert_eq!(graph.0.lock().unwrap().items.len(), 1);
}

#[tokio::test]
async fn socket_card_receipt_requires_confirmed_deletion_before_recovery() {
    let server = MockServer::start().await;
    let forge = gateway(&server).await;
    let graph = mount_graph(&server, BoardFixture::default()).await;
    metadata(&server).await;
    forge.mirror_task(&repo(), &task()).await.unwrap();
    for response in [
        json!({"data":{"node":null},"errors":[{"type":"FORBIDDEN","path":["node"]}]}),
        json!({"data":{}}),
        json!({"data":{"node":{"id":"FOREIGN","isArchived":false,"project":{"id":"BOARD"}}}}),
    ] {
        graph.0.lock().unwrap().item_response = Some(response);
        assert!(forge.mirror_task(&repo(), &task()).await.is_err());
    }
    {
        let mut state = graph.0.lock().unwrap();
        state.items.clear();
        state.item_response =
            Some(json!({"data":{"node":null},"errors":[{"type":"NOT_FOUND","path":["node"]}]}));
    }
    forge.mirror_task(&repo(), &task()).await.unwrap();
    graph.0.lock().unwrap().item_response = None;
    forge.mirror_task(&repo(), &task()).await.unwrap();
    let queries = server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.url.path() == "/graphql")
        .map(|r| {
            serde_json::from_slice::<Value>(&r.body).unwrap()["query"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        queries
            .iter()
            .filter(|q| q.contains("items(first:"))
            .count(),
        2
    );
    assert_eq!(
        queries
            .iter()
            .filter(|q| q.contains("fields(first:"))
            .count(),
        3
    );
    assert_eq!(graph.0.lock().unwrap().items.len(), 1);
}

#[tokio::test]
async fn socket_prepared_fields_are_reused_across_card_updates() {
    let server = MockServer::start().await;
    let forge = gateway(&server).await;
    mount_graph(&server, BoardFixture::default()).await;
    metadata(&server).await;
    forge.mirror_project(&repo()).await.unwrap();
    for _ in 0..2 {
        forge.mirror_task(&repo(), &task()).await.unwrap();
    }
    let field_reads = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|request| {
            request.url.path() == "/graphql"
                && serde_json::from_slice::<Value>(&request.body).unwrap()["query"]
                    .as_str()
                    .unwrap()
                    .contains("fields(first:")
        })
        .count();
    assert_eq!(field_reads, 3);
}
