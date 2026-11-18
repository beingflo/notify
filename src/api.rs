//! The HTTP surface: two routes with two different policies, plus `/health`.
//!
//! Both handlers do the same three things — validate, account for the rate
//! limit where one applies, hand the message to the queue — and then answer
//! without waiting for SMTP. The sender task does the talking.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use axum::Json;
use axum::Router;
use axum::extract::{DefaultBodyLimit, State, rejection::JsonRejection};
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE, RETRY_AFTER};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;
use tower_http::cors::{AllowHeaders, AllowMethods, AllowOrigin, CorsLayer};

use crate::config;
use crate::limit::Limiter;
use crate::mail::Mail;

/// The closed set of feedback producers. `dd.rest.quest` and
/// `ledger.rest.quest` are dead and gone; anyone else is a payload we do not
/// understand and answer `422` to.
#[derive(Clone, Copy, Debug, Deserialize)]
pub enum Project {
    #[serde(rename = "marending.dev")]
    MarendingDev,
    #[serde(rename = "rest.quest")]
    RestQuest,
    #[serde(rename = "go.rest.quest")]
    GoRestQuest,
    #[serde(rename = "journal")]
    Journal,
}

impl Project {
    pub fn as_str(self) -> &'static str {
        match self {
            Project::MarendingDev => "marending.dev",
            Project::RestQuest => "rest.quest",
            Project::GoRestQuest => "go.rest.quest",
            Project::Journal => "journal",
        }
    }
}

#[derive(Clone)]
pub struct AppState {
    queue: mpsc::Sender<Mail>,
    limiter: Arc<Limiter>,
    alerts: Arc<Alerts>,
    dry_run: bool,
}

impl AppState {
    pub fn new(queue: mpsc::Sender<Mail>, alert_token: Option<String>, dry_run: bool) -> Self {
        Self {
            queue,
            limiter: Arc::new(Limiter::new()),
            alerts: Arc::new(Alerts::new(alert_token)),
            dry_run,
        }
    }
}

/// Everything `/alert` needs besides the payload: the token to compare against
/// and the flood counter. Separate from `AppState` so the two policies stay
/// visibly separate.
struct Alerts {
    token: Option<String>,
    storm: Mutex<Storm>,
}

struct Storm {
    start: Instant,
    count: u32,
}

impl Alerts {
    fn new(token: Option<String>) -> Self {
        Self {
            token,
            storm: Mutex::new(Storm {
                start: Instant::now(),
                count: 0,
            }),
        }
    }

    /// Authenticates the bearer token, or says why it could not.
    fn authorize(&self, headers: &HeaderMap) -> Result<(), StatusCode> {
        // Fail closed: an unset secret must never mean "no token required".
        let Some(expected) = self.token.as_deref() else {
            return Err(StatusCode::SERVICE_UNAVAILABLE);
        };

        let presented = headers
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "));

        match presented {
            Some(presented) if presented.as_bytes().ct_eq(expected.as_bytes()).into() => Ok(()),
            _ => Err(StatusCode::UNAUTHORIZED),
        }
    }

    /// Counts a request in the current minute and returns the running total.
    /// No limit, no dropping: a flood here is our own client misbehaving, and
    /// throwing the mail away would be the wrong answer to that.
    fn observe(&self) -> u32 {
        // A poisoned counter is still a counter: recover rather than panic, so a
        // panic elsewhere can never turn /alert into a 500 machine.
        let mut storm = self.storm.lock().unwrap_or_else(PoisonError::into_inner);
        if storm.start.elapsed() >= Duration::from_secs(60) {
            *storm = Storm {
                start: Instant::now(),
                count: 0,
            };
        }
        storm.count += 1;
        storm.count
    }
}

#[derive(Deserialize)]
struct FeedbackRequest {
    project: Project,
    /// `marending.dev` sends `path: null`, the rest send `""`. Both are "no path".
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    email: Option<String>,
    content: String,
}

#[derive(Deserialize)]
struct AlertRequest {
    source: String,
    severity: String,
    title: String,
    body: String,
}

