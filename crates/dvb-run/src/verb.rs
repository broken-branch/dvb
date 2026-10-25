//! `dvb run <verb> [<json>]`: the client the model's CLI calls. The
//! body is the last argument when it is JSON, otherwise stdin; `issue <number>`
//! and `issues` need no body. The reply is printed on stdout; a refusal exits 1
//! with its code on stderr.

use crate::client;
use std::io::Read;

/// The verb's words and its body from the arguments; None reads stdin.
fn split(args: &[String]) -> (&[String], Option<String>) {
    match args {
        // The read verbs take their argument as words and never wait on stdin.
        [verb, number] if verb == "issue" && number.parse::<u64>().is_ok() => {
            (&args[..1], Some(format!("{{\"number\":{number}}}")))
        }
        [verb] if verb == "issues" => (args, Some("{}".to_owned())),
        [words @ .., last] if last.trim_start().starts_with('{') => (words, Some(last.clone())),
        _ => (args, None),
    }
}

pub fn verb(args: &[String]) -> i32 {
    let (words, body) = match split(args) {
        (words, Some(body)) => (words, body),
        (words, None) => {
            let mut body = String::new();
            if let Err(e) = std::io::stdin().read_to_string(&mut body) {
                eprintln!("dvb run: stdin: {e}");
                return 2;
            }
            (words, body)
        }
    };
    if words.is_empty()
        || words.iter().any(|word| {
            word.is_empty() || !word.bytes().all(|b| b.is_ascii_lowercase() || b == b'-')
        })
    {
        eprintln!("usage: dvb run <verb> [<json body>]  (see prompts/run-verbs.md)");
        return 2;
    }
    let Ok(sock) = std::env::var("DVB_RUN_SOCK") else {
        eprintln!("dvb run: DVB_RUN_SOCK is not set; run verbs work only inside a run");
        return 2;
    };
    let path = format!("/run/{}", words.join("/"));
    let reply = match client::post(sock.as_ref(), &path, &body) {
        Ok(reply) => reply,
        Err(e) => {
            eprintln!("dvb run: {e}");
            return 2;
        }
    };
    if let Err(error) = dvb_cli::write_stdout(format_args!("{}\n", reply.body)) {
        eprintln!("dvb run: {error}");
        return 2;
    }
    match reply {
        reply if reply.ok() => 0,
        reply => {
            let code = serde_json::from_str::<serde_json::Value>(&reply.body)
                .ok()
                .and_then(|v| v["code"].as_str().map(str::to_owned))
                .unwrap_or_else(|| reply.status.to_string());
            eprintln!("dvb run {}: refused: {code}", words.join(" "));
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(args: &[&str]) -> Vec<String> {
        args.iter().map(|a| (*a).to_owned()).collect()
    }

    #[test]
    fn issue_verbs_take_words_and_others_take_json() {
        let args = words(&["issue", "12"]);
        assert_eq!(
            split(&args),
            (&args[..1], Some(r#"{"number":12}"#.to_owned()))
        );
        let args = words(&["issues"]);
        assert_eq!(split(&args), (&args[..], Some("{}".to_owned())));
        let args = words(&["task", "stop", r#"{"task":"t"}"#]);
        assert_eq!(
            split(&args),
            (&args[..2], Some(r#"{"task":"t"}"#.to_owned()))
        );
        let args = words(&["issue", "x"]);
        assert_eq!(split(&args), (&args[..], None));
        let args = words(&["note"]);
        assert_eq!(split(&args), (&args[..], None));
    }
}
