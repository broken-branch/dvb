//! Every App/installation request goes through the same not-before gate.
use bytes::Bytes;
use dvb_core::forge::RateLimit;
use dvb_core::{Clock, SystemClock, Timestamp};
use http::{Method, Response};
use http_body_util::combinators::BoxBody;
use octocrab::{FromResponse, Octocrab};
use serde::Serialize;
use std::sync::{Arc, Mutex};

type Raw = Response<BoxBody<Bytes, octocrab::Error>>;

pub struct Wait {
    pub(super) clock: Arc<dyn Clock>,
    pub(super) deadline: Mutex<Option<(Timestamp, RateLimit)>>,
}
impl std::fmt::Debug for Wait {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Wait").finish_non_exhaustive()
    }
}
impl Default for Wait {
    fn default() -> Self {
        Self::new(Arc::new(SystemClock))
    }
}
impl Wait {
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            clock,
            deadline: Mutex::new(None),
        }
    }
    pub(super) fn check(&self) -> Result<(), Error> {
        let now = self.clock.now();
        if let Some((at, limit)) = *self.deadline.lock().unwrap_or_else(dvb_core::sync::recover)
            && now < at
        {
            return Err(Error::Wait(limit, None));
        }
        Ok(())
    }
    pub(super) fn record(&self, limit: RateLimit, now: Timestamp) {
        let at = limit.not_before(now, 2_000);
        let mut deadline = self.deadline.lock().unwrap_or_else(dvb_core::sync::recover);
        if deadline.is_none_or(|(old, _)| at > old) {
            *deadline = Some((at, limit));
        }
    }
}

#[derive(Debug)]
pub(super) enum Error {
    Api(octocrab::Error),
    Wait(RateLimit, Option<u16>),
}
impl From<octocrab::Error> for Error {
    fn from(error: octocrab::Error) -> Self {
        Self::Api(error)
    }
}
impl Error {
    pub fn api(&self) -> Option<&octocrab::Error> {
        match self {
            Self::Api(error) => Some(error),
            Self::Wait(_, _) => None,
        }
    }
}

