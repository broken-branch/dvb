# Security

## Reporting a vulnerability

Report vulnerabilities privately through GitHub's
[Report a vulnerability](https://github.com/broken-branch/dvb/security/advisories/new)
form (a private security advisory), not in a public issue.

## Supported versions

Fixes go to the latest release. Install it with `dvb update`.
The public signing key is in [release.pub](crates/dvb-engine/src/update/release.pub); see [Check a release](README.md#check-a-release) for verification.

## Projects on one install

Runs of all projects share the Claude and Codex login directories, so a project
you don't trust shouldn't share a dvb install with projects you do.
