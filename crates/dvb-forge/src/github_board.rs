//! One-way Projects v2 mirror. Queries recover object identities, never engine state.
use crate::github::{GitHub, MirrorIds, api_error, error_word, refused, text};
use dvb_core::api::GithubAccountType;
use dvb_core::forge::{BoardState, ForgeError, GoalMirror, Repo, TaskMirror};
use serde_json::{Value, json};

const BOARD_QUERY: &str = "query($id:ID!){node(id:$id){... on ProjectV2{id title}}}";

const BOARDS_QUERY: &str = concat!(
    "query($org:String!,$after:String){organization(login:$org){id projectsV2(first:100,",
    "after:$after){nodes{id title} pageInfo{hasNextPage endCursor}}}}",
);

const CREATE_BOARD: &str = concat!(
    "mutation($owner:ID!,$title:String!){createProjectV2(input:{ownerId:$owner,title:$title}){",
    "projectV2{id title}}}",
);

const UPDATE_BOARD: &str = concat!(
    "mutation($id:ID!,$title:String!){updateProjectV2(input:{projectId:$id,title:$title}){",
    "projectV2{id}}}",
);

const LINK_BOARD: &str = concat!(
    "mutation($project:ID!,$repo:ID!){linkProjectV2ToRepository(input:{projectId:$project,",
    "repositoryId:$repo}){repository{id}}}",
);

const BOARD_FIELDS_QUERY: &str = concat!(
    "query($id:ID!){node(id:$id){... on ProjectV2{fields(first:100){nodes{... on ",
    "ProjectV2Field{id name dataType} ... on ProjectV2SingleSelectField{id name options{id ",
    "name}}}}}}}",
);

const UPDATE_FIELD: &str = concat!(
    "mutation($input:UpdateProjectV2FieldInput!){updateProjectV2Field(input:$input){",
    "projectV2Field{... on ProjectV2SingleSelectField{id name options{id name}}}}}",
);

const CREATE_FIELD: &str = concat!(
    "mutation($input:CreateProjectV2FieldInput!){createProjectV2Field(input:$input){",
    "projectV2Field{... on ProjectV2Field{id name dataType} ... on ProjectV2SingleSelectField{",
    "id name options{id name}}}}}",
);

const ITEM_QUERY: &str = concat!(
    "query($id:ID!){node(id:$id){... on ProjectV2Item{id isArchived project{id} content{... on ",
    "DraftIssue{id body} ... on PullRequest{id}}}}}",
);

const BOARD_ITEMS_QUERY: &str = concat!(
    "query($id:ID!,$after:String){node(id:$id){... on ProjectV2{items(first:100,after:$after,",
    "archivedStates:[ARCHIVED,NOT_ARCHIVED]){nodes{id isArchived content{... on DraftIssue{id ",
    "body} ... on PullRequest{id}}} pageInfo{hasNextPage endCursor}}}}}",
);

const ADD_ITEM: &str = concat!(
    "mutation($board:ID!,$content:ID!){addProjectV2ItemById(input:{projectId:$board,",
    "contentId:$content}){item{id}}}",
);

const UPDATE_DRAFT: &str = concat!(
    "mutation($id:ID!,$title:String!,$body:String!){updateProjectV2DraftIssue(input:{",
    "draftIssueId:$id,title:$title,body:$body}){draftIssue{id}}}",
);

const ADD_DRAFT: &str = concat!(
    "mutation($board:ID!,$title:String!,$body:String!){addProjectV2DraftIssue(input:{",
    "projectId:$board,title:$title,body:$body}){projectItem{id}}}",
);

const UPDATE_ITEM_FIELD: &str = concat!(
    "mutation($input:UpdateProjectV2ItemFieldValueInput!){updateProjectV2ItemFieldValue(",
    "input:$input){projectV2Item{id}}}",
);

const DELETE_ITEM: &str = concat!(
    "mutation($board:ID!,$item:ID!){deleteProjectV2Item(input:{projectId:$board,itemId:$item}){",
    "deletedItemId}}",
);

const UNARCHIVE_ITEM: &str = concat!(
    "mutation($board:ID!,$item:ID!){unarchiveProjectV2Item(input:{projectId:$board,",
    "itemId:$item}){item{id}}}",
);

const STATES: [BoardState; 9] = [
    BoardState::Planned,
    BoardState::Working,
    BoardState::InReview,
    BoardState::ReadyToMerge,
    BoardState::Done,
    BoardState::Stopped,
    BoardState::Cancelled,
    BoardState::Failed,
    BoardState::Unknown,
];

fn graphql_data(mut response: Value) -> Result<Value, ForgeError> {
    if response.get("errors").is_some() || !response["data"].is_object() {
        let codes: Vec<String> = response["errors"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|error| error.is_object())
            .map(|error| {
                // Only plain-word codes and the root field: later path segments can hold
                // submitted data. The message is clipped.
                let kind = match error_word(&error["type"]) {
                    "?" => error_word(&error["extensions"]["code"]),
                    kind => kind,
                };
                let field = error_word(&error["path"][0]);
                match error["message"].as_str() {
                    Some(message) => {
                        let message: String = message
                            .chars()
                            .filter(|c| !c.is_control())
                            .take(200)
                            .collect();
                        format!("{field} {kind}: {message}")
                    }
                    None => format!("{field} {kind}"),
                }
            })
            .collect();
        let detail = if codes.is_empty() {
            "GitHub board response is unreadable".into()
        } else {
            format!("GitHub board response is unreadable ({})", codes.join(", "))
        };
        return Err(refused(detail));
    }
    Ok(response["data"].take())
}

// Only an explicit null node or exclusively NOT_FOUND errors for that node
// permit forgetting a receipt. Permission and malformed responses still refuse.
fn board_node_data(response: Value) -> Result<Value, ForgeError> {
    if response["errors"].as_array().is_some_and(|errors| {
        !errors.is_empty()
            && errors
                .iter()
                .all(|error| error["type"] == "NOT_FOUND" && error["path"] == json!(["node"]))
    }) {
        return Ok(json!({"node":null}));
    }
    graphql_data(response)
}

fn forget_board(ids: &mut MirrorIds, repo: &str) {
    if let Some(board) = ids.boards.remove(repo) {
        ids.fields.remove(&board);
    }
    ids.forbidden_board_links.remove(repo);
    let prefix = format!("{repo}:");
    ids.cards.retain(|key, _| !key.starts_with(&prefix));
}

