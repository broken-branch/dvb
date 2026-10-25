//! The common outbound-text scan. The engine scans text before it calls a GitHub
//! writer; the forge itself scans only check log excerpts. Identity values stay
//! in engine memory.
use regex::Regex;
use serde::Deserialize;
use std::borrow::Cow;
use std::sync::OnceLock;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub file: String,
    pub line: u32,
    pub rule: String,
}

/// The owner's name and email are rules only when set: the engine sets them for
/// what dvb writes to GitHub itself (commit identity and message, PR title and
/// body, milestone title and description, board (project) item title, role and
/// model). `ScanPolicy::default()`, the secret rules alone,
/// scans owner-typed config, commit content, issue text shown to runs and
/// check log excerpts.
#[derive(Clone, Default)]
pub struct ScanPolicy {
    pub owner_name: String,
    pub owner_email: String,
}

#[derive(Deserialize)]
struct Rules {
    rule: Vec<Rule>,
}
#[derive(Deserialize)]
struct Rule {
    id: String,
    regex: String,
}

#[expect(
    clippy::expect_used,
    reason = "Embedded release scan policy and regexes are build-time constants verified by scan \
                tests; invalid policy must fail closed"
)]
fn rules() -> &'static [(String, Regex)] {
    static RULES: OnceLock<Vec<(String, Regex)>> = OnceLock::new();
    RULES.get_or_init(|| {
        let parsed: Rules = toml::from_str(include_str!("../../../prompts/scan-rules.toml"))
            .expect("invalid release scan rules");
        parsed
            .rule
            .into_iter()
            .map(|r| {
                (
                    r.id,
                    Regex::new(&r.regex).expect("invalid release scan regex"),
                )
            })
            .collect()
    })
}

/// `text` with each run's work directory, `/var/lib/dvb/work/<ULID>`, written
/// as `/var/lib/dvb/work/<run>`: a run id means nothing on GitHub, and a path
/// in a PR is no reason to hold it back.
#[expect(
    clippy::expect_used,
    reason = "A constant regex verified by the scan tests"
)]
pub fn without_run_ids(text: &str) -> Cow<'_, str> {
    static RUN_DIR: OnceLock<Regex> = OnceLock::new();
    RUN_DIR
        .get_or_init(|| {
            Regex::new(r"/var/lib/dvb/work/[0-9A-HJKMNP-TV-Z]{26}\b")
                .expect("invalid run dir regex")
        })
        .replace_all(text, "/var/lib/dvb/work/<run>")
}

/// `text` with each line the secret scan matches replaced, never passed on:
/// for text only the owner and the owner's runs read, so the owner's name and
/// email stay.
pub fn scrub(text: &str) -> String {
    text.lines()
        .map(|line| match ScanPolicy::default().scan("text", line) {
            Some(hit) => format!("[line withheld by dvb's secret scan: {}]", hit.rule),
            None => line.to_owned(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

impl ScanPolicy {
    pub fn scan(&self, file: &str, text: &str) -> Option<Hit> {
        self.hits(file, text).into_iter().next()
    }

    /// Return every matching rule so an allowed fixture cannot hide a second leak.
    pub fn hits(&self, file: &str, text: &str) -> Vec<Hit> {
        let mut hits = Vec::new();
        let owner_name = self.owner_name.to_lowercase();
        let owner_email = self.owner_email.to_lowercase();
        for (i, line) in text.lines().enumerate() {
            let lower = line.to_lowercase();
            let mut ids: Vec<&str> = rules()
                .iter()
                .filter(|(_, re)| re.is_match(line))
                .map(|(id, _)| id.as_str())
                .collect();
            if !owner_email.is_empty() && lower.contains(&owner_email) {
                ids.push("owner-email");
            }
            if !owner_name.is_empty() && lower.contains(&owner_name) {
                ids.push("owner-name");
            }
            for rule in ids {
                hits.push(Hit {
                    file: file.into(),
                    line: u32::try_from(i + 1).unwrap_or(u32::MAX),
                    rule: rule.into(),
                });
            }
        }
        hits
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_work_dirs_pass_the_scan_and_lose_their_run_id() {
        let run = "01M59VRF3PQDEY1AQYY29F3TS6";
        for suffix in ["", "/ws/node_modules/.cache/file"] {
            let text = format!("cache:\n/var/lib/dvb/work/{run}{suffix}");
            assert!(ScanPolicy::default().hits("outbound", &text).is_empty());
            assert_eq!(
                without_run_ids(&text),
                format!("cache:\n/var/lib/dvb/work/<run>{suffix}")
            );
        }
        assert_eq!(
            without_run_ids("/var/lib/dvb/work/%i"),
            "/var/lib/dvb/work/%i"
        );
    }

    #[test]
    fn documented_paths_and_project_home_paths_pass() {
        let policy = ScanPolicy::default();
        for text in [
            "/var/lib/dvb/work/%i",
            "/var/lib/dvb/work/...",
            "/var/lib/dvb/work/<run>",
            "/var/lib/dvb/state/...",
            "/var/lib/dvb",
            "/home/app",
            "/root/",
        ] {
            assert!(policy.hits("outbound", text).is_empty(), "{text}");
        }
    }

    #[test]
    fn secrets_and_literal_identity_share_the_same_scan() {
        let policy = ScanPolicy {
            owner_name: "Example [Owner]".into(),
            owner_email: "owner@example.invalid".into(),
        };
        for (text, rule) in [
            (format!("ghp_{}", "a".repeat(30)), "github-token"),
            ("hello\nExample [Owner]".into(), "owner-name"),
            ("OWNER@example.invalid".into(), "owner-email"),
        ] {
            assert_eq!(policy.scan("outbound", &text).unwrap().rule, rule);
        }
        assert!(policy.scan("outbound", "safe text").is_none());
    }
}
