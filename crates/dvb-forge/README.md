# Gateway and GitHub

`GitHub` implements `dvb_core::forge::Forge` with octocrab 0.54 and its
aws-lc-rs JWT backend. `FakeForge` records operations and optionally uses local Git.
Every test uses the Fake or a loopback wiremock server.

## Install configuration

The engine reads these local files in its state directory:

- `github-app.json`: `app_id`, `installation_id`, `organization`. Only that
  organization is accepted. The wizard supplies it after verifying the installation.
- `github-app.pem`: the App RSA key, a regular engine-owned file. Loading it
  makes it mode 0600 if group or other bits are set, and logs that once. It is
  never sent to a worker or included in an error.
- `publication.json`: optional `name`, `email` (defaults match
  dvb's neutral repository identity), `owner_name`, `owner_email`, and
  `as_owner` (default `false`: the neutral identity may not name the owner). Both owner
  fields are required before enabling the real GitHub gateway. These values stay
  local and supply the literal identity scan. The wizard and `PUT /settings/publication` (`dvb settings publication`) activate these settings immediately; manual file edits require a restart.

No App configuration means no real forge is enabled. Dev mode always uses Fake.
Project add works without a populated model pool; it records the default roles
and adds hires when a pool exists.

API operations cache one installation token and client per repository, with the
App's permissions, refreshing 60 seconds before GitHub's returned expiration.
Repository creation and installation-wide reads use one installation-wide token,
because that repository does not yet exist. Pushes reuse the repository's session
for fresh metadata validation and Git. Git receives it only in
that command's environment; an ephemeral credential helper reads it without a
URL credential or persisted git configuration. Credential helpers, prompts, hooks
and global or system git configuration are disabled. API and git failures do not echo remote response bodies.
Octocrab's automatic request retries are disabled; dispatch owns retries and
reconciliation, especially for a lost merge response.

## Project setup

`dvb project add NAME --repo ORGANIZATION/REPOSITORY --create --check CHECK`
creates a private empty repository. Omit `--create` for an existing repository.
Omit `--check` to use its effective ruleset checks, or supply additional expected
names. An empty expected set refuses unless `--no-ci` was explicitly chosen;
existing required checks still apply even when that flag is supplied.

A newly created repo receives `.dvb/project.json` as one neutral root commit on
its default branch, guarded by an empty-ref lease. This is the sole default-branch
push exception. An existing repo receives no commit, branch or PR. The gateway installs a
ruleset with PR required, no bypass actors, no force push, no deletion and the
expected checks, then reads the effective branch rules back before recording
`project.added`. Unsupported/refused rulesets fail setup rather than silently
weakening it. Setup does not invent CI workflows that report success.

A failed remote setup can leave a private repository behind; it is never
automatically deleted. The one exception is `--create`'s Free-plan probe: a
repository whose rulesets GitHub refuses is deleted while it still has the
identity it was created with and no branch. The default-branch exception is limited to repos
with a durable `repository.created` receipt in the engine event log. After a
restart, `Forge::resume_setup` restores the receipt only if the private remote
repository's numeric identity matches. Recovery reuses the recorded setup root
commit; its default-branch push still requires that matching identity and an
empty-ref lease, so it refuses if another writer has initialized the branch.

## Publication and merge

Publication keeps the recorded run base and collected tree, discards worker
history and supplies both neutral identities. The scan reads changed blobs as
text regardless of binary guesses or worker attributes. Release rules and owner
identity apply to file contents, paths, messages, PR text, repo/check names and
other human text the gateway sends; the engine scans each where it is accepted,
and the gateway sends what the engine gives it.

The optional `.dvb/scan-ignore.toml` is read exclusively from the recorded base:

```toml
[[ignore]]
file = "fixtures/example.txt"
rule = "github-token"
```

Entries allow only an exact file/rule pair. They do not apply to PR text or file
paths. Malformed base configuration refuses the scan, and allowing one rule never
hides another matching rule. A worker cannot allow its own new fixture.

CI polling reads paginated check runs and commit statuses for the exact head.
Actions checks come from the head's `pull_request` workflow suites, so manual,
scheduled and other workflow events cannot replace PR results. Other apps' checks
and commit statuses still come from the commit APIs. Only `success` counts;
skipped checks are missing, neutral runs fail, and unknown responses are unreadable.
Discovery and main-branch jobs also report skipped checks as missing.
Statuses use the newest observation per context. Every observation for an
expected name must succeed; an empty expected set never
counts as success. Polling runs every ten seconds while awaiting checks, stops
when state/head changes, and rechecks evidence before a merge request.

Merges require a clean verdict from the engine's recorded reviewer for that head,
the correct PR base and the review cap. The request carries `sha` and the squash
method, with scanned engine-written commit text. A head mismatch/refused merge
returns to review; the new remote head is fetched and replaces the stale head.
A lost response is reconciled by reading the PR before retry. A merged observation
requires its merge SHA. Cleanup closes the linked issue when the goal's other
tasks are done and deletes the task branch. The task stays Merging until cleanup
succeeds, allowing reconciliation to retry it without merging twice.

## Milestones and organization board

The engine mirrors every project to a private organization-owned Projects v2 board,
linked to its repository. Lookups read the board title, including when using a
saved board ID. Only a changed title is repaired.
Each goal is a milestone; its description contains the
current goal and task progress (only the task progress on a public or internal
repository, read on every pass), every PR receives the milestone, and completion
closes it. Cards start as drafts and become PR cards without creating repository
issues. The board's Status field includes Working, In review, Ready to merge and
Done, with inactive states also named honestly. Role and Model are text fields.

Only engine snapshots supply text, progress and state. Queries recover GitHub object
identities and field option IDs; they never change dvb's plan. Engine-owned
`github-mirror.json` stores board, milestone and card identities so a manual edit
of a title or marker does not create another object on restart. A lost creation
reply is recovered by the engine marker (or by PR identity); a failed draft deletion
is retried after repairing the PR card. Writes use engine-scanned text and fresh,
repository-scoped tokens. Identity/secret hits and API failures produce
`github.mirror_observed` with a visible “Not mirrored on GitHub” mark on the goal
or task. Mirror failures do not cancel or weaken publication or review.

GraphQL refusals include the error type (or `extensions.code`), the first path
field and the message clipped to 200 characters, for example
`GitHub board response is unreadable (organization FORBIDDEN: …)`. A type, code
or field that is not one plain word appears as `?`; later path segments are never
copied. REST validation errors keep the same plain-word filter. Partial data with
errors is still refused.

Mirroring runs on startup and relevant engine events, coalesces event bursts,
passes over every project every five minutes and backs off failed project, goal and
task writes from one minute, doubling up to one hour. Success or changed mirror
content resets that mirror’s backoff. Retry deadlines use the engine clock and
are kept in memory. A milestone or card is sent only when its content differs
from its last receipt, a quiet `github.mirror_sent` event with the digest of
what GitHub confirmed (and the milestone number), so passes after a restart or
update send nothing for unchanged and finished goals. A failed send drops the
item's receipt, so it is sent again even if its content returns to the last one.
A manual edit of a milestone or card stays until dvb's item changes.
Run output does not trigger GitHub writes. Mirror and App setup effects also use
the update protocol’s gate, so an update snapshots only after their writes finish. The Project heading shows No CI and
its cards explicitly retain Review required.

## App wizard

The setup page offers Create App, then Install App.
The organization and the owner's identity for the publication scan are sent to the
engine over the desk bridge; the identity is excluded from the manifest. The desk
supplies a random callback state and its exact loopback callback URL. It accepts
external callbacks by that state, independently of the browser's SameSite cookie,
and returns to setup with no code or key in the page or redirect. Starting setup
requires the normal desk Host, Origin and session checks.

The engine prepares the private, webhook-free manifest, exchanges the code, verifies
App/organization ownership, and atomically writes mode-0600 engine-owned files.
`github-app.pending.json` holds the converted App and key until installation, so a
restart or a crash while writing `github-app.pem` does not consume the code twice.
The installation must match the App, organization and all requested permissions.
Only then are `github-app.json` and `publication.json` installed, the gateway is
activated without a restart, and the pending file is removed. Callback state is
checked before exchange; an unconverted code expires after an hour. Fake/dev mode
refuses real credential setup. Neither App secrets nor local identity enter events,
workers or error bodies.

## Tests

Tests use Fake/local Git or recorded loopback responses. See the [code map](../../docs/map/forge.md)
for fixtures and flows, and the [testing guide](../../docs/testing.md) for gates.
`cargo nextest run --locked --offline -p dvb-engine --test github_mirror`
drives two fixture PRs through the board, closes their shared milestone, overwrites manual card
edits on the task's next change, sends nothing for unchanged items, also after a restart, and
clears a scan failure after restarting from the event log.