// A denied convenience link must not block card mirroring. Only this exact
// error is tolerated; mixed, malformed and unrelated errors still refuse.
async fn link_board<F, Fut>(
    board: &str,
    forbidden_receipt: Option<&str>,
    request: F,
) -> Result<bool, ForgeError>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<Value, ForgeError>>,
{
    if forbidden_receipt == Some(board) {
        return Ok(true);
    }
    let response = request().await?;
    if response["errors"].as_array().is_some_and(|errors| {
        !errors.is_empty()
            && errors.iter().all(|error| {
                error["type"] == "FORBIDDEN"
                    && error["path"]
                        .as_array()
                        .and_then(|path| path.first())
                        .and_then(Value::as_str)
                        == Some("linkProjectV2ToRepository")
            })
    }) {
        return Ok(true);
    }
    graphql_data(response)?;
    Ok(false)
}

enum MilestoneWriteError {
    AlreadyExists(ForgeError),
    Other(ForgeError),
}

impl MilestoneWriteError {
    fn into_forge(self) -> ForgeError {
        match self {
            Self::AlreadyExists(error) | Self::Other(error) => error,
        }
    }
}

fn milestone_already_exists(status: Option<u16>, errors: &[Value]) -> bool {
    status == Some(422)
        && errors.iter().any(|error| {
            error["resource"] == "Milestone"
                && error["field"] == "title"
                && error["code"] == "already_exists"
        })
}

fn milestone_write_error(error: crate::gateway::Error) -> MilestoneWriteError {
    let already_exists = if let Some(octocrab::Error::GitHub { source, .. }) = error.api() {
        milestone_already_exists(
            Some(source.status_code.as_u16()),
            source.errors.as_deref().unwrap_or_default(),
        )
    } else {
        false
    };
    if already_exists {
        MilestoneWriteError::AlreadyExists(api_error(error))
    } else {
        MilestoneWriteError::Other(api_error(error))
    }
}

fn milestone_number(row: &Value) -> Result<u64, ForgeError> {
    row["number"]
        .as_u64()
        .ok_or_else(|| refused("milestone response has no number"))
}

fn milestone_target(
    rows: &[Value],
    receipt: Option<u64>,
    claimed: &[u64],
    title: &str,
    disambiguated_title: &str,
    marker: &str,
) -> Result<(Option<u64>, String), ForgeError> {
    let available = |row: &Value| {
        !row["number"].as_u64().is_some_and(|n| claimed.contains(&n))
            && !row["description"].as_str().is_some_and(|description| {
                description.lines().any(|line| {
                    line.starts_with("<!-- dvb goal ") && line.ends_with(" -->") && line != marker
                })
            })
    };
    let title = if rows
        .iter()
        .any(|row| row["title"] == title && !available(row))
    {
        disambiguated_title
    } else {
        title
    };
    if rows
        .iter()
        .any(|row| row["title"] == title && !available(row))
    {
        return Err(refused(
            "disambiguated milestone title belongs to another goal",
        ));
    }
    // Receipts and markers keep identity through renames, but neither may
    // override another goal's marker or receipt.
    let existing = rows
        .iter()
        .find(|row| available(row) && receipt.is_some() && row["number"].as_u64() == receipt)
        .or_else(|| {
            rows.iter().find(|row| {
                available(row)
                    && row["description"]
                        .as_str()
                        .is_some_and(|s| s.lines().next() == Some(marker))
            })
        })
        .or_else(|| {
            rows.iter()
                .find(|row| row["title"] == title && available(row))
        })
        .map(milestone_number)
        .transpose()?;
    Ok((existing, title.to_owned()))
}

async fn sync_milestone<L, LFut, W, WFut>(
    receipt: Option<u64>,
    claimed: &[u64],
    title: &str,
    disambiguated_title: &str,
    marker: &str,
    mut list: L,
    mut write: W,
) -> Result<u64, ForgeError>
where
    L: FnMut() -> LFut,
    LFut: std::future::Future<Output = Result<Vec<Value>, ForgeError>>,
    W: FnMut(Option<u64>, String) -> WFut,
    WFut: std::future::Future<Output = Result<Value, MilestoneWriteError>>,
{
    let mut rows = list().await?;
    // At most one create for each title, then one update after a create race.
    for attempt in 0..3 {
        let (existing, write_title) =
            milestone_target(&rows, receipt, claimed, title, disambiguated_title, marker)?;
        match write(existing, write_title.clone()).await {
            Ok(response) => return milestone_number(&response),
            Err(MilestoneWriteError::AlreadyExists(error)) if existing.is_none() && attempt < 2 => {
                rows = list().await?;
                if !rows.iter().any(|row| row["title"] == write_title) {
                    return Err(error);
                }
            }
            Err(error) => return Err(error.into_forge()),
        }
    }
    unreachable!("the final write returns its result")
}

/// Read a GraphQL connection page by page, up to 1000 pages, until `visit` says
/// stop. `read` takes the `after` cursor and returns a page's data; `connection`
/// is the path to the paged connection in it. `visit` sees the data and the nodes.
async fn graphql_pages<R, Fut, V>(
    mut read: R,
    connection: [&str; 2],
    mut visit: V,
) -> Result<(), ForgeError>
where
    R: FnMut(Value) -> Fut,
    Fut: std::future::Future<Output = Result<Value, ForgeError>>,
    V: FnMut(&Value, &[Value]) -> Result<bool, ForgeError>,
{
    let mut after = Value::Null;
    for _ in 0..1000 {
        let data = read(after).await?;
        let page = &data[connection[0]][connection[1]];
        let nodes = page["nodes"]
            .as_array()
            .ok_or_else(|| refused("board list is unreadable"))?;
        if visit(&data, nodes)? || page["pageInfo"]["hasNextPage"] != true {
            return Ok(());
        }
        after = page["pageInfo"]["endCursor"].clone();
        if !after.is_string() {
            return Err(refused("board pagination is unreadable"));
        }
    }
    Err(refused("board pagination limit exceeded"))
}

