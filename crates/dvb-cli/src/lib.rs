//! The owner verbs on the command line: one subcommand per route.

/// Write command output, treating a closed pipe as a successful command exit.
pub fn write_stdout(args: std::fmt::Arguments<'_>) -> Result<(), client::CliError> {
    use std::io::Write;
    let mut stdout = std::io::stdout().lock();
    if let Err(error) = stdout.write_fmt(args).and_then(|()| stdout.flush()) {
        if error.kind() == std::io::ErrorKind::BrokenPipe {
            std::process::exit(0);
        }
        return Err(client::CliError::Message(format!("stdout: {error}")));
    }
    Ok(())
}

/// Write a `--json` output: one compact JSON object and a newline.
fn write_json(value: &impl serde::Serialize) -> Result<(), client::CliError> {
    let text = serde_json::to_string(value).map_err(client::message)?;
    write_stdout(format_args!("{text}\n"))
}

mod doctor;
pub mod github;
mod update;
mod wait;
mod watch;

pub mod app;
pub mod client;

use clap::{Args, Subcommand};
use client::{CliError, Client, message};
use dvb_core::api::{
    Brief, ChartMove, DiffView, GoalState, GoalView, HireCreated, HireView, LoginCode,
    LoginStarted, NewGoal, NewHire, NewProject, ProjectView, PublicationChange,
    PublicationMismatch, PublishingIdentity, QuestionView, Redirect, Report, ReportKind,
    ReportPreview, Restore, RoleChanges, RoleFields, SetOnly, Settings, SettingsKey,
    SettingsRestore, Status, TaskView, Usage, VerdictView,
};
use dvb_core::event::{Answer, CiMode};
use dvb_core::roles::{HireRecord, HireStatus, RoleRecord};
use dvb_core::wording::{goal_word, state_word};
use dvb_core::{Effort, Event, EventId, EventKind, RoleId, TaskState, Vendor};
use std::path::PathBuf;

/// Where the engine is.
#[derive(Debug, Clone, Args)]
pub struct Target {
    /// Talk to a dev-mode engine (`dvb engine --dev <dir>`).
    #[arg(long, global = true, value_name = "DIR")]
    pub dev: Option<PathBuf>,
    /// The engine's socket.
    #[arg(long, global = true, env = "DVB_ENGINE_SOCK", value_name = "PATH")]
    pub sock: Option<PathBuf>,
}

impl Target {
    pub fn sock(&self) -> PathBuf {
        match (&self.sock, &self.dev) {
            (Some(s), _) => s.clone(),
            (None, Some(dir)) => dir.join("engine.sock"),
            (None, None) => dvb_core::io::ENGINE_SOCKET.into(),
        }
    }
}