#[derive(Serialize)]
struct Health {
    status: &'static str,
    smtp: &'static str,
}

pub fn router(state: AppState) -> Router {
    // CORS is a /feedback concern only: the browser is the only client that
    // needs it, and /alert is server to server.
    let feedback = Router::new()
        .route("/feedback", post(feedback))
        .layer(DefaultBodyLimit::max(config::FEEDBACK_BODY_LIMIT))
        .layer(cors());

    let alerts = Router::new()
        .route("/alert", post(alert))
        .layer(DefaultBodyLimit::max(config::ALERT_BODY_LIMIT));

    feedback
        .merge(alerts)
        .route("/health", get(health))
        .with_state(state)
}

/// Explicit methods and headers, not `any()`: the router only answers `POST`,
/// and the only header a browser needs to send is `Content-Type`.
fn cors() -> CorsLayer {
    let origins: Vec<HeaderValue> = config::CORS_ORIGINS
        .iter()
        .map(|origin| {
            origin
                .parse()
                .expect("CORS_ORIGINS are constants and must parse")
        })
        .collect();

    CorsLayer::new()
        .allow_origin(AllowOrigin::list(origins))
        .allow_methods(AllowMethods::list([Method::POST]))
        .allow_headers(AllowHeaders::list([CONTENT_TYPE]))
        .max_age(Duration::from_secs(86_400))
}

async fn health(State(state): State<AppState>) -> Json<Health> {
    Json(Health {
        status: "ok",
        smtp: if state.dry_run { "dry-run" } else { "live" },
    })
}

async fn feedback(
    State(state): State<AppState>,
    request: Result<Json<FeedbackRequest>, JsonRejection>,
) -> Response {
    // A rejected body (`415`, `413`, `422` from the extractor) is already a
    // response; only a body we understand costs a rate-limit slot.
    let request = match request {
        Ok(Json(request)) => request,
        Err(rejection) => return rejection.into_response(),
    };

    let mail = match build_feedback(request) {
        Ok(mail) => mail,
        Err(why) => return unprocessable(why),
    };

    if let Err(retry_after) = state.limiter.acquire() {
        tracing::info!(retry_after, "feedback rate limited");
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [(RETRY_AFTER, retry_after.to_string())],
            "slow down\n",
        )
            .into_response();
    }

    enqueue(&state, mail)
}

async fn alert(
    State(state): State<AppState>,
    headers: HeaderMap,
    request: Result<Json<AlertRequest>, JsonRejection>,
) -> Response {
    // Authentication first, always: a wrong token must not learn anything about
    // the payload's shape, and a rejected body must not cost an authentication
    // attempt.
    if let Err(status) = state.alerts.authorize(&headers) {
        tracing::warn!(status = status.as_u16(), "alert rejected");
        let why = match status {
            StatusCode::SERVICE_UNAVAILABLE => "alert token is not configured\n",
            _ => "unauthorized\n",
        };
        return (status, why).into_response();
    }

    let seen = state.alerts.observe();
    if seen > config::ALERT_STORM_PER_MINUTE {
        tracing::warn!(per_minute = seen, "alert storm: more than expected");
    }

    let request = match request {
        Ok(Json(request)) => request,
        Err(rejection) => return rejection.into_response(),
    };

    let mail = match build_alert(request) {
        Ok(mail) => mail,
        Err(why) => return unprocessable(why),
    };

    enqueue(&state, mail)
}

fn enqueue(state: &AppState, mail: Mail) -> Response {
    // Either the queue is full (32 waiting for SMTP) or the sender task is
    // gone; both mean the same thing to the caller. `try_send` hands the mail
    // back on failure, which is why it can still be logged here.
    match state.queue.try_send(mail) {
        Ok(()) => StatusCode::OK.into_response(),
        Err(TrySendError::Full(mail)) => {
            tracing::error!(mail = %mail, "not queued: queue is full");
            StatusCode::SERVICE_UNAVAILABLE.into_response()
        }
        Err(TrySendError::Closed(mail)) => {
            tracing::error!(mail = %mail, "not queued: sender task is gone");
            StatusCode::SERVICE_UNAVAILABLE.into_response()
        }
    }
}