// The receipt identifies a board even after a manual rename.
async fn resolve_board<F, Fut, D>(
    org: &str,
    title: &str,
    receipt: Option<String>,
    mut graphql: F,
    mut deleted: D,
) -> Result<String, ForgeError>
where
    F: FnMut(&'static str, Value) -> Fut,
    Fut: std::future::Future<Output = Result<Value, ForgeError>>,
    D: FnMut() -> Result<(), ForgeError>,
{
    let mut found = None;
    if let Some(id) = receipt {
        let v = graphql(BOARD_QUERY, json!({"id":id})).await?;
        if v.get("node") == Some(&Value::Null) {
            deleted()?;
        } else {
            found = Some(v["node"].clone());
        }
    }
    let found = if found.is_some() {
        found
    } else {
        let mut owner = String::new();
        let mut found = None;
        graphql_pages(
            |after| graphql(BOARDS_QUERY, json!({"org":org,"after":after})),
            ["organization", "projectsV2"],
            |v, rows| {
                owner = text(&v["organization"], "id")?.into();
                found = rows.iter().find(|p| p["title"] == title).cloned();
                Ok(found.is_some())
            },
        )
        .await?;
        if found.is_none() {
            let v = graphql(CREATE_BOARD, json!({"owner":owner,"title":title})).await?;
            found = Some(v["createProjectV2"]["projectV2"].clone());
        }
        found
    };
    let board = found.ok_or_else(|| refused("board response is unreadable"))?;
    let id = text(&board, "id")?.to_owned();
    if text(&board, "title")? != title {
        graphql(UPDATE_BOARD, json!({"id":id,"title":title})).await?;
    }
    Ok(id)
}

impl GitHub {
    fn with_ids<T>(&self, f: impl FnOnce(&mut MirrorIds) -> T) -> T {
        f(&mut self
            .mirror_ids
            .lock()
            .unwrap_or_else(dvb_core::sync::recover))
    }

    /// Change the receipts and save them before another change can interleave.
    fn save_ids(&self, f: impl FnOnce(&mut MirrorIds)) -> Result<(), ForgeError> {
        self.with_ids(|ids| {
            f(ids);
            match &self.state_dir {
                Some(state) => crate::github_setup::write_private(
                    state,
                    "github-mirror.json",
                    &serde_json::to_vec(ids)
                        .map_err(|_| refused("cannot encode mirror receipts"))?,
                ),
                None => Ok(()),
            }
        })
    }

    async fn graphql(
        &self,
        repo: &Repo,
        query: &str,
        variables: Value,
    ) -> Result<Value, ForgeError> {
        graphql_data(self.graphql_response(repo, query, variables).await?)
    }

    /// An App on a personal account gets no projects permission, so only an
    /// organization has a board; a user's goals and PRs still get milestones.
    fn has_board(&self) -> bool {
        match self.account {
            GithubAccountType::Organization => true,
            GithubAccountType::User => false,
        }
    }

    async fn graphql_response(
        &self,
        repo: &Repo,
        query: &str,
        variables: Value,
    ) -> Result<Value, ForgeError> {
        let api = self.client(repo).await?;
        let v: Value = api
            .post(
                "/graphql",
                Some(&json!({"query":query,"variables":variables})),
            )
            .await
            .map_err(api_error)?;
        Ok(v)
    }

    pub(super) async fn sync_goal(
        &self,
        repo: &Repo,
        goal: &GoalMirror,
    ) -> Result<u64, ForgeError> {
        let marker = format!("<!-- dvb goal {} -->", goal.key);
        let description = format!("{marker}\n{}", goal.description);
        let api = self.client(repo).await?;
        let route = self.route(repo, "/milestones?state=all")?;
        let key = format!("{}:{}", repo.0, goal.key);
        let (receipt, claimed) = self.with_ids(|ids| {
            let repo_prefix = format!("{}:", repo.0);
            let claimed: Vec<u64> = ids
                .milestones
                .iter()
                .filter(|(owner, _)| owner.starts_with(&repo_prefix) && *owner != &key)
                .map(|(_, number)| *number)
                .collect();
            (ids.milestones.get(&key).copied(), claimed)
        });
        let short_id: String = goal
            .key
            .chars()
            .skip(goal.key.chars().count().saturating_sub(8))
            .collect();
        let disambiguated_title = format!("{} ({short_id})", goal.title);
        let milestone_route = self.route(repo, "/milestones")?;
        let number = sync_milestone(
            receipt,
            &claimed,
            &goal.title,
            &disambiguated_title,
            &marker,
            || self.list(&api, &route, None),
            |existing, title| {
                let api = &api;
                let description = &description;
                let route = milestone_route.clone();
                async move {
                    let body = json!({
                        "title":title,
                        "description":description,
                        "state":if goal.done {
                            "closed"
                        } else {
                            "open"
                        }
                    });
                    match existing {
                        Some(number) => api.patch(format!("{route}/{number}"), Some(&body)).await,
                        None => api.post(route, Some(&body)).await,
                    }
                    .map_err(milestone_write_error)
                }
            },
        )
        .await?;
        self.save_ids(|ids| {
            ids.milestones.insert(key, number);
        })?;
        Ok(number)
    }

    pub(super) async fn prepare_board(&self, repo: &Repo) -> Result<(), ForgeError> {
        if !self.has_board() {
            return Ok(());
        }
        let metadata = self.metadata(repo).await?;
        let board = self.board(repo).await?;
        let receipt = self.with_ids(|ids| ids.forbidden_board_links.get(&repo.0).cloned());
        if link_board(&board, receipt.as_deref(), || async {
            self.graphql_response(
                repo,
                LINK_BOARD,
                json!({"project":board,"repo":text(&metadata,"node_id")?}),
            )
            .await
        })
        .await?
        {
            self.save_ids(|ids| {
                ids.forbidden_board_links
                    .insert(repo.0.clone(), board.clone());
            })?;
        }
        self.with_ids(|ids| ids.fields.remove(&board));
        self.field(repo, &board, "Status", true).await?;
        self.field(repo, &board, "Role", false).await?;
        self.field(repo, &board, "Model", false).await?;
        Ok(())
    }
    async fn board(&self, repo: &Repo) -> Result<String, ForgeError> {
        let title = format!("dvb: {}", repo.0);
        let receipt = self.with_ids(|ids| ids.boards.get(&repo.0).cloned());
        let id = resolve_board(
            &self.org,
            &title,
            receipt,
            |query, variables| async move {
                let response = self.graphql_response(repo, query, variables).await?;
                if query == BOARD_QUERY {
                    board_node_data(response)
                } else {
                    graphql_data(response)
                }
            },
            || self.save_ids(|ids| forget_board(ids, &repo.0)),
        )
        .await?;
        self.save_ids(|ids| {
            ids.boards.insert(repo.0.clone(), id.clone());
        })?;
        Ok(id)
    }

    async fn field(
        &self,
        repo: &Repo,
        board: &str,
        name: &str,
        select: bool,
    ) -> Result<Value, ForgeError> {
        if let Some(field) =
            self.with_ids(|ids| ids.fields.get(board).and_then(|f| f.get(name)).cloned())
        {
            return Ok(field);
        }
        let cache = |field: Value| {
            self.with_ids(|ids| {
                ids.fields
                    .entry(board.into())
                    .or_default()
                    .insert(name.into(), field.clone());
            });
            Ok(field)
        };
        let v = self
            .graphql(repo, BOARD_FIELDS_QUERY, json!({"id":board}))
            .await?;
        let rows = v["node"]["fields"]["nodes"]
            .as_array()
            .ok_or_else(|| refused("board fields are unreadable"))?;
        if let Some(field) = rows.iter().find(|f| f["name"] == name) {
            if select && !field["options"].is_array() || !select && field["dataType"] != "TEXT" {
                return Err(refused("board field type changed"));
            }
            if select
                && STATES.iter().any(|s| {
                    !field["options"]
                        .as_array()
                        .is_some_and(|options| options.iter().any(|o| o["name"] == s.label()))
                })
            {
                let options =
                    STATES.map(|s| json!({"name":s.label(),"color":"GRAY","description":""}));
                let variables = json!({
                    "input": {"fieldId": text(field, "id")?, "singleSelectOptions": options}
                });
                let v = self.graphql(repo, UPDATE_FIELD, variables).await?;
                return cache(v["updateProjectV2Field"]["projectV2Field"].clone());
            }
            return cache(field.clone());
        }
        let mut input = json!({"projectId":board,"name":name,"dataType":if select {"SINGLE_SELECT"} else {"TEXT"}});
        if select {
            input["singleSelectOptions"] =
                json!(STATES.map(|s| json!({"name":s.label(),"color":"GRAY","description":""})));
        }
        let v = self
            .graphql(repo, CREATE_FIELD, json!({"input":input}))
            .await?;
        cache(v["createProjectV2Field"]["projectV2Field"].clone())
    }

    pub(super) async fn sync_task(&self, repo: &Repo, task: &TaskMirror) -> Result<(), ForgeError> {
        let marker = format!("<!-- dvb task {} -->", task.key);
        let body = format!("{marker}\nRole: {}\nModel: {}", task.role, task.model);
        let content = if let Some(pr) = task.pr {
            Some(sync_task_pr(&self.client(repo).await?, &self.route(repo, "")?, pr, task).await?)
        } else {
            None
        };
        if !self.has_board() {
            return Ok(());
        }
        let board = self.board(repo).await?;
        let key = format!("{}:{}", repo.0, task.key);
        let receipt = self.with_ids(|ids| ids.cards.get(&key).cloned());
        let mut drafts = Vec::new();
        let mut item = None;
        let mut usable_receipt = false;
        if let Some(id) = receipt.as_ref() {
            let response = self
                .graphql_response(repo, ITEM_QUERY, json!({"id":id}))
                .await?;
            let data = board_node_data(response)?;
            if data.get("node") == Some(&Value::Null) {
                self.save_ids(|ids| {
                    ids.cards.remove(&key);
                })?;
            } else {
                let row = data
                    .get("node")
                    .filter(|row| row.is_object())
                    .ok_or_else(|| refused("board item is unreadable"))?;
                if text(row, "id")? != id || !row["isArchived"].is_boolean() {
                    return Err(refused("board item identity is unreadable"));
                }
                if text(&row["project"], "id")? == board {
                    let c = &row["content"];
                    if c["body"].is_string() {
                        drafts.push((id.clone(), text(c, "id")?.into(), row["isArchived"] == true));
                        usable_receipt = content.is_none();
                    } else if content
                        .as_ref()
                        .is_some_and(|expected| c["id"] == *expected)
                    {
                        item = Some((id.clone(), row["isArchived"] == true));
                        usable_receipt = true;
                    }
                }
            }
        }
        if !usable_receipt {
            // A draft receipt still needs a scan when converting to a PR: a lost
            // add/delete reply may have left both cards on the board.
            drafts.clear();
            graphql_pages(
                |after| self.graphql(repo, BOARD_ITEMS_QUERY, json!({"id":board,"after":after})),
                ["node", "items"],
                |_, rows| {
                    for row in rows {
                        let c = &row["content"];
                        if (c["body"].is_string()
                            && receipt.as_ref().is_some_and(|id| row["id"] == *id))
                            || c["body"]
                                .as_str()
                                .is_some_and(|s| s.lines().next() == Some(marker.as_str()))
                        {
                            drafts.push((
                                text(row, "id")?.to_owned(),
                                text(c, "id")?.to_owned(),
                                row["isArchived"] == true,
                            ));
                        }
                        if content.as_ref().is_some_and(|id| c["id"] == *id) {
                            item = Some((text(row, "id")?.to_owned(), row["isArchived"] == true));
                        }
                    }
                    Ok(false)
                },
            )
            .await?;
        }
        let item = if let Some(content) = content {
            match item {
                Some((id, archived)) => {
                    if archived {
                        self.unarchive_item(repo, &board, &id).await?;
                    }
                    id
                }
                None => {
                    let v = self
                        .graphql(repo, ADD_ITEM, json!({"board":board,"content":content}))
                        .await?;
                    text(&v["addProjectV2ItemById"]["item"], "id")?.to_owned()
                }
            }
        } else if let Some((id, draft, archived)) = drafts.first() {
            if *archived {
                self.unarchive_item(repo, &board, id).await?;
            }
            self.graphql(
                repo,
                UPDATE_DRAFT,
                json!({"id":draft,"title":task.title,"body":body}),
            )
            .await?;
            id.clone()
        } else {
            let v = self
                .graphql(
                    repo,
                    ADD_DRAFT,
                    json!({"board":board,"title":task.title,"body":body}),
                )
                .await?;
            text(&v["addProjectV2DraftIssue"]["projectItem"], "id")?.to_owned()
        };
        for (field_name, value, select) in [
            ("Status", task.state.label(), true),
            ("Role", task.role.as_str(), false),
            ("Model", task.model.as_str(), false),
        ] {
            let field = self.field(repo, &board, field_name, select).await?;
            let val = if select {
                let option = field["options"]
                    .as_array()
                    .and_then(|a| a.iter().find(|o| o["name"] == value))
                    .ok_or_else(|| refused("board state option is missing"))?;
                json!({"singleSelectOptionId":text(option,"id")?})
            } else {
                json!({"text":value})
            };
            let variables = json!({
                "input": {
                    "projectId": board, "itemId": item, "fieldId": text(&field, "id")?, "value": val
                }
            });
            self.graphql(repo, UPDATE_ITEM_FIELD, variables).await?;
        }
        // Add/repair the PR first; a lost delete reply is harmless on the next pass.
        for (draft, _, _) in drafts.into_iter().filter(|(id, _, _)| id != &item) {
            self.graphql(repo, DELETE_ITEM, json!({"board":board,"item":draft}))
                .await?;
        }
        self.save_ids(|ids| {
            ids.cards.insert(key, item);
        })?;
        Ok(())
    }

    async fn unarchive_item(&self, repo: &Repo, board: &str, item: &str) -> Result<(), ForgeError> {
        self.graphql(repo, UNARCHIVE_ITEM, json!({"board":board,"item":item}))
            .await?;
        Ok(())
    }
}

async fn sync_task_pr(
    api: &crate::gateway::Client,
    route: &str,
    pr: u64,
    task: &TaskMirror,
) -> Result<String, ForgeError> {
    let v = api
        .get(format!("{route}/pulls/{pr}"))
        .await
        .map_err(api_error)?;
    let id = text(&v, "node_id")?.to_owned();
    api.patch(
        format!("{route}/issues/{pr}"),
        Some(&json!({"milestone":task.milestone,"title":task.title})),
    )
    .await
    .map_err(api_error)?;
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dvb_core::forge::ForgeError;
    use serde_json::{Value, json};

    #[tokio::test]
    async fn pr_title_and_milestone_edit_reads_the_node_and_patches_the_issue() {
        use bytes::Bytes;
        use http::{Method, Request, Response};
        use http_body_util::{BodyExt, Full};
        use std::sync::{Arc, Mutex};

        let calls = Arc::new(Mutex::new(Vec::new()));
        let requests = calls.clone();
        let service = tower::service_fn(move |request: Request<octocrab::OctoBody>| {
            let requests = requests.clone();
            async move {
                let (parts, body) = request.into_parts();
                let body = body.collect().await.unwrap().to_bytes();
                requests
                    .lock()
                    .unwrap()
                    .push((parts.method.clone(), parts.uri.path().to_owned()));
                let (status, response) = match (parts.method, parts.uri.path()) {
                    (Method::GET, "/repos/org/repo/pulls/23") => (200, json!({"node_id":"PR-23"})),
                    (Method::PATCH, "/repos/org/repo/issues/23") => {
                        assert_eq!(
                            serde_json::from_slice::<Value>(&body).unwrap(),
                            json!({"title":"Build feature","milestone":3})
                        );
                        (200, json!({}))
                    }
                    request => panic!("unexpected request: {request:?}"),
                };
                Ok::<_, std::convert::Infallible>(
                    Response::builder()
                        .status(status)
                        .body(Full::new(Bytes::from(response.to_string())))
                        .unwrap(),
                )
            }
        });
        let api = octocrab::OctocrabBuilder::new_empty()
            .with_service(service)
            .with_auth(octocrab::AuthState::None)
            .build()
            .unwrap();
        let task = TaskMirror {
            key: "task".into(),
            title: "Build feature".into(),
            role: "Backend builder".into(),
            model: "fixture-model".into(),
            state: BoardState::Done,
            pr: Some(23),
            milestone: 3,
        };
        let api = crate::gateway::Client {
            api,
            wait: Arc::new(crate::gateway::Wait::default()),
        };
        let id = sync_task_pr(&api, "/repos/org/repo", 23, &task)
            .await
            .unwrap();
        assert_eq!(id, "PR-23");
        assert_eq!(
            *calls.lock().unwrap(),
            vec![
                (Method::GET, "/repos/org/repo/pulls/23".into()),
                (Method::PATCH, "/repos/org/repo/issues/23".into()),
            ]
        );
    }

    fn forbidden_link() -> Value {
        json!({"data":{"linkProjectV2ToRepository":null},"errors":[{
            "type":"FORBIDDEN","path":["linkProjectV2ToRepository"],"message":"private-data"
        }]})
    }

    #[tokio::test]
    async fn forbidden_link_is_remembered_across_restart_and_scoped_to_board() {
        let denied = link_board("BOARD", None, || std::future::ready(Ok(forbidden_link())))
            .await
            .unwrap();
        assert!(denied);
        let mut ids = crate::github::MirrorIds::default();
        ids.forbidden_board_links
            .insert("example-org/project".into(), "BOARD".into());
        let restored: crate::github::MirrorIds =
            serde_json::from_slice(&serde_json::to_vec(&ids).unwrap()).unwrap();
        let receipt = restored
            .forbidden_board_links
            .get("example-org/project")
            .map(String::as_str);
        assert!(
            link_board("BOARD", receipt, || async {
                panic!("retried forbidden link")
            })
            .await
            .unwrap()
        );
        assert!(
            !link_board("NEW_BOARD", receipt, || std::future::ready(Ok(
                json!({"data":{"linkProjectV2ToRepository":{"repository":{"id":"REPO"}}}})
            )))
            .await
            .unwrap()
        );
        let old: crate::github::MirrorIds = serde_json::from_value(json!({
            "boards":{"example-org/project":"BOARD"},"cards":{},"milestones":{}
        }))
        .unwrap();
        assert!(old.forbidden_board_links.is_empty());
    }

    #[tokio::test]
    async fn link_errors_other_than_the_exact_forbidden_operation_still_fail() {
        for response in [
            json!({"errors":[{"type":"INSUFFICIENT_SCOPES","path":["linkProjectV2ToRepository"]}]}),
            json!({"errors":[{"type":"FORBIDDEN","path":["updateProjectV2"]}]}),
            json!({"errors":[{"type":"FORBIDDEN"}]}),
            json!({"errors":[{"type":"FORBIDDEN","path":["linkProjectV2ToRepository"]},{}]}),
            json!({"errors":[]}),
            json!({}),
        ] {
            assert!(
                link_board("BOARD", None, || std::future::ready(Ok(response)))
                    .await
                    .is_err()
            );
        }
        let error = refused("transport failure");
        assert_eq!(
            link_board("BOARD", None, || std::future::ready(Err(error)))
                .await
                .unwrap_err()
                .detail,
            "transport failure"
        );
    }

    async fn recorded_milestone(
        receipt: Option<u64>,
        lists: Vec<Vec<Value>>,
        writes: Vec<Result<Value, MilestoneWriteError>>,
    ) -> (Result<u64, ForgeError>, Vec<Option<u64>>) {
        let (result, calls) = recorded_milestone_for(receipt, &[], "goal-1", lists, writes).await;
        (
            result,
            calls.into_iter().map(|(number, _)| number).collect(),
        )
    }

    async fn recorded_milestone_for(
        receipt: Option<u64>,
        claimed: &[u64],
        goal_key: &str,
        lists: Vec<Vec<Value>>,
        writes: Vec<Result<Value, MilestoneWriteError>>,
    ) -> (Result<u64, ForgeError>, Vec<(Option<u64>, String)>) {
        let mut lists = std::collections::VecDeque::from(lists);
        let mut writes = std::collections::VecDeque::from(writes);
        let mut calls = Vec::new();
        let result = sync_milestone(
            receipt,
            claimed,
            "Goal",
            &format!("Goal ({goal_key})"),
            &format!("<!-- dvb goal {goal_key} -->"),
            || std::future::ready(Ok(lists.pop_front().expect("unexpected list"))),
            |number, title| {
                calls.push((number, title));
                std::future::ready(writes.pop_front().expect("unexpected write"))
            },
        )
        .await;
        assert!(lists.is_empty());
        assert!(writes.is_empty());
        (result, calls)
    }

    #[tokio::test]
    async fn milestones_reuse_exact_titles_in_open_and_closed_rows() {
        for state in ["open", "closed"] {
            let (result, calls) = recorded_milestone(
                None,
                vec![vec![
                    json!({"number":1,"title":"goal","state":state}),
                    json!({"number":2,"title":"Goal extra","state":state}),
                    json!({"number":3,"title":"Goal","state":state,"description":"manual"}),
                ]],
                vec![Ok(json!({"number":3}))],
            )
            .await;
            assert_eq!(result.unwrap(), 3);
            assert_eq!(calls, vec![Some(3)]);
        }
    }

    #[tokio::test]
    async fn milestone_receipts_and_markers_keep_identity_after_renaming() {
        let (result, calls) = recorded_milestone(
            Some(7),
            vec![vec![json!({"number":7,"title":"Old title"})]],
            vec![Ok(json!({"number":7}))],
        )
        .await;
        assert_eq!(result.unwrap(), 7);
        assert_eq!(calls, vec![Some(7)]);
        let (result, calls) = recorded_milestone(None, vec![vec![
            json!({"number":1,"title":"Goal"}),
            json!({"number":7,"title":"Old title","description":"<!-- dvb goal goal-1 -->\nold"}),
        ]], vec![Ok(json!({"number":7}))]).await;
        assert_eq!(result.unwrap(), 7);
        assert_eq!(calls, vec![Some(7)]);
    }

    #[tokio::test]
    async fn shared_milestone_titles_create_a_separate_goal_milestone() {
        for state in ["open", "closed"] {
            for (description, claimed) in [
                ("<!-- dvb goal goal-1 -->\nfirst goal", vec![]),
                ("manually edited description", vec![7]),
                ("manual\n<!-- dvb goal goal-1 -->", vec![]),
            ] {
                let first =
                    json!({"number":7,"title":"Goal","description":description,"state":state});
                let (result, calls) = recorded_milestone_for(
                    None,
                    &claimed,
                    "goal-2",
                    vec![vec![first.clone()]],
                    vec![Ok(json!({"number":8}))],
                )
                .await;
                assert_eq!(result.unwrap(), 8);
                assert_eq!(calls, vec![(None, "Goal (goal-2)".into())]);

                // A saved receipt or recovered marker must keep using the
                // separate milestone and its title on subsequent syncs.
                for receipt in [Some(8), None] {
                    let (result, calls) = recorded_milestone_for(
                        receipt,
                        &claimed,
                        "goal-2",
                        vec![vec![
                            first.clone(),
                            json!({
                                "number":8,
                                "title":"Goal (goal-2)",
                                "description":"<!-- dvb goal goal-2 -->\nsecond goal"
                            }),
                        ]],
                        vec![Ok(json!({"number":8}))],
                    )
                    .await;
                    assert_eq!(result.unwrap(), 8);
                    assert_eq!(calls, vec![(Some(8), "Goal (goal-2)".into())]);
                }
            }
        }
    }

    #[tokio::test]
    async fn shared_milestone_title_races_create_a_separate_goal_milestone() {
        for (description, claimed) in [
            ("<!-- dvb goal goal-1 -->\nfirst goal", vec![]),
            ("manually edited description", vec![7]),
        ] {
            let first =
                json!({"number":7,"title":"Goal","description":description,"state":"closed"});
            let (result, calls) = recorded_milestone_for(
                None,
                &claimed,
                "goal-2",
                vec![vec![], vec![first]],
                vec![
                    Err(MilestoneWriteError::AlreadyExists(refused("duplicate"))),
                    Ok(json!({"number":8})),
                ],
            )
            .await;
            assert_eq!(result.unwrap(), 8);
            assert_eq!(
                calls,
                vec![(None, "Goal".into()), (None, "Goal (goal-2)".into())]
            );
        }
    }

    #[tokio::test]
    async fn disambiguated_milestone_creation_races_recover_this_goals_marker() {
        let first = json!({"number":7,"title":"Goal","description":"<!-- dvb goal goal-1 -->"});
        let second =
            json!({"number":8,"title":"Goal (goal-2)","description":"<!-- dvb goal goal-2 -->"});
        let (result, calls) = recorded_milestone_for(
            None,
            &[],
            "goal-2",
            vec![vec![], vec![first.clone()], vec![first, second]],
            vec![
                Err(MilestoneWriteError::AlreadyExists(refused("duplicate"))),
                Err(MilestoneWriteError::AlreadyExists(refused("duplicate"))),
                Ok(json!({"number":8})),
            ],
        )
        .await;
        assert_eq!(result.unwrap(), 8);
        assert_eq!(
            calls,
            vec![
                (None, "Goal".into()),
                (None, "Goal (goal-2)".into()),
                (Some(8), "Goal (goal-2)".into()),
            ]
        );
    }

    #[tokio::test]
    async fn milestones_reuse_this_goals_exact_title_after_a_race() {
        let row = json!({"number":7,"title":"Goal","description":"<!-- dvb goal goal-1 -->"});
        for race in [false, true] {
            let (lists, writes) = if race {
                (
                    vec![vec![], vec![row.clone()]],
                    vec![
                        Err(MilestoneWriteError::AlreadyExists(refused("duplicate"))),
                        Ok(json!({"number":7})),
                    ],
                )
            } else {
                (vec![vec![row.clone()]], vec![Ok(json!({"number":7}))])
            };
            let (result, calls) = recorded_milestone(None, lists, writes).await;
            assert_eq!(result.unwrap(), 7);
            assert_eq!(
                calls,
                if race {
                    vec![None, Some(7)]
                } else {
                    vec![Some(7)]
                }
            );
        }
    }

    #[tokio::test]
    async fn foreign_receipts_override_this_goals_marker_and_shared_receipt() {
        let (result, calls) = recorded_milestone_for(
            Some(7),
            &[7],
            "goal-2",
            vec![vec![
                json!({"number":7,"title":"Goal","description":"<!-- dvb goal goal-2 -->"}),
            ]],
            vec![Ok(json!({"number":8}))],
        )
        .await;
        assert_eq!(result.unwrap(), 8);
        assert_eq!(calls, vec![(None, "Goal (goal-2)".into())]);
    }

    #[tokio::test]
    async fn foreign_disambiguated_titles_are_never_overwritten() {
        let rows = vec![
            json!({"number":7,"title":"Goal","description":"<!-- dvb goal goal-1 -->"}),
            json!({"number":8,"title":"Goal (goal-2)","description":"<!-- dvb goal goal-3 -->"}),
        ];
        for race in [false, true] {
            let (lists, writes) = if race {
                (
                    vec![vec![rows[0].clone()], rows.clone()],
                    vec![Err(MilestoneWriteError::AlreadyExists(refused(
                        "duplicate",
                    )))],
                )
            } else {
                (vec![rows.clone()], vec![])
            };
            let (result, calls) = recorded_milestone_for(None, &[], "goal-2", lists, writes).await;
            assert!(result.is_err());
            assert_eq!(
                calls,
                if race {
                    vec![(None, "Goal (goal-2)".into())]
                } else {
                    vec![]
                }
            );
        }
    }

    #[tokio::test]
    async fn milestone_creation_races_lookup_and_update_the_existing_title() {
        let (result, calls) = recorded_milestone(
            None,
            vec![
                vec![],
                vec![json!({"number":9,"title":"Goal","state":"closed"})],
            ],
            vec![
                Err(MilestoneWriteError::AlreadyExists(refused("duplicate"))),
                Ok(json!({"number":9})),
            ],
        )
        .await;
        assert_eq!(result.unwrap(), 9);
        assert_eq!(calls, vec![None, Some(9)]);
        let (result, calls) =
            recorded_milestone(None, vec![vec![]], vec![Ok(json!({"number":10}))]).await;
        assert_eq!(result.unwrap(), 10);
        assert_eq!(calls, vec![None]);
    }

    #[tokio::test]
    async fn milestone_race_without_an_exact_title_and_other_failures_remain_errors() {
        let (result, calls) = recorded_milestone(
            None,
            vec![vec![], vec![json!({"number":9,"title":"Goal extra"})]],
            vec![Err(MilestoneWriteError::AlreadyExists(refused(
                "duplicate",
            )))],
        )
        .await;
        assert_eq!(result.unwrap_err().detail, "duplicate");
        assert_eq!(calls, vec![None]);
        let (result, _) = recorded_milestone(
            None,
            vec![vec![]],
            vec![Err(MilestoneWriteError::Other(refused("other validation")))],
        )
        .await;
        assert_eq!(result.unwrap_err().detail, "other validation");
    }

    #[test]
    fn milestone_race_requires_the_structured_title_conflict() {
        let conflict = json!({"resource":"Milestone","field":"title","code":"already_exists"});
        assert!(milestone_already_exists(
            Some(422),
            std::slice::from_ref(&conflict)
        ));
        for status in [None, Some(403), Some(500)] {
            assert!(!milestone_already_exists(
                status,
                std::slice::from_ref(&conflict)
            ));
        }
        for error in [
            json!({"resource":"Issue","field":"title","code":"already_exists"}),
            json!({"resource":"Milestone","field":"description","code":"already_exists"}),
            json!({"resource":"Milestone","field":"title","code":"invalid"}),
            json!({"message":"Milestone.title already_exists"}),
        ] {
            assert!(!milestone_already_exists(Some(422), &[error]));
        }
    }

    async fn recorded_board(
        receipt: Option<&str>,
        responses: Vec<Value>,
    ) -> (Result<String, ForgeError>, Vec<(&'static str, Value)>) {
        let expected_deletion = receipt.is_some()
            && responses.first().and_then(|v| v.get("node")) == Some(&Value::Null);
        let mut deleted = false;
        let mut responses = std::collections::VecDeque::from(responses);
        let mut calls = Vec::new();
        let result = resolve_board(
            "example-org",
            "dvb: example-org/project",
            receipt.map(str::to_owned),
            |query, variables| {
                calls.push((query, variables));
                std::future::ready(Ok(responses
                    .pop_front()
                    .expect("unexpected GraphQL request")))
            },
            || {
                assert!(!deleted, "receipt was discarded twice");
                deleted = true;
                Ok(())
            },
        )
        .await;
        assert_eq!(deleted, expected_deletion);
        assert!(responses.is_empty(), "missing GraphQL request");
        (result, calls)
    }

    fn board_list(board: Value) -> Value {
        let boards = vec![board];
        json!({"organization":{"id":"ORG","projectsV2":{"nodes":boards,"pageInfo":{"hasNextPage":false}}}})
    }

    #[tokio::test]
    async fn deleted_board_receipts_rediscover_or_create_boards() {
        let board = json!({"id":"NEW","title":"dvb: example-org/project"});
        for create in [false, true] {
            let mut responses = vec![json!({"node":null})];
            if create {
                responses.push(json!({"organization":{"id":"ORG","projectsV2":{
                    "nodes":[],"pageInfo":{"hasNextPage":false}}}}));
                responses.push(json!({"createProjectV2":{"projectV2":board}}));
            } else {
                responses.push(board_list(board.clone()));
            }
            let (result, calls) = recorded_board(Some("OLD"), responses).await;
            assert_eq!(result.unwrap(), "NEW");
            assert_eq!(calls.len(), if create { 3 } else { 2 });
        }
    }

    #[tokio::test]
    async fn deletion_is_recorded_before_a_failed_rediscovery() {
        let mut calls = 0;
        let mut deleted = false;
        let result = resolve_board(
            "example-org",
            "board",
            Some("OLD".into()),
            |_, _| {
                calls += 1;
                std::future::ready(if calls == 1 {
                    Ok(json!({"node":null}))
                } else {
                    Err(refused("discovery refused"))
                })
            },
            || {
                deleted = true;
                Ok(())
            },
        )
        .await;
        assert_eq!(result.unwrap_err().detail, "discovery refused");
        assert!(deleted);
        assert_eq!(calls, 2);
    }

    #[tokio::test]
    async fn malformed_node_is_refused_without_discarding_the_receipt() {
        let (result, calls) = recorded_board(Some("OLD"), vec![json!({})]).await;
        assert_eq!(result.unwrap_err().detail, "GitHub response missing id");
        assert_eq!(calls.len(), 1);
    }

    #[test]
    fn only_confirmed_missing_board_responses_allow_recovery() {
        for response in [
            json!({"data":{"node":null}}),
            json!({"data":{"node":null},"errors":[{"type":"NOT_FOUND","path":["node"]}]}),
        ] {
            assert_eq!(board_node_data(response).unwrap(), json!({"node":null}));
        }
        for errors in [
            json!([{"type":"FORBIDDEN","path":["node"]}]),
            json!([{"type":"NOT_FOUND","path":["node"]},{"type":"FORBIDDEN","path":["node"]}]),
            json!([{"type":"NOT_FOUND","path":["organization"]}]),
            json!([{"type":"NOT_FOUND"}]),
            json!([]),
        ] {
            assert!(board_node_data(json!({"data":{"node":null},"errors":errors})).is_err());
        }
    }

    #[test]
    fn forgetting_a_board_clears_only_its_dependent_receipts() {
        let mut ids: crate::github::MirrorIds = serde_json::from_value(json!({
            "boards":{"example-org/project":"OLD","example-org/other":"OTHER"},
            "cards":{"example-org/project:task":"CARD","example-org/other:task":"KEEP"},
            "milestones":{"example-org/project:goal":7},
            "forbidden_board_links":{"example-org/project":"OLD","example-org/other":"OTHER"}
        }))
        .unwrap();
        forget_board(&mut ids, "example-org/project");
        let restored: crate::github::MirrorIds =
            serde_json::from_slice(&serde_json::to_vec(&ids).unwrap()).unwrap();
        assert!(!restored.boards.contains_key("example-org/project"));
        assert!(
            !restored
                .forbidden_board_links
                .contains_key("example-org/project")
        );
        assert_eq!(restored.cards.len(), 1);
        assert_eq!(restored.cards["example-org/other:task"], "KEEP");
        assert_eq!(restored.boards["example-org/other"], "OTHER");
        assert_eq!(restored.milestones["example-org/project:goal"], 7);
    }

    #[tokio::test]
    async fn a_board_with_the_expected_title_needs_no_mutation() {
        let board = json!({"id":"BOARD","title":"dvb: example-org/project"});
        for (receipt, response) in [
            (None, board_list(board.clone())),
            (Some("BOARD"), json!({"node":board})),
        ] {
            let (result, calls) = recorded_board(receipt, vec![response]).await;
            assert_eq!(result.unwrap(), "BOARD");
            assert_eq!(calls.len(), 1);
            assert!(calls[0].0.contains("id title"));
            assert!(!calls[0].0.contains("mutation"));
        }
    }

    #[tokio::test]
    async fn a_receipt_repairs_only_a_changed_title() {
        let (result, calls) = recorded_board(
            Some("BOARD"),
            vec![
                json!({"node":{"id":"BOARD","title":"Manual rename"}}),
                json!({"updateProjectV2":{"projectV2":{"id":"BOARD"}}}),
            ],
        )
        .await;
        assert_eq!(result.unwrap(), "BOARD");
        assert_eq!(calls[0].1, json!({"id":"BOARD"}));
        assert!(calls[1].0.contains("input:{projectId:$id,title:$title}"));
        assert_eq!(
            calls[1].1,
            json!({"id":"BOARD","title":"dvb: example-org/project"})
        );
    }

    #[tokio::test]
    async fn a_new_board_needs_no_title_repair() {
        let (result, calls) = recorded_board(None, vec![
            json!({"organization":{"id":"ORG","projectsV2":{"nodes":[],"pageInfo":{"hasNextPage":false}}}}),
            json!({"createProjectV2":{"projectV2":{"id":"BOARD","title":"dvb: example-org/project"}}}),
        ]).await;
        assert_eq!(result.unwrap(), "BOARD");
        assert_eq!(
            calls[1].1,
            json!({"owner":"ORG","title":"dvb: example-org/project"})
        );
        assert!(calls[1].0.contains("input:{ownerId:$owner,title:$title}"));
    }

    #[test]
    fn graphql_success_returns_data_and_partial_errors_refuse() {
        let data = json!({"node": {"id": "BOARD"}});
        assert_eq!(graphql_data(json!({"data": data})).unwrap(), data);
        let error = graphql_data(
            json!({"data": data, "errors": [{"type": "FORBIDDEN", "path": ["updateProjectV2"]}]}),
        )
        .unwrap_err();
        assert_eq!(
            error.detail,
            "GitHub board response is unreadable (updateProjectV2 FORBIDDEN)"
        );
    }
}