#[derive(Debug, clap::Args)]
pub struct GoalArgs {
    #[command(subcommand)]
    pub verb: Option<GoalVerb>,
    #[arg(required = true)]
    pub project: Option<String>,
    /// Plan only after this goal is Done or Dropped.
    #[arg(long, value_name = "GOAL-ID")]
    pub after: Option<String>,
    /// Attach a trusted issue of the project's repository; repeat for more.
    /// The goal carries its text and its last PR closes it.
    #[arg(long = "issue", value_name = "NUMBER")]
    pub issues: Vec<u64>,
    #[arg(required_unless_present = "issues", num_args = 1..)]
    pub text: Vec<String>,
    /// Print {"goal": "<id>"}.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Subcommand)]
pub enum GoalVerb {
    /// Drop a goal: its planned tasks are cancelled, its questions closed, and
    /// it is never planned again. Merged work stays.
    Drop {
        goal: String,
        /// Print the dropped goal as one JSON object.
        #[arg(long)]
        json: bool,
    },
    /// Plan a stated goal with no started task only after another goal is
    /// Done or Dropped; `none` clears its order.
    After {
        goal: String,
        #[arg(value_name = "OTHER-GOAL|none")]
        other: String,
        /// Print the goal as one JSON object.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum Verb {
    /// Open or focus the dvb app window.
    App {
        /// Open on an app path, such as /setup.
        #[arg(long, value_name = "PATH", value_parser = app::parse_route)]
        route: Option<String>,
    },
    /// Connect GitHub from the command line.
    #[command(subcommand)]
    Github(GithubVerb),
    /// What setup still needs: the GitHub App, its repository access and the
    /// vendor logins, each unmet item with the command that fixes it. Exits 1
    /// when anything is unmet.
    Doctor {
        /// Print one JSON object: {"items": [{"met", "text"}], "unmet"}.
        #[arg(long)]
        json: bool,
    },
    /// Projects.
    #[command(subcommand)]
    Project(ProjectVerb),
    /// State a goal for a project, `goal drop <goal>` to drop one, or
    /// `goal after <goal> <other-goal|none>` to change its order.
    #[command(args_conflicts_with_subcommands = true, subcommand_negates_reqs = true)]
    Goal(GoalArgs),
    /// Answer a question with an option's number, or a reply in words.
    Answer {
        question: String,
        #[arg(required_unless_present = "reply")]
        choice: Option<u32>,
        #[arg(long, conflicts_with = "choice")]
        reply: Option<String>,
        /// Print one JSON object: the question, the answer, and its task and goal.
        #[arg(long)]
        json: bool,
    },
    /// What is running, what needs you, and a project's goals and tasks.
    Status {
        project: Option<String>,
        #[arg(long)]
        bar: bool,
        /// Include finished goals and their tasks.
        #[arg(long)]
        all: bool,
        /// Print the same content as one JSON object.
        #[arg(long, conflicts_with = "bar")]
        json: bool,
    },
    /// Run only these exact model ids install-wide for a time; no models reads it.
    Only {
        #[arg(requires = "hours")]
        models: Vec<String>,
        /// Whole hours, from 1 to 168. Reissuing replaces the current switch.
        #[arg(long, requires = "models", value_parser = clap::value_parser!(u16).range(1..=168))]
        hours: Option<u16>,
        #[arg(long, conflicts_with_all = ["models", "hours"])]
        clear: bool,
    },
    /// Replace a role's active hire (use its id from `dvb role list`).
    Hire {
        role: String,
        model: String,
        #[arg(long, default_value = "medium")]
        effort: Effort,
        /// Hire the role's routine builder: routine tasks and fix rounds on
        /// open PRs run on it. Replaces only the role's routine hire.
        #[arg(long)]
        routine: bool,
    },
    /// List a project's active hires in role-list order.
    Hires {
        project: String,
        /// Include retired hires.
        #[arg(long)]
        all: bool,
    },
    /// A project's roles: list them, add one, or change one.
    #[command(subcommand)]
    Role(RoleVerb),
    /// Move roles on a project's chart: who reports to whom, groups and order.
    Chart {
        project: String,
        #[command(subcommand)]
        change: ChartVerb,
    },
    /// What a task changed, or what its reviewer found.
    #[command(subcommand)]
    Task(TaskVerb),
    /// Retire a hire, keeping its record.
    Retire { hire: String },
    /// Read settings or set one key to a JSON value.
    Settings {
        #[command(subcommand)]
        command: Option<SettingsVerb>,
    },
    /// What project checks, a role, hire or setting were before: its versions, newest first.
    #[command(subcommand)]
    History(Record),
    /// Bring a role, hire or setting back to the value an earlier event set.
    #[command(subcommand)]
    Restore(RestoreRecord),
    /// Prepare a bug report or feature request: prints the issue and the GitHub
    /// link that opens it. Nothing is filed.
    Report {
        #[arg(value_enum)]
        kind: ReportKindArg,
        #[arg(long)]
        title: String,
        #[arg(required = true, num_args = 1..)]
        text: Vec<String>,
    },
    /// Log a vendor's CLI in (`claude` or `codex`) as the worker user: prints the
    /// link to open and, for Claude, takes the code the page shows.
    Login { vendor: Vendor },
    /// Tokens, estimated cost and each vendor's usage window.
    Usage { project: Option<String> },
    /// Check GitHub for a newer release; without `--check`, install it.
    Update {
        /// Only say whether a newer release exists.
        #[arg(long)]
        check: bool,
        /// Install this version (going back needs the same database schema).
        #[arg(long, conflicts_with = "check")]
        to: Option<String>,
    },
    /// Add one Hyprland keybinding that opens the app (Omarchy's `bindings.lua`).
    Keybind {
        /// The keys, as Omarchy writes them.
        #[arg(long, default_value = DEFAULT_KEYS)]
        keys: String,
        /// The bindings file (default: ~/.config/hypr/bindings.lua).
        #[arg(long)]
        file: Option<PathBuf>,
    },
    /// Stop a task, in any state; its branch, workspace and results are kept.
    Stop {
        task: String,
        /// Why: recorded with the stop. With a reason the orchestrator is not
        /// woken to ask why.
        #[arg(long)]
        reason: Option<String>,
        /// Print {"task": {...}}: the task as `dvb status <project> --json` lists it.
        #[arg(long)]
        json: bool,
    },
    /// Resume a stopped task from where it stopped, unpause a project, or ask
    /// a goal's plan on hold ("Not yet") again: the same plan question, under
    /// a new question id, with no new plan.
    Resume {
        /// A task id, a goal id, or a project's name or id.
        id: String,
        /// Print {"task": {...}}, {"project": {...}}, or for a goal
        /// {"goal": {...}, "question": {...}} with the new question's id and options.
        #[arg(long)]
        json: bool,
    },
    /// Retry a failed task, resume a task stopped on a blocked or partial
    /// result, or send a task with a PR back to its builder with the retry text.
    Retry {
        task: String,
        /// Missing information for this retry's builder; at most 16 KiB of UTF-8.
        text: Vec<String>,
        /// Print {"task": {...}}: the task as `dvb status <project> --json` lists it.
        #[arg(long)]
        json: bool,
    },
    /// Pause a project: no new runs or steps start until it is resumed; what is
    /// running carries on.
    Pause {
        project: String,
        /// Print {"project": {...}}: the project as `dvb project list --json` lists it.
        #[arg(long)]
        json: bool,
    },
    /// Show a project's brief, or set it from the text given (`--clear` removes
    /// it). The brief is kept only in dvb and goes into the orchestrator's and
    /// every builder's prompt; it is never sent to GitHub.
    Brief {
        project: String,
        #[arg(long, conflicts_with = "text")]
        clear: bool,
        text: Vec<String>,
    },
    /// Change course: the goal's unstarted tasks are dropped and the
    /// orchestrator plans the new text, then asks for your yes.
    Redirect {
        goal: String,
        #[arg(required = true, num_args = 1..)]
        text: Vec<String>,
    },
    /// Print events as they happen, one line each.
    Watch {
        /// Start after this event id.
        #[arg(long, default_value_t = 0)]
        after: u64,
        /// Print what has happened so far, then exit.
        #[arg(long)]
        once: bool,
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        role: Option<String>,
        #[arg(long)]
        task: Option<String>,
        #[arg(long = "kind")]
        kinds: Vec<String>,
        /// Print one JSON object per line: the event with `time` and `text`.
        #[arg(long)]
        json: bool,
    },
    /// Block until a goal's plan is asked, it is Done, one of its tasks fails
    /// or stops, its plan is on hold, or its project is paused. Only this
    /// goal's tasks and questions end the wait; reviews do not. A state that
    /// already holds returns at once. Reconnects when the engine restarts.
    #[command(after_help = wait::EXIT_CODES)]
    Wait {
        /// The goal's id, as `dvb goal` and `dvb status <project>` print it.
        goal: String,
        #[arg(long, value_enum, default_value_t = wait::Until::Done)]
        until: wait::Until,
        /// Give up after this many whole minutes; 0 checks once.
        #[arg(long, value_name = "MINUTES")]
        timeout: Option<u64>,
        /// Print what ended the wait as one JSON object; the exit code is the same.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum RoleVerb {
    /// List a project's roles.
    List { project: String },
    /// Add a role to a project.
    Add {
        project: String,
        #[arg(long)]
        name: String,
        /// What the role does.
        #[arg(long, default_value = "")]
        job: String,
        /// What the role decides.
        #[arg(long, default_value = "")]
        lane: String,
        /// A lead reviews its reports' work.
        #[arg(long)]
        lead: bool,
        /// The role it reports to (id or slug).
        #[arg(long)]
        reports_to: Option<String>,
        /// The role that reviews its work (id or slug).
        #[arg(long)]
        reviewed_by: Option<String>,
        #[arg(long, default_value = "")]
        review_instructions: String,
        #[arg(long)]
        group: Option<String>,
        #[arg(long, default_value_t = 0)]
        order: u32,
    },
    /// Change a role: only the fields you give change.
    Edit {
        project: String,
        /// The role's id or slug.
        role: String,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        job: Option<String>,
        #[arg(long)]
        lane: Option<String>,
        #[arg(long)]
        lead: Option<bool>,
        /// The role it reports to (id or slug); empty for none.
        #[arg(long)]
        reports_to: Option<String>,
        /// The role that reviews its work (id or slug); empty for none.
        #[arg(long)]
        reviewed_by: Option<String>,
        #[arg(long)]
        review_instructions: Option<String>,
        /// The group name; empty for none.
        #[arg(long)]
        group: Option<String>,
        #[arg(long)]
        order: Option<u32>,
    },
}

#[derive(Debug, Subcommand)]
pub enum ChartVerb {
    /// Make a role report to another, or to no one with no `--to`.
    Report {
        role: String,
        #[arg(long)]
        to: Option<String>,
    },
    /// Put roles in a group, or take them out of theirs with no `--name`.
    Group {
        #[arg(long)]
        name: Option<String>,
        #[arg(required = true, num_args = 1..)]
        roles: Vec<String>,
    },
    /// Move a role before another, or last on its line with no `--before`.
    Reorder {
        role: String,
        #[arg(long)]
        before: Option<String>,
    },
    /// Exchange a role and its lead, including their other reports.
    Swap { role: String, lead: String },
}

#[derive(Debug, Subcommand)]
pub enum TaskVerb {
    /// The task's published change against its base, as a unified diff.
    Diff {
        task: String,
        /// Print one JSON object: {"task", "base", "head", "diff"}.
        #[arg(long)]
        json: bool,
    },
    /// The latest review of the task's current change.
    Verdict {
        task: String,
        /// Print the review as one JSON object.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum ReportKindArg {
    Bug,
    Feature,
}

#[derive(Debug, Subcommand)]
pub enum Record {
    /// A project's saved check choices, including its initial choice.
    Checks {
        project: String,
    },
    Role {
        id: String,
    },
    Hire {
        id: String,
    },
    /// A setting, by key (such as `response_style`).
    Setting {
        key: String,
    },
}

#[derive(Debug, Args)]
pub struct RestoreArgs {
    /// The role or hire id, or the setting's key.
    pub id: String,
    /// The event whose value comes back (see `dvb history`).
    pub event: u64,
    /// The latest event you looked at; the restore is refused if it changed since.
    /// Defaults to the latest event now.
    #[arg(long)]
    pub current: Option<u64>,
}

#[derive(Debug, Subcommand)]
pub enum RestoreRecord {
    Role(RestoreArgs),
    Hire(RestoreArgs),
    Setting(RestoreArgs),
}

#[derive(Debug, Subcommand)]
pub enum SettingsVerb {
    Get,
    Set {
        key: String,
        value: String,
    },
    /// Show who dvb's commits name as author and committer, or change it
    /// for the next commit dvb publishes.
    Publication {
        /// The name dvb's commits show as author and committer, e.g. `dvb`.
        /// dvb keeps your identity out of what it publishes: the name can't
        /// contain your name or email as set at GitHub setup (unless
        /// --as-owner), a line break, '<' or '>', or anything that looks like
        /// a secret.
        #[arg(long)]
        name: Option<String>,
        /// The email dvb's commits show, e.g. `dvb@dvb.invalid`: any address
        /// that isn't yours as set at GitHub setup (unless --as-owner), with
        /// no line break, '<' or '>', and nothing that looks like a secret.
        #[arg(long)]
        email: Option<String>,
        /// Publish as yourself: the name and email may be yours, and commits
        /// count on the GitHub account the email belongs to. Secrets are still
        /// refused, and the scan of published files is unchanged.
        #[arg(long, conflicts_with = "neutral")]
        as_owner: bool,
        /// Keep your identity out of dvb's commits again (the default), as
        /// dvb's default name and email unless --name or --email is given.
        #[arg(long)]
        neutral: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum GithubVerb {
    /// Create dvb's GitHub App under an organization or personal account and
    /// install it: prints the links to open, waits up to 15 minutes for GitHub,
    /// then prints the account and repositories. Exits as `dvb doctor` does.
    /// Credentials stay in the engine.
    Connect {
        /// The GitHub organization or personal account; defaults to the one
        /// already being set up.
        #[arg(long, value_name = "ORG-OR-USER")]
        owner: Option<String>,
        /// Also wait until the App can reach these repositories (`name` or
        /// `owner/name`).
        #[arg(long, value_delimiter = ',', value_name = "REPO,...")]
        repos: Vec<String>,
        /// Print one JSON line per link, then the result as `dvb doctor --json`
        /// does, with `owner` and `repositories`.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum ProjectVerb {
    /// Print the saved checks, or replace them with names or explicit --no-ci.
    Checks {
        project: String,
        #[arg(long, conflicts_with = "names")]
        no_ci: bool,
        names: Vec<String>,
    },
    /// Add a project: a GitHub `owner/name`, or a local repo path with the Fake forge.
    Add {
        name: String,
        #[arg(long)]
        repo: String,
        /// Create a private repository, then publish its setup commit; no CI unless --check is given.
        #[arg(long)]
        create: bool,
        #[arg(long)]
        default_branch: Option<String>,
        /// The owner's explicit choice: no CI; the reviewer states the local gates ran.
        #[arg(long, conflicts_with = "check")]
        no_ci: bool,
        /// A check name that must succeed on a head before it merges.
        #[arg(long)]
        check: Vec<String>,
        /// Print the project as one JSON object, as the engine returns it.
        #[arg(long)]
        json: bool,
    },
    /// Point the project at its repository's new name after a rename:
    /// the same repository, checked by its GitHub id.
    Repo { project: String, repo: String },
    /// Remove a project from dvb and free its files on this machine (work
    /// directories, mirror, toolchain cache, GitHub mirror receipts). Refused
    /// while it has running or open tasks; nothing changes on GitHub.
    Remove { project: String },
    /// List projects.
    List {
        /// Print {"projects": [...]}, each project as `project add --json` prints it.
        #[arg(long)]
        json: bool,
    },
}

fn publication_text(identity: &PublishingIdentity) -> String {
    let default = if identity.is_default {
        " (the default)"
    } else {
        ""
    };
    format!(
        "dvb publishes commits as {} <{}>{default}",
        identity.name, identity.email
    )
}

/// The publication identity `dvb settings publication` asks for: what was
/// given, the rest as it is now, or empty (the engine's default) with
/// `--neutral`. `None` when nothing was given.
fn publication_change(
    current: &PublishingIdentity,
    name: Option<String>,
    email: Option<String>,
    as_owner: bool,
    neutral: bool,
) -> Option<PublicationChange> {
    let rest = |current: &String| {
        if neutral {
            String::new()
        } else {
            current.clone()
        }
    };
    (name.is_some() || email.is_some() || as_owner || neutral).then(|| PublicationChange {
        name: name.unwrap_or_else(|| rest(&current.name)),
        email: email.unwrap_or_else(|| rest(&current.email)),
        as_owner: as_owner || (current.as_owner && !neutral),
    })
}

/// `dvb settings publication`'s output: the identity, and what publishing as
/// the owner changes.
fn publication_setting_text(identity: &PublishingIdentity) -> String {
    let mut text = publication_text(identity);
    if identity.as_owner {
        text.push_str(", as you: they count on that GitHub account");
    }
    text
}

/// The mismatch, and the command to switch unless the recent commits are the
/// owner's (`dvb settings publication` without `--as-owner` refuses those).
fn mismatch_text(m: &PublicationMismatch) -> String {
    let mut text = format!(
        "dvb publishes commits as {} <{}>, but this repository's recent dvb commits are by {} <{}>",
        m.publishes_as.name, m.publishes_as.email, m.recent.name, m.recent.email,
    );
    if !m.recent_is_owner {
        text.push_str(&format!(
            "; to switch: dvb settings publication --name {} --email {}",
            shell_word(&m.recent.name),
            shell_word(&m.recent.email),
        ));
    }
    text
}

/// `text` as one word for a POSIX shell.
fn shell_word(text: &str) -> String {
    if !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "@%+,-./:=_".contains(c))
    {
        text.to_owned()
    } else {
        format!("'{}'", text.replace('\'', r"'\''"))
    }
}

fn project_not_freed(left: &str, project: &dvb_core::ProjectId) -> String {
    let mut text = format!("dvb: not freed: {left}");
    let cache = std::path::Path::new("mirrors/.cache").join(project.as_str());
    if let Some((path, _error)) = left.rsplit_once(": ")
        && std::path::Path::new(path).ends_with(cache)
    {
        text.push_str(&format!(
            "\ndvb: This folder was made by an older dvb and can be deleted as root: sudo rm -rf {}",
            shell_word(path)
        ));
    }
    text
}

/// Runs a verb; the exit code.
pub async fn main(target: &Target, verb: Verb) -> i32 {
    let client = Client::new(target.sock());
    let result = run(target, &client, verb).await;
    report(result)
}

/// Every error exits 1; 2 is clap's, for a usage error.
fn report(result: Result<i32, CliError>) -> i32 {
    match result {
        Ok(code) => code,
        Err(CliError::Refused(r)) => {
            eprintln!("dvb: refused: {} ({})", r.code.as_str(), r.detail);
            1
        }
        Err(e) => {
            eprintln!("dvb: {e}");
            1
        }
    }
}

/// The refusal for a name or id the engine has nothing for.
fn not_found(detail: String) -> CliError {
    CliError::Refused(dvb_core::Refusal::new(
        dvb_core::RefusalCode::NotFound,
        detail,
    ))
}

/// A record's history route, and its versions (newest first) as plain JSON.
async fn history(
    client: &Client,
    record: &Record,
) -> Result<(String, Vec<serde_json::Value>), CliError> {
    let path = match record {
        Record::Checks { project } => {
            let p = project_id(client, project).await?;
            format!("/projects/{}/checks/history", p.id)
        }
        Record::Role { id } => format!("/roles/{id}/history"),
        Record::Hire { id } => format!("/hires/{id}/history"),
        Record::Setting { key } => format!(
            "/settings/history?{}",
            serde_urlencoded::to_string([("key", key)]).map_err(message)?
        ),
    };
    let h: serde_json::Value = client.get(&path).await?;
    let versions = h["versions"].as_array().cloned().unwrap_or_default();
    Ok((path, versions))
}

fn version_line(v: &serde_json::Value) -> String {
    format!(
        "{:>5}  {:<6} {}",
        v["event"],
        v["actor"].as_str().unwrap_or("run"),
        v["value"]
    )
}

async fn restore(client: &Client, r: RestoreRecord) -> Result<(), CliError> {
    let (record, path, key, args) = match r {
        RestoreRecord::Role(a) => (
            Record::Role { id: a.id.clone() },
            format!("/roles/{}/restore", a.id),
            None,
            a,
        ),
        RestoreRecord::Hire(a) => (
            Record::Hire { id: a.id.clone() },
            format!("/hires/{}/restore", a.id),
            None,
            a,
        ),
        RestoreRecord::Setting(a) => (
            Record::Setting { key: a.id.clone() },
            "/settings/restore".into(),
            Some(a.id.parse::<SettingsKey>().map_err(CliError::Message)?),
            a,
        ),
    };
    let current = match args.current {
        Some(c) => c,
        None => history(client, &record)
            .await?
            .1
            .first()
            .and_then(|v| v["event"].as_u64())
            .ok_or_else(|| not_found(format!("no history for {}", args.id)))?,
    };
    let (event, current) = (EventId(args.event), EventId(current));
    let restored: serde_json::Value = match key {
        None => client.post(&path, &Restore { event, current }).await?,
        Some(key) => {
            let body = SettingsRestore {
                key,
                event,
                current,
            };
            client.post(&path, &body).await?
        }
    };
    write_stdout(format_args!(
        "Restored {} from event {}: {restored}\n",
        args.id, args.event
    ))?;
    Ok(())
}

/// Keep role order and mark the hire used by new builder tasks.
fn hires_text(roles: &[RoleRecord], hires: &[HireView], all: bool) -> String {
    let mut out = String::new();
    for role in roles {
        let active = hires
            .iter()
            .filter(|h| h.hire.role == role.id && h.hire.status == HireStatus::Active)
            .count();
        for hire in hires
            .iter()
            .filter(|h| h.hire.role == role.id && (all || h.hire.status == HireStatus::Active))
        {
            let marker = match hire.hire.status {
                HireStatus::Retired => "  [retired]",
                HireStatus::Active if hire.hire.routine => "  [routine]",
                HireStatus::Active
                    if active > 1 && hire.default_hire.as_ref() == Some(&hire.hire.id) =>
                {
                    "  [default]"
                }
                HireStatus::Active => "",
            };
            out.push_str(&format!(
                "{}  {}  {}  {}  {}{}\n",
                role.name,
                role.id,
                hire.hire.id,
                hire.hire.model,
                hire.hire.effort.as_str(),
                marker
            ));
        }
    }
    out
}

async fn project_roles(
    client: &Client,
    project: &str,
) -> Result<(ProjectView, Vec<RoleRecord>), CliError> {
    let p = project_id(client, project).await?;
    let roles = client.get(&format!("/projects/{}/roles", p.id)).await?;
    Ok((p, roles))
}

/// A role's id, by id or slug.
fn role_id(roles: &[RoleRecord], key: &str) -> Result<RoleId, CliError> {
    roles
        .iter()
        .find(|r| r.id.as_str() == key || r.slug == key)
        .map(|r| r.id.clone())
        .ok_or_else(|| not_found(format!("no role {key}")))
}

/// An optional role flag: absent is no change, empty clears it.
fn role_flag(roles: &[RoleRecord], flag: Option<&str>) -> Result<Option<Option<RoleId>>, CliError> {
    match flag {
        None => Ok(None),
        Some("") => Ok(Some(None)),
        Some(key) => role_id(roles, key).map(|id| Some(Some(id))),
    }
}

async fn role(client: &Client, verb: RoleVerb) -> Result<(), CliError> {
    match verb {
        RoleVerb::List { project } => {
            let (_, roles) = project_roles(client, &project).await?;
            for r in roles {
                write_stdout(format_args!("{}  {}  {}\n", r.slug, r.id, r.name))?;
            }
        }
        RoleVerb::Add {
            project,
            name,
            job,
            lane,
            lead,
            reports_to,
            reviewed_by,
            review_instructions,
            group,
            order,
        } => {
            let (p, roles) = project_roles(client, &project).await?;
            let fields = RoleFields {
                name,
                job,
                lane,
                lead,
                reports_to: role_flag(&roles, reports_to.as_deref())?.flatten(),
                reviewed_by: role_flag(&roles, reviewed_by.as_deref())?.flatten(),
                review_instructions,
                group,
                order,
            };
            let r: RoleRecord = client
                .post(&format!("/projects/{}/roles", p.id), &fields)
                .await?;
            write_stdout(format_args!("Role {} added ({})\n", r.name, r.id))?;
        }
        RoleVerb::Edit {
            project,
            role,
            name,
            job,
            lane,
            lead,
            reports_to,
            reviewed_by,
            review_instructions,
            group,
            order,
        } => {
            let (_, roles) = project_roles(client, &project).await?;
            let id = role_id(&roles, &role)?;
            let changes = RoleChanges {
                name,
                job,
                lane,
                lead,
                reports_to: role_flag(&roles, reports_to.as_deref())?,
                reviewed_by: role_flag(&roles, reviewed_by.as_deref())?,
                review_instructions,
                group: group.map(|g| (!g.is_empty()).then_some(g)),
                order,
            };
            let r: RoleRecord = client.patch(&format!("/roles/{id}"), &changes).await?;
            write_stdout(format_args!("Role {} changed ({})\n", r.name, r.id))?;
        }
    }
    Ok(())
}

async fn chart(client: &Client, project: &str, change: ChartVerb) -> Result<(), CliError> {
    let (p, roles) = project_roles(client, project).await?;
    let movement = match change {
        ChartVerb::Report { role, to } => ChartMove::Report {
            role: role_id(&roles, &role)?,
            parent: to.map(|t| role_id(&roles, &t)).transpose()?,
        },
        ChartVerb::Group { name, roles: keys } => ChartMove::Group {
            roles: keys
                .iter()
                .map(|k| role_id(&roles, k))
                .collect::<Result<_, _>>()?,
            group: name,
        },
        ChartVerb::Reorder { role, before } => ChartMove::Reorder {
            role: role_id(&roles, &role)?,
            before: before.map(|b| role_id(&roles, &b)).transpose()?,
        },
        ChartVerb::Swap { role, lead } => ChartMove::Swap {
            role: role_id(&roles, &role)?,
            lead: role_id(&roles, &lead)?,
        },
    };
    let moved: Vec<RoleRecord> = client
        .post(&format!("/projects/{}/chart", p.id), &movement)
        .await?;
    for r in moved {
        write_stdout(format_args!(
            "{}  reports to {}\n",
            r.slug,
            r.reports_to.map_or("no one".into(), |t| {
                roles
                    .iter()
                    .find(|x| x.id == t)
                    .map_or(t.to_string(), |x| x.slug.clone())
            })
        ))?;
    }
    Ok(())
}

/// A verdict in plain words: the summary, then each finding with its severity,
/// location and explanation.
pub fn verdict_text(v: &VerdictView) -> String {
    let mut out = format!("{} ({} open): {}", v.head, v.open_findings, v.summary);
    for f in &v.findings {
        let at = f.line.map_or(f.file.clone(), |l| format!("{}:{l}", f.file));
        let state = if f.open { "open" } else { "closed" };
        let severity = match f.severity {
            dvb_core::run_verbs::Severity::High => "high",
            dvb_core::run_verbs::Severity::Medium => "medium",
            dvb_core::run_verbs::Severity::Low => "low",
        };
        out.push_str(&format!("\n  [{state}] {severity} {at}  {}", f.title));
        for line in f.detail.lines() {
            out.push_str(&format!("\n      {line}"));
        }
    }
    out
}

async fn project_id(client: &Client, name_or_id: &str) -> Result<ProjectView, CliError> {
    let projects: Vec<ProjectView> = client.get("/projects").await?;
    projects
        .into_iter()
        .find(|p| p.name == name_or_id || p.id.as_str() == name_or_id)
        .ok_or_else(|| not_found(format!("no project {name_or_id}")))
}

/// Runs a verb; the exit code when it succeeds.
async fn run(target: &Target, client: &Client, verb: Verb) -> Result<i32, CliError> {
    match verb {
        Verb::App { route } => app::open(target, route.as_deref()).await?,
        Verb::Doctor { json } => return doctor::doctor(client, json, None).await,
        Verb::Github(GithubVerb::Connect { owner, repos, json }) => {
            return github::connect(
                target,
                client,
                github::Connect { owner, repos, json },
                github::POLL,
                github::LIMIT,
                |text| write_stdout(format_args!("{text}")),
            )
            .await;
        }
        Verb::Project(ProjectVerb::Add {
            name,
            repo,
            default_branch,
            create,
            no_ci,
            check,
            json,
        }) => {
            let ci = if !check.is_empty() {
                CiMode::Expected(check)
            } else if no_ci || create {
                CiMode::NoCi
            } else {
                let query =
                    serde_urlencoded::to_string([("repo", repo.as_str())]).map_err(message)?;
                let discovery: dvb_core::api::GithubCheckDiscovery =
                    client.get(&format!("/github/checks?{query}")).await?;
                match discovery {
                    dvb_core::api::GithubCheckDiscovery::Discovered { checks, .. } => {
                        let mut names: Vec<_> =
                            checks.into_iter().map(|check| check.name).collect();
                        names.sort();
                        names.dedup();
                        if names.is_empty() {
                            return Err(CliError::Refused(dvb_core::Refusal::new(
                                dvb_core::RefusalCode::CiEvidenceMissing,
                                "No checks detected on the default branch. Choose --check <name> or explicitly --no-ci.",
                            )));
                        }
                        if !json {
                            for name in &names {
                                write_stdout(format_args!("Detected check: {name}\n"))?;
                            }
                        }
                        CiMode::Expected(names)
                    }
                    dvb_core::api::GithubCheckDiscovery::Inaccessible { failure }
                    | dvb_core::api::GithubCheckDiscovery::Failed { failure } => {
                        return Err(CliError::Message(format!(
                            "{} No check choice was saved; retry discovery or choose --check <name> or explicitly --no-ci.",
                            failure.message
                        )));
                    }
                }
            };
            let p: ProjectView = client
                .post(
                    "/projects",
                    &NewProject {
                        name,
                        create,
                        repo,
                        default_branch,
                        ci,
                    },
                )
                .await?;
            if json {
                write_json(&p)?;
                return Ok(0);
            }
            write_stdout(format_args!("Project {} added ({})\n", p.name, p.id))?;
            if let Some(mismatch) = &p.publication_mismatch {
                write_stdout(format_args!("{}\n", mismatch_text(mismatch)))?;
            }
            if let Some(found) = p.issues_found.and_then(|counts| {
                dvb_core::wording::event_text(&EventKind::ProjectIssuesFound(counts), local_time)
            }) {
                write_stdout(format_args!("{found}\n"))?;
            }
        }
        Verb::Project(ProjectVerb::Checks {
            project,
            no_ci,
            names,
        }) => {
            let mut p = project_id(client, &project).await?;
            if no_ci || !names.is_empty() {
                let ci = if no_ci {
                    CiMode::NoCi
                } else {
                    CiMode::Expected(names)
                };
                p = client
                    .put(&format!("/projects/{}/checks", p.id), &ci)
                    .await?;
            }
            match p.ci {
                CiMode::NoCi => write_stdout(format_args!("no CI\n"))?,
                CiMode::Expected(names) => {
                    for name in names {
                        write_stdout(format_args!("{name}\n"))?;
                    }
                }
            }
        }
        Verb::Project(ProjectVerb::Repo { project, repo }) => {
            let before = project_id(client, &project).await?;
            let p: ProjectView = client
                .put(
                    &format!("/projects/{}/repo", before.id),
                    &dvb_core::api::ProjectRepo { repo },
                )
                .await?;
            if let Some(text) = dvb_core::wording::event_text(
                &EventKind::ProjectRepoChanged {
                    from: before.repo,
                    to: p.repo,
                },
                local_time,
            ) {
                write_stdout(format_args!("{text}\n"))?;
            }
        }
        Verb::Project(ProjectVerb::Remove { project }) => {
            let p = project_id(client, &project).await?;
            let removal: dvb_core::api::ProjectRemoval = client
                .post(&format!("/projects/{}/remove", p.id), &())
                .await?;
            if let Some(text) = dvb_core::wording::event_text(
                &EventKind::ProjectRemoved {
                    name: removal.project.name,
                    repo: removal.project.repo,
                },
                local_time,
            ) {
                write_stdout(format_args!("{text}\n"))?;
            }
            if !removal.not_freed.is_empty() {
                for left in &removal.not_freed {
                    eprintln!("{}", project_not_freed(left, &removal.project.id));
                }
                return Ok(1);
            }
        }
        Verb::Project(ProjectVerb::List { json }) => {
            let projects: Vec<ProjectView> = client.get("/projects").await?;
            if json {
                write_json(&serde_json::json!({ "projects": projects }))?;
                return Ok(0);
            }
            for p in projects {
                write_stdout(format_args!("{}  {}  {}\n", p.name, p.id, p.repo))?;
            }
        }
        Verb::Goal(GoalArgs {
            verb: Some(GoalVerb::Drop { goal, json }),
            ..
        }) => {
            let g: GoalView = client.post(&format!("/goals/{goal}/drop"), &()).await?;
            if json {
                write_json(&g)?;
            } else {
                write_stdout(format_args!("Goal {}: dropped - {}\n", g.id, g.text))?;
            }
        }
        Verb::Goal(GoalArgs {
            verb: Some(GoalVerb::After { goal, other, json }),
            ..
        }) => {
            let after = (other != "none").then(|| dvb_core::GoalId::new(other));
            let g: GoalView = client
                .post(
                    &format!("/goals/{goal}/after"),
                    &dvb_core::api::GoalAfter {
                        after: after.clone(),
                    },
                )
                .await?;
            if json {
                write_json(&g)?;
            } else if let Some(line) = dvb_core::wording::event_text(
                &EventKind::GoalAfterSet { goal: g.id, after },
                local_time,
            ) {
                write_stdout(format_args!("{line}\n"))?;
            }
        }
        Verb::Goal(GoalArgs {
            verb: None,
            project,
            after,
            issues,
            text,
            json,
        }) => {
            let project = project.unwrap_or_default();
            let p = project_id(client, &project).await?;
            let g: GoalView = client
                .post(
                    &format!("/projects/{}/goals", p.id),
                    &NewGoal {
                        text: text.join(" "),
                        from_issue: None,
                        issues,
                        after: after.map(dvb_core::GoalId::new),
                    },
                )
                .await?;
            if json {
                write_json(&serde_json::json!({"goal": g.id}))?;
            } else {
                write_stdout(format_args!("Goal {}: {}\n", g.id, g.text))?;
            }
        }
        Verb::Answer {
            question,
            choice,
            reply,
            json,
        } => {
            let answer = match (choice, reply) {
                (Some(n), _) => Answer::Choice(n),
                (None, Some(text)) => Answer::Reply(text),
                (None, None) => return Err(CliError::Message("give a choice or --reply".into())),
            };
            let q: QuestionView = client
                .post(&format!("/questions/{question}/answer"), &answer)
                .await?;
            if json {
                write_json(&answer_json(&q, &answer))?;
            } else {
                write_stdout(format_args!("Answered: {}\n", q.subject))?;
            }
        }
        Verb::Status {
            project,
            bar,
            all,
            json,
        } => status(client, project, bar, all, json).await?,
        Verb::Only {
            models,
            hours,
            clear,
        } => {
            let only: Option<dvb_core::model::OnlyModels> = if clear {
                client.post("/only/clear", &serde_json::Value::Null).await?
            } else if let Some(hours) = hours {
                Some(client.put("/only", &SetOnly { models, hours }).await?)
            } else {
                client.get("/only").await?
            };
            write_stdout(format_args!(
                "{}\n",
                only.as_ref()
                    .map(only_line)
                    .unwrap_or_else(|| "No model switch is active.".into())
            ))?;
        }
        Verb::Hire {
            role,
            model,
            effort,
            routine,
        } => {
            let receipt: HireCreated = client
                .post(
                    "/hires",
                    &NewHire {
                        role: dvb_core::RoleId::new(role),
                        model,
                        effort,
                        routine,
                    },
                )
                .await?;
            write_stdout(format_args!(
                "Hired {} ({}){}\n",
                receipt.hire.model,
                receipt.hire.id,
                if receipt.hire.routine {
                    " as routine"
                } else {
                    ""
                }
            ))?;
            for hire in receipt.retired {
                write_stdout(format_args!(
                    "Retired {} ({})  {}\n",
                    hire.model,
                    hire.id,
                    hire.effort.as_str()
                ))?;
            }
        }
        Verb::Hires { project, all } => {
            let (p, roles) = project_roles(client, &project).await?;
            let hires: Vec<HireView> = client.get(&format!("/hires?project={}", p.id)).await?;
            write_stdout(format_args!("{}", hires_text(&roles, &hires, all)))?;
        }
        Verb::Retire { hire } => {
            let h: HireRecord = client.post(&format!("/hires/{hire}/retire"), &()).await?;
            write_stdout(format_args!("Retired {} ({})\n", h.model, h.id))?;
        }
        Verb::Settings {
            command:
                Some(SettingsVerb::Publication {
                    name,
                    email,
                    as_owner,
                    neutral,
                }),
        } => {
            let current: PublishingIdentity = client.get("/settings/publication").await?;
            let identity = if let Some(change) =
                publication_change(&current, name, email, as_owner, neutral)
            {
                client
                    .put("/settings/publication", &change)
                    .await
                    .map_err(|mut error| {
                        if let CliError::Refused(r) = &mut error
                            && r.code == dvb_core::RefusalCode::InvalidValue
                        {
                            r.detail.push_str(if change.as_owner {
                                " Give another with --name or --email."
                            } else {
                                " Give another with --name or --email, or publish as yourself with --as-owner."
                            });
                        }
                        error
                    })?
            } else {
                current
            };
            write_stdout(format_args!("{}\n", publication_setting_text(&identity)))?;
        }
        Verb::Settings { command } => {
            let change = match command {
                None | Some(SettingsVerb::Get | SettingsVerb::Publication { .. }) => None,
                Some(SettingsVerb::Set { key, value }) => Some((key, value)),
            };
            let memory_saved = change
                .as_ref()
                .is_some_and(|(key, _)| key == "runs_memory_gb");
            let s: Settings = if let Some((key, value)) = change {
                let value: serde_json::Value = serde_json::from_str(&value).map_err(message)?;
                let patch: Settings =
                    serde_json::from_value(serde_json::json!({"values": {key: value}}))
                        .map_err(message)?;
                client.put("/settings", &patch).await?
            } else {
                client.get("/settings").await?
            };
            write_stdout(format_args!(
                "{}\n",
                serde_json::to_string_pretty(&s).map_err(message)?
            ))?;
            if memory_saved && let Some(warning) = s.runs_memory_warning() {
                write_stdout(format_args!("{warning}\n"))?;
            }
        }
        Verb::Role(r) => role(client, r).await?,
        Verb::Chart { project, change } => chart(client, &project, change).await?,
        Verb::Task(TaskVerb::Diff { task, json }) => {
            let d: DiffView = client.get(&format!("/tasks/{task}/diff")).await?;
            if json {
                write_json(&d)?;
            } else {
                write_stdout(format_args!("{}", d.diff))?;
            }
        }
        Verb::Task(TaskVerb::Verdict { task, json }) => {
            let v: VerdictView = client.get(&format!("/tasks/{task}/verdict")).await?;
            if json {
                let t: TaskView = client.get(&format!("/tasks/{task}")).await?;
                let roles: Vec<RoleRecord> = client
                    .get(&format!("/projects/{}/roles", t.project))
                    .await?;
                let mut out = serde_json::to_value(&v).map_err(client::message)?;
                out["role_name"] = role_name(&roles, &v.role).into();
                write_json(&out)?;
            } else {
                write_stdout(format_args!("{}\n", verdict_text(&v)))?;
            }
        }
        Verb::Stop { task, reason, json } => {
            let body = dvb_core::api::StopTask { reason };
            let t: TaskView = client.post(&format!("/tasks/{task}/stop"), &body).await?;
            if json {
                write_task_json(client, &t).await?;
            } else {
                let word = if let TaskState::Stopped { was } = &t.state {
                    dvb_core::wording::stopped_word(was).to_owned()
                } else {
                    state_word(&t.state)
                };
                write_stdout(format_args!("Task {}: {word}\n", t.id))?;
            }
        }
        Verb::Resume { id, json } => {
            let projects: Vec<ProjectView> = client.get("/projects").await?;
            let goals: Vec<GoalView> = client.get("/goals").await?;
            if let Some(p) = projects
                .iter()
                .find(|p| p.name == id || p.id.as_str() == id)
            {
                let () = client
                    .post(&format!("/projects/{}/resume", p.id), &())
                    .await?;
                if json {
                    write_project_json(client, &p.id).await?;
                } else {
                    write_stdout(format_args!("Project {} resumed\n", p.name))?;
                }
            } else if goals.iter().any(|g| g.id.as_str() == id) {
                let q: QuestionView = client.post(&format!("/goals/{id}/resume"), &()).await?;
                if json {
                    let goals: Vec<GoalView> = client.get("/goals").await?;
                    let g = goals
                        .iter()
                        .find(|g| g.id.as_str() == id)
                        .ok_or_else(|| not_found(format!("no goal {id}")))?;
                    write_json(&serde_json::json!({"goal": g, "question": question_json(&q)}))?;
                } else {
                    write_stdout(format_args!("Needs you: {} ({})\n", q.subject, q.id))?;
                }
            } else {
                let t: TaskView = match client.post(&format!("/tasks/{id}/resume"), &()).await {
                    Err(CliError::Refused(r)) if r.code == dvb_core::RefusalCode::NotFound => {
                        return Err(not_found(format!("no task, goal or project {id}")));
                    }
                    other => other?,
                };
                if json {
                    write_task_json(client, &t).await?;
                } else {
                    write_stdout(format_args!("Task {}: {}\n", t.id, state_word(&t.state)))?;
                }
            }
        }
        Verb::Retry { task, text, json } => {
            let body = dvb_core::api::RetryTask {
                text: (!text.is_empty()).then(|| text.join(" ")),
            };
            let t: TaskView = client.post(&format!("/tasks/{task}/retry"), &body).await?;
            if json {
                write_task_json(client, &t).await?;
            } else {
                write_stdout(format_args!("Task {}: {}\n", t.id, state_word(&t.state)))?;
            }
        }
        Verb::Pause { project, json } => {
            let p = project_id(client, &project).await?;
            let () = client
                .post(&format!("/projects/{}/pause", p.id), &())
                .await?;
            if json {
                write_project_json(client, &p.id).await?;
            } else {
                write_stdout(format_args!("Project {} paused\n", p.name))?;
            }
        }
        Verb::Brief {
            project,
            clear,
            text,
        } => {
            let p = project_id(client, &project).await?;
            let path = format!("/projects/{}/brief", p.id);
            if clear || !text.is_empty() {
                let body = Brief {
                    text: text.join(" "),
                };
                let b: Brief = client.put(&path, &body).await?;
                write_stdout(format_args!(
                    "Project {} brief {}\n",
                    p.name,
                    if b.text.is_empty() { "cleared" } else { "set" }
                ))?;
            } else {
                let b: Brief = client.get(&path).await?;
                write_stdout(format_args!("{}\n", b.text))?;
            }
        }
        Verb::Redirect { goal, text } => {
            let g: GoalView = client
                .post(
                    &format!("/goals/{goal}/redirect"),
                    &Redirect {
                        text: text.join(" "),
                    },
                )
                .await?;
            write_stdout(format_args!(
                "Goal {}: {} - the plan is asked again\n",
                g.id, g.text
            ))?;
        }
        Verb::History(record) => {
            let (_, versions) = history(client, &record).await?;
            for v in versions {
                write_stdout(format_args!("{}\n", version_line(&v)))?;
            }
        }
        Verb::Restore(r) => restore(client, r).await?,
        Verb::Report { kind, title, text } => {
            let report = Report {
                kind: match kind {
                    ReportKindArg::Bug => ReportKind::Bug,
                    ReportKindArg::Feature => ReportKind::Feature,
                },
                title,
                text: text.join(" "),
            };
            let p: ReportPreview = client.post("/report", &report).await?;
            write_stdout(format_args!(
                "{}\n\n{}\nOpen to file it (nothing is filed yet):\n{}\n",
                p.title, p.body, p.url
            ))?;
        }
        Verb::Login { vendor } => login(client, vendor).await?,
        Verb::Usage { project } => usage(client, project).await?,
        Verb::Keybind { keys, file } => {
            let file = match file {
                Some(f) => f,
                None => default_bindings_file()?,
            };
            match add_keybind(&file, &keys).map_err(CliError::Message)? {
                Keybind::Added => write_stdout(format_args!(
                    "Added {keys} to {}: it opens dvb\n",
                    file.display()
                ))?,
                Keybind::AlreadyThere => write_stdout(format_args!(
                    "{} already has the dvb keybinding\n",
                    file.display()
                ))?,
            }
        }
        Verb::Update { check, to } => update::update_target(target, client, check, to).await?,
        Verb::Watch {
            after,
            once,
            project,
            role,
            task,
            kinds,
            json,
        } => {
            let project = match project {
                Some(p) => Some(project_id(client, &p).await?.id.to_string()),
                None => None,
            };
            let filter = dvb_core::api::EventPageQuery {
                project: project.map(dvb_core::ProjectId::new),
                role: role.map(dvb_core::RoleId::new),
                task: task.map(dvb_core::TaskId::new),
                kinds: (!kinds.is_empty()).then(|| kinds.join(",")),
                ..Default::default()
            };
            watch::follow(client, after, once, json, &filter, |line| {
                write_stdout(format_args!("{line}\n"))?;
                Ok(true)
            })
            .await?;
        }
        Verb::Wait {
            goal,
            until,
            timeout,
            json,
        } => {
            let goal = dvb_core::GoalId::new(goal);
            return Ok(
                match wait::wait(client, &goal, until, timeout, json).await {
                    Ok(code) => code,
                    // Its other codes are outcomes, so every error is 1.
                    Err(error) => {
                        report(Err(error));
                        wait::ERROR
                    }
                },
            );
        }
    }
    Ok(0)
}

async fn login(client: &Client, vendor: Vendor) -> Result<(), CliError> {
    if vendor == Vendor::Fake {
        return Err(CliError::Message("the vendor is claude or codex".into()));
    }
    let path = format!("/logins/{}", vendor.as_str());
    let started: LoginStarted = client.post(&path, &()).await?;
    write_stdout(format_args!("Open this link: {}\n", started.url))?;
    write_stdout(format_args!("{}\n", started.prompt))?;
    // Claude's page shows a code to paste back; Codex's page needs nothing.
    let code = if vendor == Vendor::Claude {
        write_stdout(format_args!("Code: "))?;
        let mut line = String::new();
        std::io::stdin().read_line(&mut line).map_err(message)?;
        line.trim().to_owned()
    } else {
        String::new()
    };
    let run = started.run;
    let done = finish_login(vendor, code, |code| {
        let run = run.clone();
        let path = path.clone();
        async move { client.post(&path, &LoginCode { run, code }).await }
    })
    .await?;
    match done.status {
        dvb_core::api::LoginStatus::LoggedIn => {
            write_stdout(format_args!("{}\n", done.prompt))?;
            Ok(())
        }
        dvb_core::api::LoginStatus::Pending | dvb_core::api::LoginStatus::Failed => {
            Err(CliError::Message(done.prompt))
        }
    }
}

/// Sends the code once, then asks again with an empty code while the engine says
/// the login is still pending (a device login can take the owner minutes).
pub async fn finish_login<F, Fut>(
    vendor: Vendor,
    code: String,
    send: F,
) -> Result<LoginStarted, CliError>
where
    F: FnMut(String) -> Fut,
    Fut: std::future::Future<Output = Result<LoginStarted, CliError>>,
{
    finish_login_with_output(vendor, code, send, |prompt| {
        write_stdout(format_args!("{prompt}\n"))
    })
    .await
}

async fn finish_login_with_output<F, Fut>(
    vendor: Vendor,
    code: String,
    mut send: F,
    mut output: impl FnMut(&str) -> Result<(), CliError>,
) -> Result<LoginStarted, CliError>
where
    F: FnMut(String) -> Fut,
    Fut: std::future::Future<Output = Result<LoginStarted, CliError>>,
{
    if vendor == Vendor::Claude && code.trim().is_empty() {
        return Err(CliError::Message(
            "No code entered; run dvb login claude again.".into(),
        ));
    }
    let mut done = send(code).await?;
    while done.status == dvb_core::api::LoginStatus::Pending {
        output(&done.prompt)?;
        done = send(String::new()).await?;
    }
    if done.status == dvb_core::api::LoginStatus::LoggedIn {
        done.prompt = "Logged in.".into();
    }
    Ok(done)
}

async fn usage(client: &Client, project: Option<String>) -> Result<(), CliError> {
    let query = match &project {
        Some(name) => format!("?project={}", project_id(client, name).await?.id),
        None => String::new(),
    };
    let u: Usage = client.get(&format!("/usage{query}")).await?;
    write_stdout(format_args!("{}", format_usage(&u)))?;
    Ok(())
}

/// The usage view as text: one row per project, role and model, then the windows.
pub fn format_usage(u: &Usage) -> String {
    let mut out = String::new();
    if u.rows.is_empty() {
        out.push_str("No usage yet.\n");
    }
    for r in &u.rows {
        out.push_str(&format!(
            "{}  {}  {}  in {} (+{} cached)  out {}  about ${:.2}\n",
            r.project, r.role, r.model, r.uncached_in, r.cached_in, r.out, r.est_cost
        ));
    }
    for w in &u.windows {
        let reset = w
            .resets_at
            .map(|t| format!(", resets at {}", local_time(t)))
            .unwrap_or_default();
        out.push_str(&format!(
            "{}: {:.0}% of the {} window{}{}\n",
            w.vendor.as_str(),
            w.used_pct,
            w.window,
            reset,
            if w.limited {
                " (limit reached; new runs wait)"
            } else {
                ""
            }
        ));
    }
    out
}

pub const DEFAULT_KEYS: &str = "SUPER + ALT + D";
const KEYBIND_MARK: &str = "-- dvb: open the app";

#[derive(Debug, PartialEq, Eq)]
pub enum Keybind {
    Added,
    AlreadyThere,
}

fn default_bindings_file() -> Result<PathBuf, CliError> {
    let home =
        std::env::var_os("HOME").ok_or_else(|| CliError::Message("HOME is not set".into()))?;
    Ok(PathBuf::from(home).join(".config/hypr/bindings.lua"))
}

/// Appends `o.bind(<keys>, "dvb", "dvb app")` to an Omarchy `bindings.lua`, once.
/// The file must exist (it is Omarchy's); nothing else in it is touched.
pub fn add_keybind(file: &std::path::Path, keys: &str) -> Result<Keybind, String> {
    if keys.contains('"') || keys.contains('\n') {
        return Err(format!("not a key combination: {keys}"));
    }
    let text = std::fs::read_to_string(file).map_err(|e| {
        format!(
            "{}: {e} (this is Omarchy's bindings file; dvb adds one line to it)",
            file.display()
        )
    })?;
    if text.contains(KEYBIND_MARK) {
        return Ok(Keybind::AlreadyThere);
    }
    let sep = if text.is_empty() || text.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    let line = format!("{sep}\n{KEYBIND_MARK}\no.bind(\"{keys}\", \"dvb\", \"dvb app\")\n");
    use std::io::Write;
    std::fs::OpenOptions::new()
        .append(true)
        .open(file)
        .and_then(|mut file| file.write_all(line.as_bytes()))
        .map_err(|e| format!("{}: {e}", file.display()))?;
    Ok(Keybind::Added)
}

async fn status(
    client: &Client,
    project: Option<String>,
    bar: bool,
    all: bool,
    json: bool,
) -> Result<(), CliError> {
    let p = match &project {
        Some(name) => Some(project_id(client, name).await?),
        None => None,
    };
    let query = p
        .as_ref()
        .map(|p| format!("?project={}", p.id))
        .unwrap_or_default();
    let s: Status = client.get(&format!("/status{query}")).await?;
    if bar {
        // The bar shows counts; the tasks are for the lines below.
        let counts = Status {
            tasks: Vec::new(),
            ..s
        };
        write_stdout(format_args!(
            "{}\n",
            serde_json::to_string(&counts).map_err(message)?
        ))?;
        return Ok(());
    }
    let only: Option<dvb_core::model::OnlyModels> = client.get("/only").await?;
    let projects: Vec<ProjectView> = client.get("/projects").await?;
    let goals: Vec<GoalView> = client.get(&format!("/goals{query}")).await?;
    let parked = goals
        .iter()
        .filter(|g| g.plan_parked && !matches!(g.state, GoalState::Done | GoalState::Dropped))
        .cloned()
        .collect();
    let listing = match p {
        Some(p) => {
            let total = goals.len();
            let mut goals = goals;
            goals.retain(|g| all || !matches!(g.state, GoalState::Done | GoalState::Dropped));
            let goal_ids: std::collections::BTreeSet<_> =
                goals.iter().map(|g| g.id.clone()).collect();
            let mut tasks: Vec<TaskView> = client.get(&format!("/tasks?project={}", p.id)).await?;
            tasks.retain(|t| all || goal_ids.contains(&t.goal));
            let roles = client.get(&format!("/projects/{}/roles", p.id)).await?;
            Some(Listing {
                name: project.unwrap_or_else(|| p.name.clone()),
                omitted: total - goals.len(),
                project: p,
                goals,
                tasks,
                roles,
            })
        }
        None => None,
    };
    let questions: Vec<QuestionView> = client.get("/questions?open=true").await?;
    let paused = projects
        .iter()
        .filter(|pr| pr.paused && listing.as_ref().is_none_or(|l| l.project.id == pr.id))
        .cloned()
        .collect();
    let view = StatusView {
        status: s,
        only,
        listing,
        questions,
        parked,
        paused,
        projects,
    };
    if json {
        write_json(&status_json(&view)?)
    } else {
        write_stdout(format_args!("{}", status_text(&view)))
    }
}

/// What `dvb status` reads, before it is printed as text or JSON.
struct StatusView {
    status: Status,
    only: Option<dvb_core::model::OnlyModels>,
    /// With a project: its goals and tasks.
    listing: Option<Listing>,
    /// Every open question, of every project.
    questions: Vec<QuestionView>,
    /// The goals whose plan is on hold (of the project, with one).
    parked: Vec<GoalView>,
    /// The paused projects (the project, with one).
    paused: Vec<ProjectView>,
    /// Every project, for the names of the items' projects.
    projects: Vec<ProjectView>,
}

/// A project's goals and tasks as `dvb status <project>` lists them.
struct Listing {
    project: ProjectView,
    /// The project as the owner named it.
    name: String,
    goals: Vec<GoalView>,
    /// Finished goals not shown without `--all`.
    omitted: usize,
    tasks: Vec<TaskView>,
    /// The project's roles, for the tasks' role names.
    roles: Vec<RoleRecord>,
}

impl StatusView {
    /// A counted task's open question that leaves it stopped.
    fn folded(&self, t: &dvb_core::api::NeedsYouTask) -> Option<&QuestionView> {
        self.questions
            .iter()
            .find(|q| t.question.as_ref() == Some(&q.id))
    }

    /// The open questions of the project shown, each counted on its own.
    fn own_questions(&self) -> impl Iterator<Item = &QuestionView> {
        self.in_scope().filter(|q| {
            !self
                .status
                .tasks
                .iter()
                .any(|t| t.question.as_ref() == Some(&q.id))
        })
    }

    /// A project's name, or "" for one the engine no longer lists.
    fn project_name(&self, id: &dvb_core::ProjectId) -> &str {
        self.projects
            .iter()
            .find(|p| &p.id == id)
            .map_or("", |p| p.name.as_str())
    }

    fn in_scope(&self) -> impl Iterator<Item = &QuestionView> {
        self.questions.iter().filter(|q| {
            self.listing
                .as_ref()
                .is_none_or(|l| q.project == l.project.id)
        })
    }
}

/// How a counted task continues: its word and the `dvb` verb.
fn continue_verb(t: &dvb_core::api::NeedsYouTask) -> (&'static str, &'static str) {
    if matches!(t.state, TaskState::Stopped { .. }) {
        ("Stopped", "resume")
    } else {
        ("Failed", "retry")
    }
}

fn ruleset_missing_text() -> &'static str {
    dvb_core::wording::text("project.ruleset_missing")
}

fn status_text(v: &StatusView) -> String {
    let s = &v.status;
    let mut out = format!("{} running, {} need you\n", s.running, s.needs_you);
    for warning in &s.warnings {
        out.push_str(&format!("Warning: {}\n", warning.text()));
    }
    for name in &s.rulesets_missing {
        out.push_str(&format!("Warning: {name}: {}\n", ruleset_missing_text()));
    }
    if let Some(pass) = &s.mirror_pass {
        let on = pass
            .project
            .as_ref()
            .map(|id| format!(", on project {}", v.project_name(id)))
            .unwrap_or_default();
        out.push_str(&format!(
            "GitHub mirror pass running since {}{on}\n",
            chrono::DateTime::from_timestamp_millis(pass.started.millis()).map_or_else(
                || pass.started.millis().to_string(),
                |time| time.format("%H:%M UTC").to_string(),
            )
        ));
    }
    if let Some(only) = &v.only {
        out.push_str(&format!("{}\n", only_line(only)));
    }
    if let Some(l) = &v.listing {
        if l.project.paused {
            out.push_str(&format!(
                "Project {} is paused: nothing new starts until dvb resume {}\n",
                l.project.name, l.name
            ));
        }
        for goal in &l.goals {
            out.push_str(&format!("{}\n", goal_line(goal)));
        }
        if l.omitted > 0 {
            out.push_str(&format!(
                "{} finished goals not shown (dvb status {} --all)\n",
                l.omitted, l.name
            ));
        }
        for t in &l.tasks {
            out.push_str(&format!("{}\n", task_line(t)));
        }
    }
    for t in &s.tasks {
        let (word, verb) = continue_verb(t);
        out.push_str(&format!(
            "Needs you: {} ({}) - {word}: dvb {verb} {}\n",
            t.title, t.id, t.id
        ));
        // The project view keeps the folded question's options answerable.
        if v.listing.is_some()
            && let Some(q) = v.folded(t)
        {
            out.push_str(&format!("  Or answer: {} ({})\n", q.subject, q.id));
            for o in &q.options {
                out.push_str(&format!("{}\n", option_line(q, o)));
            }
        }
    }
    for g in &v.parked {
        out.push_str(&format!(
            "Needs you: {} ({}) - {}: dvb resume {}\n",
            goal_title(g),
            g.id,
            dvb_core::wording::text("goal.plan_parked"),
            g.id
        ));
    }
    for p in &v.paused {
        out.push_str(&format!(
            "Needs you: Project {} ({}) - {}: dvb resume {}\n",
            p.name,
            p.id,
            dvb_core::wording::text("project.paused"),
            p.name
        ));
    }
    for q in v.own_questions() {
        out.push_str(&format!("Needs you: {} ({})\n", q.subject, q.id));
        for o in &q.options {
            out.push_str(&format!("{}\n", option_line(q, o)));
        }
    }
    out
}

/// `dvb status --json`: the counts, warnings and timed model switch; with a
/// project its goals and tasks; the open questions (of the project, with one);
/// and each item that needs the owner with the command that continues it.
fn status_json(v: &StatusView) -> Result<serde_json::Value, CliError> {
    use serde_json::json;
    let s = &v.status;
    let mut out = json!({
        "running": s.running,
        "needs_you": s.needs_you,
        "warnings": s
            .warnings
            .iter()
            .map(|w| json!({"code": w, "text": w.text()}))
            .chain(s.rulesets_missing.iter().map(|name| {
                json!({"code": "ruleset_missing", "project": name, "text": ruleset_missing_text()})
            }))
            .collect::<Vec<_>>(),
        "only": v.only,
        "mirror_pass": s.mirror_pass.as_ref().map(|pass| json!({
            "started": pass.started,
            "project": pass.project,
            "project_name": pass.project.as_ref().map(|id| v.project_name(id)),
        })),
        "questions": v.in_scope().map(question_json).collect::<Vec<_>>(),
    });
    let mut items = Vec::new();
    for t in &s.tasks {
        let (_, verb) = continue_verb(t);
        items.push(json!({
            "kind": "task",
            "id": t.id,
            "project": t.project,
            "project_name": v.project_name(&t.project),
            "title": t.title,
            "state": state_tag(&t.state)?,
            "reason": t.reason,
            "question": t.question,
            "command": format!("dvb {verb} {}", t.id),
        }));
    }
    for g in &v.parked {
        items.push(json!({
            "kind": "plan",
            "id": g.id,
            "project": g.project,
            "project_name": v.project_name(&g.project),
            "title": goal_title(g),
            "command": format!("dvb resume {}", g.id),
        }));
    }
    for p in &v.paused {
        items.push(json!({
            "kind": "project",
            "id": p.id,
            "project": p.id,
            "project_name": p.name,
            "title": p.name,
            "command": format!("dvb resume {}", p.name),
        }));
    }
    for q in v.own_questions() {
        items.push(json!({
            "kind": "question",
            "id": q.id,
            "project": q.project,
            "project_name": v.project_name(&q.project),
            "title": q.subject,
            "command": format!("dvb answer {} <n>", q.id),
        }));
    }
    out["needs_you_items"] = items.into();
    if let Some(l) = &v.listing {
        out["project"] = json!({
            "id": l.project.id,
            "name": l.project.name,
            "paused": l.project.paused,
        });
        out["goals"] = l
            .goals
            .iter()
            .map(|g| {
                json!({
                    "id": g.id,
                    "state": g.state,
                    "text": g.text,
                    "plan_parked": g.plan_parked,
                    "waits_for": g.waits_for,
                })
            })
            .collect::<Vec<_>>()
            .into();
        out["finished_goals_hidden"] = l.omitted.into();
        out["tasks"] = l
            .tasks
            .iter()
            .map(|t| task_json(t, &l.roles))
            .collect::<Result<Vec<_>, CliError>>()?
            .into();
    }
    Ok(out)
}

/// A task as `dvb status <project> --json` lists it, with its role's slug from `roles`.
fn task_json(t: &TaskView, roles: &[RoleRecord]) -> Result<serde_json::Value, CliError> {
    Ok(serde_json::json!({
        "id": t.id,
        "goal": t.goal,
        "state": state_tag(&t.state)?,
        "title": t.title,
        "role": t.role,
        "role_name": role_name(roles, &t.role),
        "pr": t.pr,
        "reason": t.reason.as_deref().or_else(|| failure(&t.state)),
        "waiting_for": t.waiting_for,
    }))
}

/// A role's slug, as `dvb role list` prints it; `None` for a role no longer listed.
fn role_name<'a>(roles: &'a [RoleRecord], id: &RoleId) -> Option<&'a str> {
    roles.iter().find(|r| &r.id == id).map(|r| r.slug.as_str())
}

/// `--json` of a verb that changed a task: `{"task": …}`, as status lists it.
async fn write_task_json(client: &Client, t: &TaskView) -> Result<(), CliError> {
    let roles: Vec<RoleRecord> = client
        .get(&format!("/projects/{}/roles", t.project))
        .await?;
    write_json(&serde_json::json!({"task": task_json(t, &roles)?}))
}

/// `--json` of a verb that paused or resumed a project: `{"project": …}`, as
/// `dvb project list --json` lists it, read after the change.
async fn write_project_json(client: &Client, id: &dvb_core::ProjectId) -> Result<(), CliError> {
    let projects: Vec<ProjectView> = client.get("/projects").await?;
    let p = projects
        .into_iter()
        .find(|p| &p.id == id)
        .ok_or_else(|| not_found(format!("no project {id}")))?;
    write_json(&serde_json::json!({"project": p}))
}

fn failure(state: &TaskState) -> Option<&str> {
    if let TaskState::Failed { reason } = state {
        Some(reason)
    } else {
        None
    }
}

/// A task state's name, as the engine writes it (`failed`, `in_review`, …).
fn state_tag(state: &TaskState) -> Result<serde_json::Value, CliError> {
    Ok(serde_json::to_value(state).map_err(message)?["state"].take())
}

/// An open question for `--json`: its numbered options as `dvb answer` takes them.
fn question_json(q: &QuestionView) -> serde_json::Value {
    serde_json::json!({
        "id": q.id,
        "project": q.project,
        "goal": q.goal,
        "task": q.task,
        "subject": q.subject,
        "recommended": q.recommended,
        "options": q.options.iter().map(option_json).collect::<Vec<_>>(),
    })
}

/// A numbered option, as `dvb answer <question> <n>` takes it.
fn option_json(o: &dvb_core::event::QuestionOption) -> serde_json::Value {
    serde_json::json!({"id": o.id, "text": o.text, "effect": o.effect})
}

/// `dvb answer --json`: what was answered, the option chosen, and the goal
/// and task the question belongs to.
fn answer_json(q: &QuestionView, answer: &Answer) -> serde_json::Value {
    let option = match answer {
        Answer::Choice(n) => q.options.iter().find(|o| o.id == *n),
        Answer::Reply(_) => None,
    };
    serde_json::json!({
        "question": q.id,
        "subject": q.subject,
        "answer": answer,
        "option": option.map(option_json),
        "goal": q.goal,
        "task": q.task,
        "open": q.open,
    })
}

/// One numbered option of a question, as `dvb answer <question> <n>` takes it.
fn option_line(q: &QuestionView, o: &dvb_core::event::QuestionOption) -> String {
    let rec = if o.id == q.recommended {
        " (recommended)"
    } else {
        ""
    };
    format!("  {}. {} - {}{rec}", o.id, o.text, o.effect)
}

fn local_time(time: dvb_core::Timestamp) -> String {
    chrono::DateTime::from_timestamp_millis(time.0)
        .map(|time| {
            time.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M %Z")
                .to_string()
        })
        .unwrap_or_else(|| "unknown time".into())
}

/// The model switch deadline in the owner's local timezone.
pub fn only_line(only: &dvb_core::model::OnlyModels) -> String {
    dvb_core::wording::render(
        "only.started",
        &serde_json::json!({
            "models": only.models.join(", "), "ends_at": local_time(only.ends_at)
        }),
    )
}

/// A goal's first line.
fn goal_title(g: &GoalView) -> &str {
    g.text.lines().next().unwrap_or_default()
}

/// One `dvb status` line for a goal.
pub fn goal_line(g: &GoalView) -> String {
    let mut chars = goal_title(g).chars();
    let mut text: String = chars.by_ref().take(160).collect();
    if chars.next().is_some() {
        text.push('…');
    }
    let wait = g
        .waits_for
        .as_ref()
        .map(|goal| format!("; waits for {goal}"))
        .unwrap_or_default();
    let state = if g.plan_parked {
        dvb_core::wording::text("goal.plan_parked")
    } else {
        goal_word(g.state)
    };
    format!("Goal {}: {text} - {state}{wait}", g.id)
}

/// One `dvb status` line for a task.
pub fn task_line(t: &TaskView) -> String {
    if !t.waiting_for.is_empty() {
        return format!(
            "Task {}: {} - {}",
            t.id,
            t.title,
            t.reason
                .as_deref()
                .unwrap_or_else(|| dvb_core::wording::text("task.waiting"))
        );
    }
    let reason = t
        .reason
        .as_ref()
        .map(|r| format!(": {r}"))
        .unwrap_or_default();
    format!(
        "Task {}: {} - {}{}",
        t.id,
        t.title,
        state_word(&t.state),
        reason
    )
}

/// One `dvb watch` line: id, kind and feed wording, falling back to the payload.
/// Each newline in the text is shown as ` / ` so every event stays on one line.
pub fn line(ev: &Event) -> String {
    event_line(ev, feed_word(&ev.kind))
}

fn event_line(ev: &Event, text: Option<String>) -> String {
    let v = serde_json::to_value(&ev.kind).unwrap_or_default();
    let mut payload = v
        .get("payload")
        .map(ToString::to_string)
        .unwrap_or_default();
    if payload.len() > 200 {
        payload.truncate(dvb_core::text::clip(&payload, 200).len());
        payload.push('…');
    }
    format!(
        "{:>5}  {:<18} {}",
        ev.id,
        ev.kind.kind(),
        text.unwrap_or(payload)
            .replace("\r\n", "\n")
            .replace('\n', " / ")
    )
}

fn feed_word(kind: &EventKind) -> Option<String> {
    dvb_core::wording::event_text(kind, local_time)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dvb_core::api::{CommitIdentity, UsageRow, VendorWindow};
    use dvb_core::{ProjectId, RoleId, Timestamp, Vendor};

    #[test]
    fn status_names_the_running_mirror_pass_and_its_project() {
        let project: ProjectView = serde_json::from_value(serde_json::json!({
            "id": "01M4", "name": "example", "repo": "org/example",
            "default_branch": "main", "ci": "no_ci", "paused": false,
        }))
        .unwrap();
        let started = Timestamp(1_791_990_000_000);
        let mut view = StatusView {
            status: Status {
                running: 0,
                needs_you: 0,
                warnings: vec![],
                rulesets_missing: vec![],
                tasks: vec![],
                mirror_pass: Some(dvb_core::api::MirrorPass {
                    started,
                    project: Some(ProjectId::new("01M4")),
                }),
            },
            only: None,
            listing: None,
            questions: vec![],
            parked: vec![],
            paused: vec![],
            projects: vec![project],
        };
        assert!(
            status_text(&view)
                .contains("GitHub mirror pass running since 15:00 UTC, on project example\n"),
            "{}",
            status_text(&view)
        );
        assert_eq!(
            status_json(&view).unwrap()["mirror_pass"],
            serde_json::json!({"started": 1_791_990_000_000_i64, "project": "01M4", "project_name": "example"})
        );
        view.status.mirror_pass.as_mut().unwrap().project = None;
        assert!(status_text(&view).contains("GitHub mirror pass running since 15:00 UTC\n"));
        assert!(!status_text(&view).contains("on project"));
        assert_eq!(
            status_json(&view).unwrap()["mirror_pass"],
            serde_json::json!({"started": 1_791_990_000_000_i64, "project": null, "project_name": null})
        );
    }

    #[test]
    fn project_removal_gives_a_next_step_only_for_the_project_toolchain_cache() {
        let project = ProjectId::new("01M4");
        for root in ["/var/lib/dvb", "/tmp/dev engine's"] {
            let path = format!("{root}/mirrors/.cache/{project}");
            let left = format!("{path}: Permission denied (os error 13)");
            assert_eq!(
                project_not_freed(&left, &project),
                format!(
                    "dvb: not freed: {left}\ndvb: This folder was made by an older dvb and can be deleted as root: sudo rm -rf {}",
                    shell_word(&path)
                )
            );
        }
        for path in [
            "/var/lib/dvb/work/run",
            "/var/lib/dvb/mirrors/example",
            "/var/lib/dvb/mirrors/.cache/other",
            "/var/lib/dvb/mirrors/.cache-other/01M4",
            "github-mirror.json",
        ] {
            let left = format!("{path}: Permission denied (os error 13)");
            assert_eq!(
                project_not_freed(&left, &project),
                format!("dvb: not freed: {left}")
            );
        }
    }

    #[test]
    fn watch_answers_limits_and_preflight_use_plain_words() {
        assert_eq!(
            feed_word(&EventKind::QuestionAnswered {
                question: dvb_core::QuestionId::new("q"),
                answer: Answer::Choice(2),
            }),
            Some("Answered: option 2".into())
        );
        let limited = feed_word(&EventKind::VendorLimited {
            vendor: Vendor::Codex,
            resets_at: Some(Timestamp(1_791_046_706_000)),
        })
        .unwrap();
        assert!(limited.contains("resumes at "), "{limited}");
        assert!(
            !limited.contains("Timestamp") && !limited.contains("1791046706000"),
            "{limited}"
        );
        let checks = vec![dvb_core::event::PreflightCheck {
            code: dvb_core::event::PreflightCode::PfUid,
            ok: false,
            detail: "the worker must run as its assigned user".into(),
        }];
        assert_eq!(
            feed_word(&EventKind::RunPreflight {
                run: dvb_core::RunId::new("r"),
                checks,
                cli_version: None,
                boundary: "worker unit".into(),
            }),
            Some("Preflight refused: the worker must run as its assigned user".into())
        );
    }

    #[test]
    fn watch_prints_multi_line_text_on_one_line() {
        let ev = Event {
            id: dvb_core::EventId(7),
            at: Timestamp(0),
            actor: dvb_core::Actor::Engine,
            project: None,
            kind: EventKind::GoalStated {
                goal: dvb_core::GoalId::new("g"),
                text: "first\nsecond\r\nthird".into(),
                from_issue: None,
                issues: Vec::new(),
                after: None,
            },
            evidence: None,
        };
        let watched = line(&ev);
        assert_eq!(watched.lines().count(), 1, "{watched}");
        assert!(
            watched.ends_with("goal.stated        Goal: first / second / third"),
            "{watched}"
        );
    }

    #[derive(clap::Parser)]
    struct Parsed {
        #[command(subcommand)]
        verb: Verb,
    }

    #[test]
    fn publication_flags_change_only_what_is_given_and_as_owner_stays_until_neutral() {
        use clap::Parser;
        let change = |args: &[&str], current: &PublishingIdentity| {
            let parsed =
                Parsed::try_parse_from(["dvb", "settings", "publication"].iter().chain(args))
                    .unwrap();
            let Verb::Settings {
                command:
                    Some(SettingsVerb::Publication {
                        name,
                        email,
                        as_owner,
                        neutral,
                    }),
            } = parsed.verb
            else {
                panic!("not the publication verb");
            };
            publication_change(current, name, email, as_owner, neutral)
        };
        let neutral = PublishingIdentity {
            name: "dvb".into(),
            email: "dvb@dvb.invalid".into(),
            is_default: true,
            as_owner: false,
        };
        assert_eq!(change(&[], &neutral), None);
        let owner = PublicationChange {
            name: "Example Owner".into(),
            email: "owner@example.invalid".into(),
            as_owner: true,
        };
        assert_eq!(
            change(
                &[
                    "--as-owner",
                    "--name",
                    "Example Owner",
                    "--email",
                    "owner@example.invalid"
                ],
                &neutral
            ),
            Some(owner.clone())
        );
        let current = PublishingIdentity {
            name: owner.name.clone(),
            email: owner.email.clone(),
            is_default: false,
            as_owner: true,
        };
        assert_eq!(
            change(&["--email", "other@example.invalid"], &current),
            Some(PublicationChange {
                email: "other@example.invalid".into(),
                ..owner
            })
        );
        assert_eq!(
            change(&["--neutral"], &current),
            Some(PublicationChange {
                name: String::new(),
                email: String::new(),
                as_owner: false,
            })
        );
        assert_eq!(
            change(&["--neutral", "--name", "Example Bot"], &current),
            Some(PublicationChange {
                name: "Example Bot".into(),
                email: String::new(),
                as_owner: false,
            })
        );
        assert!(
            Parsed::try_parse_from(["dvb", "settings", "publication", "--as-owner", "--neutral"])
                .is_err()
        );
    }

    #[test]
    fn goal_after_parses_as_a_goal_verb_not_a_project() {
        use clap::Parser;
        for other in ["g1", "none"] {
            let parsed = Parsed::try_parse_from(["dvb", "goal", "after", "g2", other]).unwrap();
            assert!(matches!(&parsed.verb, Verb::Goal(GoalArgs {
                verb: Some(GoalVerb::After { goal, other: o, .. }), ..
            }) if goal == "g2" && o == other));
        }
        assert!(Parsed::try_parse_from(["dvb", "goal", "after", "g2"]).is_err());
    }

    #[tokio::test]
    async fn socket_project_add_requires_successful_discovery_or_an_explicit_choice() {
        for (discovery, create, no_ci, chosen, expected) in [
            (
                serde_json::json!({"status":"discovered","default_branch":"trunk","head":"abc","checks":[
                    {"name":"rust","status":"pending","run":3,"conclusion":null,"url":null}
                ]}),
                false,
                false,
                vec![],
                Some(CiMode::Expected(vec!["rust".into()])),
            ),
            (
                serde_json::json!({"status":"discovered","default_branch":"trunk","head":"abc","checks":[]}),
                false,
                false,
                vec![],
                None,
            ),
            (
                serde_json::json!({"status":"failed","failure":{"code":"discovery_failed","message":"retry discovery","retryable":true}}),
                false,
                false,
                vec![],
                None,
            ),
            (
                serde_json::json!({"status":"inaccessible","failure":{"code":"repository_inaccessible","message":"check access","retryable":false}}),
                false,
                false,
                vec![],
                None,
            ),
            (
                serde_json::Value::Null,
                false,
                true,
                vec![],
                Some(CiMode::NoCi),
            ),
            (
                serde_json::Value::Null,
                false,
                false,
                vec!["chosen".into()],
                Some(CiMode::Expected(vec!["chosen".into()])),
            ),
            // A new repository has no checks yet: no check names means no CI.
            (
                serde_json::Value::Null,
                true,
                false,
                vec![],
                Some(CiMode::NoCi),
            ),
            (
                serde_json::Value::Null,
                true,
                true,
                vec![],
                Some(CiMode::NoCi),
            ),
            (
                serde_json::Value::Null,
                true,
                false,
                vec!["chosen".into()],
                Some(CiMode::Expected(vec!["chosen".into()])),
            ),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let sock = dir.path().join("owner.sock");
            let listener = tokio::net::UnixListener::bind(&sock).unwrap();
            let mut requests = crate::test_http::requests(listener);
            let expected_ci = expected.clone();
            let confirmed_absent = discovery["status"] == "discovered"
                && discovery["checks"].as_array().is_some_and(Vec::is_empty);
            let server = tokio::spawn(async move {
                if !discovery.is_null() {
                    let (request, response) = requests.recv().await.unwrap();
                    assert_eq!(request.uri(), "/github/checks?repo=example-org%2Fproject");
                    response
                        .send(crate::test_http::json(200, discovery))
                        .unwrap();
                }
                if let Some(ci) = expected_ci {
                    let (request, response) = requests.recv().await.unwrap();
                    assert_eq!(request.uri(), "/version");
                    let mut version = crate::test_http::json(200, serde_json::json!({}));
                    version
                        .headers_mut()
                        .insert("dvb-version", dvb_core::VERSION.parse().unwrap());
                    response.send(version).unwrap();
                    let (request, response) = requests.recv().await.unwrap();
                    assert_eq!(request.uri(), "/projects");
                    let bytes = axum::body::to_bytes(request.into_body(), 1 << 16)
                        .await
                        .unwrap();
                    let body: NewProject = serde_json::from_slice(&bytes).unwrap();
                    assert_eq!(body.ci, ci);
                    assert_eq!(body.create, create);
                    response.send(crate::test_http::json(409, serde_json::json!({"code":"CI_EVIDENCE_MISSING","detail":"test stops after validating request"}))).unwrap();
                }
            });
            let target = Target {
                dev: None,
                sock: Some(sock.clone()),
            };
            let result = run(
                &target,
                &Client::new(sock),
                Verb::Project(ProjectVerb::Add {
                    name: "example".into(),
                    repo: "example-org/project".into(),
                    default_branch: None,
                    create,
                    no_ci,
                    check: chosen,
                    json: false,
                }),
            )
            .await;
            assert!(result.is_err());
            server.await.unwrap();
            if expected.is_none() {
                if confirmed_absent {
                    assert!(matches!(
                        result,
                        Err(CliError::Refused(dvb_core::Refusal {
                            code: dvb_core::RefusalCode::CiEvidenceMissing,
                            ..
                        }))
                    ));
                } else {
                    assert!(matches!(result, Err(CliError::Message(_))));
                }
            }
        }
    }

    #[tokio::test]
    async fn socket_settings_history_encodes_one_query_value() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("owner.sock");
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        let mut requests = crate::test_http::requests(listener);
        let server = tokio::spawn(async move {
            let (request, response) = requests.recv().await.unwrap();
            assert_eq!(request.method(), axum::http::Method::GET);
            let uri = request.uri().to_string();
            response
                .send(crate::test_http::json(
                    200,
                    serde_json::json!({"versions":[]}),
                ))
                .unwrap();
            uri
        });
        let (_, versions) = history(
            &Client::new(sock),
            &Record::Setting {
                key: "a &key=é/#?".into(),
            },
        )
        .await
        .unwrap();
        assert!(versions.is_empty());
        assert_eq!(
            server.await.unwrap(),
            "/settings/history?key=a+%26key%3D%C3%A9%2F%23%3F"
        );
    }

    #[tokio::test]
    async fn login_status_controls_polling_independently_of_english_prompts() {
        use dvb_core::api::LoginStatus;
        let calls = std::cell::RefCell::new(Vec::new());
        let done = finish_login(Vendor::Claude, "abc".into(), |code| {
            calls.borrow_mut().push(code);
            let status = if calls.borrow().len() < 3 {
                LoginStatus::Pending
            } else {
                LoginStatus::LoggedIn
            };
            async move {
                Ok(LoginStarted {
                    run: dvb_core::RunId::new("r"),
                    url: String::new(),
                    prompt: "Authorization progress".into(),
                    status,
                })
            }
        })
        .await
        .unwrap();
        assert_eq!(done.status, LoginStatus::LoggedIn);
        assert_eq!(*calls.borrow(), ["abc", "", ""]);
        let mut failure_polls = 0;
        let failed = finish_login(Vendor::Codex, String::new(), |_| {
            failure_polls += 1;
            async move {
                Ok(LoginStarted {
                    status: LoginStatus::Failed,
                    run: dvb_core::RunId::new("r"),
                    url: String::new(),
                    prompt: "Denied".into(),
                })
            }
        })
        .await
        .unwrap();
        assert_eq!(failed.status, LoginStatus::Failed);
        assert_eq!(failure_polls, 1);
    }

    #[tokio::test]
    async fn empty_claude_code_stops_before_sending() {
        for code in ["", "\n", "  \t\n"] {
            let result = finish_login(Vendor::Claude, code.into(), |_| async {
                panic!("an empty Claude code must never be sent")
            })
            .await;
            assert_eq!(
                result.unwrap_err().to_string(),
                "No code entered; run dvb login claude again."
            );
        }
    }

    #[tokio::test]
    async fn every_pending_round_prints_the_engine_prompt() {
        use dvb_core::api::LoginStatus;
        let mut rounds = 0;
        let mut prompts = Vec::new();
        let done = finish_login_with_output(
            Vendor::Claude,
            "code".into(),
            |_| {
                rounds += 1;
                let status = if rounds < 4 {
                    LoginStatus::Pending
                } else {
                    LoginStatus::Failed
                };
                async move {
                    Ok(LoginStarted {
                        run: dvb_core::RunId::new("r"),
                        url: String::new(),
                        prompt: if status == LoginStatus::Pending {
                            "Still waiting for the login to finish."
                        } else {
                            "the code was not accepted; run dvb login claude again"
                        }
                        .into(),
                        status,
                    })
                }
            },
            |prompt| {
                prompts.push(prompt.to_owned());
                Ok(())
            },
        )
        .await
        .unwrap();
        assert_eq!(prompts, vec!["Still waiting for the login to finish."; 3]);
        assert_eq!(done.status, LoginStatus::Failed);
        assert_eq!(rounds, 4);
    }

    #[test]
    fn the_usage_view_lists_rows_then_windows() {
        let u = Usage {
            rows: vec![UsageRow {
                project: ProjectId::new("p"),
                role: RoleId::new("backend"),
                model: "claude-sonnet-5-5".into(),
                vendor: Vendor::Claude,
                uncached_in: 1000,
                cached_in: 5000,
                out: 200,
                est_cost: 0.0123,
            }],
            windows: vec![VendorWindow {
                vendor: Vendor::Codex,
                used_pct: 100.0,
                window: "7d".into(),
                resets_at: Some(Timestamp(1_791_046_706_000)),
                limited: true,
            }],
        };
        let text = format_usage(&u);
        assert!(
            text.contains("claude-sonnet-5-5  in 1000 (+5000 cached)  out 200  about $0.01"),
            "{text}"
        );
        assert!(text.contains("codex: 100% of the 7d window"), "{text}");
        assert!(text.contains("limit reached"), "{text}");
        assert!(
            !text.contains("1791046706000"),
            "window resets use local times: {text}"
        );
        assert_eq!(
            format_usage(&Usage {
                rows: vec![],
                windows: vec![]
            }),
            "No usage yet.\n"
        );
    }

    #[test]
    fn publication_lines_name_the_identity_the_default_and_the_switch() {
        let identity = |name: &str, email: &str, is_default| PublishingIdentity {
            name: name.into(),
            email: email.into(),
            is_default,
            as_owner: false,
        };
        assert_eq!(
            publication_setting_text(&identity("dvb", "dvb@dvb.invalid", true)),
            "dvb publishes commits as dvb <dvb@dvb.invalid> (the default)"
        );
        assert_eq!(
            publication_setting_text(&identity("Example Bot", "bot@example.invalid", false)),
            "dvb publishes commits as Example Bot <bot@example.invalid>"
        );
        assert_eq!(
            publication_setting_text(&PublishingIdentity {
                as_owner: true,
                ..identity("Example Owner", "owner@example.invalid", false)
            }),
            "dvb publishes commits as Example Owner <owner@example.invalid>, as you: they count on that GitHub account"
        );
        let mismatch = |name: &str| PublicationMismatch {
            publishes_as: CommitIdentity {
                name: "dvb".into(),
                email: "dvb@dvb.invalid".into(),
            },
            recent: CommitIdentity {
                name: name.into(),
                email: "owner@example.com".into(),
            },
            recent_is_owner: false,
        };
        assert_eq!(
            mismatch_text(&mismatch("example-owner")),
            "dvb publishes commits as dvb <dvb@dvb.invalid>, but this repository's recent dvb commits are by example-owner <owner@example.com>; to switch: dvb settings publication --name example-owner --email owner@example.com"
        );
        assert!(
            mismatch_text(&mismatch("O'Brien Bot"))
                .ends_with(r"--name 'O'\''Brien Bot' --email owner@example.com")
        );
        assert_eq!(
            mismatch_text(&PublicationMismatch {
                recent_is_owner: true,
                ..mismatch("Example Owner")
            }),
            "dvb publishes commits as dvb <dvb@dvb.invalid>, but this repository's recent dvb commits are by Example Owner <owner@example.com>"
        );
    }

    #[tokio::test]
    async fn unknown_setting_is_rejected_by_cli_before_socket_io() {
        let dir = tempfile::tempdir().unwrap();
        let target = Target {
            dev: Some(dir.path().into()),
            sock: None,
        };
        let client = Client::new(target.sock());
        let error = run(
            &target,
            &client,
            Verb::Settings {
                command: Some(SettingsVerb::Set {
                    key: "unknown_setting".into(),
                    value: "true".into(),
                }),
            },
        )
        .await
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("unknown variant `unknown_setting`"),
            "{error}"
        );
    }
    #[tokio::test]
    async fn cli_failure_exits_1() {
        let dir = tempfile::tempdir().unwrap();
        let target = Target {
            dev: Some(dir.path().into()),
            sock: None,
        };
        assert_eq!(
            main(
                &target,
                Verb::Settings {
                    command: Some(SettingsVerb::Get)
                }
            )
            .await,
            1
        );
    }
}

#[cfg(test)]
mod keybind_tests {
    use super::*;