fn unprocessable(why: String) -> Response {
    tracing::info!(why = %why, "payload rejected");
    (StatusCode::UNPROCESSABLE_ENTITY, format!("{why}\n")).into_response()
}

fn build_feedback(request: FeedbackRequest) -> Result<Mail, String> {
    Ok(Mail::Feedback {
        project: request.project,
        path: optional_field("path", request.path, config::MAX_PATH)?,
        email: optional_field("email", request.email, config::MAX_EMAIL)?,
        content: free_text("content", &request.content, config::MAX_CONTENT)?,
    })
}

fn build_alert(request: AlertRequest) -> Result<Mail, String> {
    Ok(Mail::Alert {
        source: header_field("source", &request.source, config::MAX_SOURCE)?,
        severity: header_field("severity", &request.severity, config::MAX_SEVERITY)?,
        title: header_field("title", &request.title, config::MAX_TITLE)?,
        body: free_text("body", &request.body, config::MAX_ALERT_BODY)?,
    })
}

/// A field that ends up in a mail header (or in the subject line, which is a
/// header). Capped, trimmed, and free of control characters — no CR or LF can
/// ever reach a header, so no header can be injected through one.
fn header_field(name: &str, raw: &str, max: usize) -> Result<String, String> {
    let value = raw.trim();
    if value.is_empty() {
        return Err(format!("{name} is empty"));
    }
    if value.chars().count() > max {
        return Err(format!("{name} is longer than {max} characters"));
    }
    if value.chars().any(char::is_control) {
        return Err(format!("{name} contains a control character"));
    }
    Ok(value.to_string())
}

fn optional_field(name: &str, raw: Option<String>, max: usize) -> Result<Option<String>, String> {
    match raw {
        None => Ok(None),
        Some(raw) if raw.trim().is_empty() => Ok(None),
        Some(raw) => header_field(name, &raw, max).map(Some),
    }
}