#[derive(Clone)]
pub struct Client {
    pub(super) api: Octocrab,
    pub(super) wait: Arc<Wait>,
}
impl Client {
    async fn raw<B: Serialize + ?Sized>(
        &self,
        method: Method,
        route: impl AsRef<str>,
        body: Option<&B>,
    ) -> Result<Raw, Error> {
        self.wait.check()?;
        let request = self.api.build_request(
            http::Request::builder().method(method).uri(route.as_ref()),
            body,
        )?;
        let response = self.api.execute(request).await?;
        let status = response.status().as_u16();
        if matches!(status, 403 | 429) {
            let now = self.wait.clock.now();
            let header = |name: &str| response.headers().get(name).and_then(|v| v.to_str().ok());
            let [resource, used, remaining, reset_header] = [
                "x-ratelimit-resource",
                "x-ratelimit-used",
                "x-ratelimit-remaining",
                "x-ratelimit-reset",
            ]
            .map(|name| header(name).unwrap_or("<absent>").to_owned());
            let reset = header("retry-after")
                .and_then(|v| v.parse::<i64>().ok())
                .filter(|v| *v >= 0)
                .map(|seconds| now.plus_millis(seconds.saturating_mul(1_000)))
                .or_else(|| {
                    (header("x-ratelimit-remaining") == Some("0"))
                        .then(|| {
                            header("x-ratelimit-reset")
                                .and_then(|v| v.parse::<i64>().ok())
                                .map(|seconds| Timestamp(seconds.saturating_mul(1_000)))
                        })
                        .flatten()
                });
            let error = octocrab::map_github_error(response).await;
            let limited = status == 429
                || error.as_ref().err().is_some_and(|error| {
                    matches!(
                        error,
                        octocrab::Error::GitHub { source, .. }
                            if super::github::limit_reason(status, &source.message)
                                == super::github::LimitReason::RateLimit
                    )
                });
            if limited {
                // Only these response headers; debug formatting keeps values on one line.
                eprintln!(
                    "dvb forge: GitHub HTTP {status} (rate limit): x-ratelimit-resource={resource:?} \
                x-ratelimit-used={used:?} x-ratelimit-remaining={remaining:?} \
                x-ratelimit-reset={reset_header:?}"
                );
                let limit = RateLimit { reset };
                self.wait.record(limit, now);
                return Err(Error::Wait(limit, Some(status)));
            }
            return error.map_err(Error::Api);
        }
        Ok(response)
    }
    async fn json<B: Serialize + ?Sized>(
        &self,
        method: Method,
        route: impl AsRef<str>,
        body: Option<&B>,
    ) -> Result<serde_json::Value, Error> {
        let response = self.raw(method, route, body).await?;
        Ok(serde_json::Value::from_response(octocrab::map_github_error(response).await?).await?)
    }
    pub(super) async fn get(&self, route: impl AsRef<str>) -> Result<serde_json::Value, Error> {
        self.json(Method::GET, route, None::<&()>).await
    }
    pub(super) async fn post<B: Serialize + ?Sized>(
        &self,
        route: impl AsRef<str>,
        body: Option<&B>,
    ) -> Result<serde_json::Value, Error> {
        self.json(Method::POST, route, body).await
    }
    pub(super) async fn patch<B: Serialize + ?Sized>(
        &self,
        route: impl AsRef<str>,
        body: Option<&B>,
    ) -> Result<serde_json::Value, Error> {
        self.json(Method::PATCH, route, body).await
    }
    pub(super) async fn put<B: Serialize + ?Sized>(
        &self,
        route: impl AsRef<str>,
        body: Option<&B>,
    ) -> Result<serde_json::Value, Error> {
        self.json(Method::PUT, route, body).await
    }
    pub(super) async fn post_raw(&self, route: impl AsRef<str>) -> Result<Raw, Error> {
        self.raw(Method::POST, route, None::<&()>).await
    }
    pub(super) async fn delete_raw(&self, route: impl AsRef<str>) -> Result<Raw, Error> {
        self.raw(Method::DELETE, route, None::<&()>).await
    }
    pub(super) async fn get_raw(&self, route: impl AsRef<str>) -> Result<Raw, Error> {
        self.raw(Method::GET, route, None::<&()>).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dvb_core::FakeClock;
    use http_body_util::Full;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn fixture(
        responses: Vec<Response<Full<Bytes>>>,
    ) -> (Client, Arc<FakeClock>, Arc<AtomicUsize>) {
        let responses = Arc::new(Mutex::new(VecDeque::from(responses)));
        let count = Arc::new(AtomicUsize::new(0));
        let requests = count.clone();
        let service = tower::service_fn(move |_: http::Request<octocrab::OctoBody>| {
            requests.fetch_add(1, Ordering::SeqCst);
            let response = responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected request");
            async move { Ok::<_, std::convert::Infallible>(response) }
        });
        let api = octocrab::OctocrabBuilder::new_empty()
            .with_service(service)
            .with_auth(octocrab::AuthState::None)
            .build()
            .unwrap();
        let clock = Arc::new(FakeClock::at(Timestamp(10_000)));
        let wait = Arc::new(Wait {
            clock: clock.clone(),
            deadline: Mutex::new(None),
        });
        (Client { api, wait }, clock, count)
    }

    fn response(status: u16, headers: &[(&str, &str)]) -> Response<Full<Bytes>> {
        let mut response = Response::builder().status(status);
        for (key, value) in headers {
            response = response.header(*key, *value);
        }
        response
            .body(Full::new(Bytes::from_static(
                br#"{"message":"API rate limit exceeded"}"#,
            )))
            .unwrap()
    }

    #[tokio::test]
    async fn rate_limit_diagnostics_log_one_safe_header_line_per_response() {
        const CASE_ENV: &str = "DVB_TEST_RATE_DIAGNOSTIC";
        let cases = [
            (429, "private-body", true),
            (403, "API rate limit exceeded private-body", true),
            (403, "abuse detection private-body", true),
            (429, "private-body", false),
            (403, "API rate limit exceeded private-body", false),
            (
                403,
                "Resource not accessible by integration private-body",
                true,
            ),
        ];
        if let Ok(case) = std::env::var(CASE_ENV) {
            let (status, message, headers) = cases[case.parse::<usize>().unwrap()];
            let mut limited = response(
                status,
                if headers {
                    &[
                        ("x-ratelimit-resource", "core"),
                        ("x-ratelimit-used", "5000"),
                        ("x-ratelimit-remaining", "0"),
                        ("x-ratelimit-reset", "20"),
                    ]
                } else {
                    &[]
                },
            );
            *limited.body_mut() = Full::new(Bytes::from(
                serde_json::json!({"message":format!("{message} ghs_offline-secret")}).to_string(),
            ));
            let (mut client, _, count) = fixture(vec![limited, response(200, &[])]);
            client.api = client.api.user_access_token("ghs_offline-secret").unwrap();
            let error = super::super::github::api_error(
                client.post("/graphql", None::<&()>).await.unwrap_err(),
            );
            if message.starts_with("Resource") {
                assert_eq!(error.detail, "GitHub HTTP 403 (App permission missing)");
                assert!(error.rate_limit.is_none());
                client.get("/repos/org/repo").await.unwrap();
                assert_eq!(count.load(Ordering::SeqCst), 2);
            } else {
                assert_eq!(error.detail, format!("GitHub HTTP {status} (rate limit)"));
                assert!(error.rate_limit.is_some());
                // A blocked request is not an actual response and must log nothing.
                assert!(client.get("/repos/org/repo").await.is_err());
                assert_eq!(count.load(Ordering::SeqCst), 1);
            }
            return;
        }
        // Capture the real stderr emission without global logger/capture races.
        for (case, (status, message, headers)) in cases.into_iter().enumerate() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "gateway::tests::rate_limit_diagnostics_log_one_safe_header_line_per_response",
                    "--nocapture",
                ])
                .env(CASE_ENV, case.to_string())
                .output()
                .unwrap();
            assert!(output.status.success(), "{output:?}");
            let log = String::from_utf8(output.stderr).unwrap();
            assert!(!log.contains("private-body"), "{log}");
            assert!(!log.contains("ghs_offline-secret"), "{log}");
            if message.starts_with("Resource") {
                assert!(log.is_empty(), "{log}");
            } else {
                let [resource, used, remaining, reset] = if headers {
                    ["core", "5000", "0", "20"]
                } else {
                    ["<absent>"; 4]
                };
                assert_eq!(
                    log,
                    format!(
                        "dvb forge: GitHub HTTP {status} (rate limit): x-ratelimit-resource={resource:?} \
                x-ratelimit-used={used:?} x-ratelimit-remaining={remaining:?} \
                x-ratelimit-reset={reset:?}\n"
                    )
                );
            }
        }
    }

    #[tokio::test]
    async fn retry_after_429_blocks_every_request_until_reset_and_margin() {
        let (client, clock, count) = fixture(vec![
            response(
                429,
                &[
                    ("retry-after", "7"),
                    ("x-ratelimit-remaining", "0"),
                    ("x-ratelimit-reset", "999"),
                ],
            ),
            response(200, &[]),
        ]);
        let error = super::super::github::api_error(
            client.post("/graphql", None::<&()>).await.unwrap_err(),
        );
        assert_eq!(
            error.rate_limit,
            Some(RateLimit {
                reset: Some(Timestamp(17_000))
            })
        );
        // The App and each installation client share this same gate.
        let other = Client {
            api: client.api.clone(),
            wait: client.wait.clone(),
        };
        clock.set(Timestamp(17_999));
        assert!(
            other
                .get("/app/installations/1/access_tokens")
                .await
                .is_err()
        );
        assert!(
            client
                .patch("/repos/org/repo/pulls/1", Some(&serde_json::json!({})))
                .await
                .is_err()
        );
        assert!(
            client
                .delete_raw("/repos/org/repo/git/refs/heads/dvb/t")
                .await
                .is_err()
        );
        assert_eq!(count.load(Ordering::SeqCst), 1);
        clock.set(Timestamp(18_000));
        other.get("/repos/org/repo/check-runs").await.unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn rate_limited_403_uses_zero_remaining_reset_and_never_bypasses_the_gate() {
        let (client, clock, count) = fixture(vec![
            response(
                403,
                &[("x-ratelimit-remaining", "0"), ("x-ratelimit-reset", "20")],
            ),
            response(200, &[]),
        ]);
        let error = super::super::github::api_error(
            client
                .post("/app/installations/1/access_tokens", None::<&()>)
                .await
                .unwrap_err(),
        );
        assert_eq!(
            error.rate_limit,
            Some(RateLimit {
                reset: Some(Timestamp(20_000))
            })
        );
        clock.set(Timestamp(20_999));
        assert!(client.post("/graphql", None::<&()>).await.is_err());
        assert_eq!(count.load(Ordering::SeqCst), 1);
        clock.set(Timestamp(21_000));
        client
            .put("/repos/org/repo/pulls/1/merge", None::<&()>)
            .await
            .unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_limit_without_timing_headers_uses_backoff() {
        let (client, clock, count) = fixture(vec![response(429, &[]), response(200, &[])]);
        assert!(matches!(
            client.get("/repos/org/repo").await,
            Err(Error::Wait(RateLimit { reset: None }, _))
        ));
        clock.set(Timestamp(11_999));
        assert!(client.get("/repos/org/repo").await.is_err());
        assert_eq!(count.load(Ordering::SeqCst), 1);
        clock.set(Timestamp(12_000));
        client.get("/repos/org/repo").await.unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 2);
    }
}