    #[test]
    fn the_keybinding_is_added_once_and_the_rest_of_the_file_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("bindings.lua");
        std::fs::write(&f, "-- mine\no.bind(\"SUPER + X\", nil, \"x\")").unwrap();
        assert_eq!(add_keybind(&f, DEFAULT_KEYS), Ok(Keybind::Added));
        let once = std::fs::read_to_string(&f).unwrap();
        assert!(once.starts_with("-- mine\no.bind(\"SUPER + X\", nil, \"x\")\n"));
        assert!(once.ends_with("o.bind(\"SUPER + ALT + D\", \"dvb\", \"dvb app\")\n"));
        assert_eq!(add_keybind(&f, DEFAULT_KEYS), Ok(Keybind::AlreadyThere));
        assert_eq!(std::fs::read_to_string(&f).unwrap(), once);
    }

    #[test]
    fn invalid_keys_leave_an_existing_unmarked_file_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("bindings.lua");
        let contents = "-- mine\no.bind(\"SUPER + X\", nil, \"x\")";
        std::fs::write(&file, contents).unwrap();
        for keys in ["a\"b", "a\nb"] {
            assert_eq!(
                add_keybind(&file, keys),
                Err(format!("not a key combination: {keys}"))
            );
            assert_eq!(std::fs::read_to_string(&file).unwrap(), contents);
        }
    }

    #[test]
    fn no_bindings_file_is_an_error_not_a_new_file() {
        let dir = tempfile::tempdir().unwrap();
        assert!(add_keybind(&dir.path().join("nope.lua"), DEFAULT_KEYS).is_err());
        assert!(!dir.path().join("nope.lua").exists());
    }
}

#[cfg(test)]
#[path = "../../../tests/support/http.rs"]
pub mod test_http;