/// A field that becomes the mail body: newlines are the point there, so only
/// the other control characters are refused.
fn free_text(name: &str, raw: &str, max: usize) -> Result<String, String> {
    let value = raw.trim();
    if value.is_empty() {
        return Err(format!("{name} is empty"));
    }
    if value.chars().count() > max {
        return Err(format!("{name} is longer than {max} characters"));
    }
    if value
        .chars()
        .any(|c| c.is_control() && c != '\n' && c != '\t')
    {
        return Err(format!("{name} contains a control character"));
    }
    Ok(value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request as HttpRequest;
    use tower::ServiceExt;

    /// A router wired to a queue nobody drains, plus the receiving end so a
    /// test can inspect what was queued.
    fn app(capacity: usize) -> (Router, mpsc::Receiver<Mail>) {
        app_with_token(Some("secret"), capacity)
    }

    fn app_with_token(token: Option<&str>, capacity: usize) -> (Router, mpsc::Receiver<Mail>) {
        let (tx, rx) = mpsc::channel(capacity);
        (
            router(AppState::new(tx, token.map(str::to_string), false)),
            rx,
        )
    }

    fn post(uri: &str, body: String) -> HttpRequest<Body> {
        HttpRequest::builder()
            .method(Method::POST)
            .uri(uri)
            .header(CONTENT_TYPE, "application/json")
            .body(Body::from(body))
            .unwrap()
    }

    fn feedback_body(project: &str, path: &str, content: &str) -> String {
        format!(r#"{{"project":"{project}","path":{path},"email":"","content":"{content}"}}"#)
    }

    const ALERT: &str =
        r#"{"source":"acta","severity":"warning","title":"co2 high","body":"line one\nline two"}"#;

    async fn status(app: &Router, request: HttpRequest<Body>) -> StatusCode {
        app.clone().oneshot(request).await.unwrap().status()
    }

    #[tokio::test]
    async fn null_path_is_accepted_and_queued() {
        let (app, mut rx) = app(4);
        // Exactly what marending.dev sends.
        let body =
            r#"{"project":"marending.dev","email":"a@example.com","content":"hi","path":null}"#;
        assert_eq!(
            status(&app, post("/feedback", body.into())).await,
            StatusCode::OK
        );

        match rx.try_recv().expect("queued") {
            Mail::Feedback {
                project,
                path,
                email,
                ..
            } => {
                assert_eq!(project.as_str(), "marending.dev");
                assert!(path.is_none());
                assert_eq!(email.as_deref(), Some("a@example.com"));
            }
            _ => panic!("expected feedback"),
        }
    }

    #[tokio::test]
    async fn empty_path_is_no_path() {
        let (app, mut rx) = app(4);
        let body = feedback_body("rest.quest", "\"\"", "hi");
        assert_eq!(status(&app, post("/feedback", body)).await, StatusCode::OK);
        match rx.try_recv().expect("queued") {
            Mail::Feedback { path, email, .. } => {
                assert!(path.is_none());
                assert!(email.is_none());
            }
            _ => panic!("expected feedback"),
        }
    }

    #[tokio::test]
    async fn unknown_project_is_unprocessable() {
        let (app, mut rx) = app(4);
        let body = feedback_body("ledger.rest.quest", "\"\"", "hi");
        assert_eq!(
            status(&app, post("/feedback", body)).await,
            StatusCode::UNPROCESSABLE_ENTITY
        );
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn crlf_in_a_header_bound_field_is_rejected() {
        let (app, mut rx) = app(4);
        let body =
            r#"{"project":"journal","path":"/x\r\nBcc: someone@example.com","content":"hi"}"#;
        assert_eq!(
            status(&app, post("/feedback", body.into())).await,
            StatusCode::UNPROCESSABLE_ENTITY
        );
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn newlines_are_fine_in_the_body_of_the_mail() {
        let (app, mut rx) = app(4);
        let body = feedback_body("journal", "\"\"", "line one\\nline two");
        assert_eq!(status(&app, post("/feedback", body)).await, StatusCode::OK);
        match rx.try_recv().expect("queued") {
            Mail::Feedback { content, .. } => assert!(content.contains('\n')),
            _ => panic!("expected feedback"),
        }
    }

    #[tokio::test]
    async fn oversized_content_is_unprocessable() {
        let (app, _rx) = app(4);
        let body = feedback_body("journal", "\"\"", &"x".repeat(config::MAX_CONTENT + 1));
        assert_eq!(
            status(&app, post("/feedback", body)).await,
            StatusCode::UNPROCESSABLE_ENTITY
        );
    }

    #[tokio::test]
    async fn oversized_request_body_is_413() {
        let (app, _rx) = app(4);
        let body = feedback_body(
            "journal",
            "\"\"",
            &"x".repeat(config::FEEDBACK_BODY_LIMIT + 1),
        );
        assert_eq!(
            status(&app, post("/feedback", body)).await,
            StatusCode::PAYLOAD_TOO_LARGE
        );
    }

    #[tokio::test]
    async fn wrong_content_type_is_415() {
        let (app, _rx) = app(4);
        let request = HttpRequest::builder()
            .method(Method::POST)
            .uri("/feedback")
            .header(CONTENT_TYPE, "text/plain")
            .body(Body::from(feedback_body("journal", "\"\"", "hi")))
            .unwrap();
        assert_eq!(
            status(&app, request).await,
            StatusCode::UNSUPPORTED_MEDIA_TYPE
        );
    }

    #[tokio::test]
    async fn feedback_is_limited_to_three_a_minute() {
        let (app, _rx) = app(8);
        for _ in 0..config::LIMIT_PER_WINDOW {
            assert_eq!(
                status(
                    &app,
                    post("/feedback", feedback_body("journal", "\"\"", "hi"))
                )
                .await,
                StatusCode::OK
            );
        }

        let response = app
            .clone()
            .oneshot(post("/feedback", feedback_body("journal", "\"\"", "hi")))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(response.headers().get(RETRY_AFTER).is_some());
    }

    #[tokio::test]
    async fn validation_failures_do_not_spend_quota() {
        let (app, _rx) = app(8);
        for _ in 0..10 {
            assert_eq!(
                status(&app, post("/feedback", "{}".into())).await,
                StatusCode::UNPROCESSABLE_ENTITY
            );
        }
        assert_eq!(
            status(
                &app,
                post("/feedback", feedback_body("journal", "\"\"", "hi"))
            )
            .await,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn full_queue_is_503() {
        // Capacity 1, and nothing ever reads it.
        let (app, _rx) = app(1);
        assert_eq!(
            status(
                &app,
                post("/feedback", feedback_body("journal", "\"\"", "hi"))
            )
            .await,
            StatusCode::OK
        );
        assert_eq!(
            status(
                &app,
                post("/feedback", feedback_body("journal", "\"\"", "hi"))
            )
            .await,
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

    #[tokio::test]
    async fn alert_needs_the_token() {
        let (app, mut rx) = app_with_token(Some("secret"), 4);
        let (app_no_token, _rx) = app_with_token(None, 4);

        // Missing, then wrong, then right.
        assert_eq!(
            status(&app, post("/alert", ALERT.into())).await,
            StatusCode::UNAUTHORIZED
        );

        let wrong = HttpRequest::builder()
            .method(Method::POST)
            .uri("/alert")
            .header(CONTENT_TYPE, "application/json")
            .header(AUTHORIZATION, "Bearer secreT")
            .body(Body::from(ALERT))
            .unwrap();
        assert_eq!(status(&app, wrong).await, StatusCode::UNAUTHORIZED);

        let right = HttpRequest::builder()
            .method(Method::POST)
            .uri("/alert")
            .header(CONTENT_TYPE, "application/json")
            .header(AUTHORIZATION, "Bearer secret")
            .body(Body::from(ALERT))
            .unwrap();
        assert_eq!(status(&app, right).await, StatusCode::OK);
        match rx.try_recv().expect("queued") {
            Mail::Alert {
                source,
                severity,
                title,
                ..
            } => {
                assert_eq!(source, "acta");
                assert_eq!(severity, "warning");
                assert_eq!(title, "co2 high");
            }
            _ => panic!("expected alert"),
        }

        // A prefix of the token is not the token.
        let prefix = HttpRequest::builder()
            .method(Method::POST)
            .uri("/alert")
            .header(CONTENT_TYPE, "application/json")
            .header(AUTHORIZATION, "Bearer secre")
            .body(Body::from(ALERT))
            .unwrap();
        assert_eq!(status(&app, prefix).await, StatusCode::UNAUTHORIZED);

        // Unconfigured token: 503, never "no token required".
        let anonymous = HttpRequest::builder()
            .method(Method::POST)
            .uri("/alert")
            .header(CONTENT_TYPE, "application/json")
            .body(Body::from(ALERT))
            .unwrap();
        assert_eq!(
            status(&app_no_token, anonymous).await,
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

    #[tokio::test]
    async fn alert_token_does_not_open_feedback() {
        let (app, _rx) = app_with_token(Some("secret"), 4);
        let request = HttpRequest::builder()
            .method(Method::POST)
            .uri("/feedback")
            .header(CONTENT_TYPE, "application/json")
            .header(AUTHORIZATION, "Bearer secret")
            .body(Body::from(feedback_body("journal", "\"\"", "hi")))
            .unwrap();
        // It is queued, but only because /feedback needs no token at all: the
        // token is not a key to this route, so a request carrying it is treated
        // like any other. The assertion that matters is that omitting it works
        // identically.
        assert_eq!(status(&app, request).await, StatusCode::OK);
    }

    #[tokio::test]
    async fn alert_is_not_rate_limited() {
        let (app, _rx) = app_with_token(Some("secret"), 64);
        for _ in 0..40 {
            let request = HttpRequest::builder()
                .method(Method::POST)
                .uri("/alert")
                .header(CONTENT_TYPE, "application/json")
                .header(AUTHORIZATION, "Bearer secret")
                .body(Body::from(ALERT))
                .unwrap();
            assert_eq!(status(&app, request).await, StatusCode::OK);
        }
    }

    #[tokio::test]
    async fn alert_strips_nothing_from_the_body_but_refuses_crlf_in_the_title() {
        let (app, mut rx) = app_with_token(Some("secret"), 4);
        let request = HttpRequest::builder()
            .method(Method::POST)
            .uri("/alert")
            .header(CONTENT_TYPE, "application/json")
            .header(AUTHORIZATION, "Bearer secret")
            .body(Body::from(ALERT))
            .unwrap();
        assert_eq!(status(&app, request).await, StatusCode::OK);
        match rx.try_recv().expect("queued") {
            Mail::Alert { body, .. } => assert!(body.contains("line two")),
            _ => panic!("expected alert"),
        }

        // Escaped CRLF: valid JSON, and our own validation has to refuse it.
        let escaped = ALERT.replace("co2 high", "co2 high\\r\\nBcc: x@example.com");
        let request = HttpRequest::builder()
            .method(Method::POST)
            .uri("/alert")
            .header(CONTENT_TYPE, "application/json")
            .header(AUTHORIZATION, "Bearer secret")
            .body(Body::from(escaped))
            .unwrap();
        assert_eq!(
            status(&app, request).await,
            StatusCode::UNPROCESSABLE_ENTITY
        );

        // Raw CRLF inside a JSON string is not JSON at all, so it never even
        // reaches the validator.
        let raw = ALERT.replace("co2 high", "co2 high\r\nBcc: x@example.com");
        let request = HttpRequest::builder()
            .method(Method::POST)
            .uri("/alert")
            .header(CONTENT_TYPE, "application/json")
            .header(AUTHORIZATION, "Bearer secret")
            .body(Body::from(raw))
            .unwrap();
        assert_eq!(status(&app, request).await, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn health_reports_the_transport() {
        let (app, _rx) = app(4);
        let response = app
            .clone()
            .oneshot(
                HttpRequest::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn unknown_routes_are_404() {
        let (app, _rx) = app(4);
        // No static root, no index: nothing but the three paths above, so the
        // vhost cannot serve site content even if Caddy asks it to. A GET on
        // /feedback is a 405, which is also fine.
        for uri in ["/", "/index.html", "/feedback/", "/admin", "/../etc/passwd"] {
            let response = app
                .clone()
                .oneshot(HttpRequest::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{uri}");
        }
    }

    #[tokio::test]
    async fn cors_allows_the_known_origins_only() {
        let (app, _rx) = app(4);

        let preflight = |origin: &'static str| {
            HttpRequest::builder()
                .method(Method::OPTIONS)
                .uri("/feedback")
                .header("origin", origin)
                .header("access-control-request-method", "POST")
                .header("access-control-request-headers", "content-type")
                .body(Body::empty())
                .unwrap()
        };

        for origin in config::CORS_ORIGINS {
            let response = app.clone().oneshot(preflight(origin)).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{origin}");
            assert_eq!(
                response
                    .headers()
                    .get("access-control-allow-origin")
                    .and_then(|v| v.to_str().ok()),
                Some(origin)
            );
        }

        // dd.rest.quest is dead: it gets no allow-origin header at all.
        let response = app
            .clone()
            .oneshot(preflight("https://dd.rest.quest"))
            .await
            .unwrap();
        assert!(
            response
                .headers()
                .get("access-control-allow-origin")
                .is_none()
        );
    }

    #[tokio::test]
    async fn validation_refuses_control_characters_and_long_fields() {
        assert!(header_field("source", "acta", 10).is_ok());
        assert!(header_field("source", "", 10).is_err());
        assert!(header_field("source", "acta\rmarending", 64).is_err());
        assert!(
            header_field("source", "acta\n", 64).is_ok(),
            "trimmed, not injected"
        );
        assert!(header_field("source", "abcdef", 5).is_err());
        assert!(free_text("body", "one\ntwo", 100).is_ok());
        assert!(free_text("body", "one\0two", 100).is_err());
    }
}
