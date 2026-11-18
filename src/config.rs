//! Every knob this service has.
//!
//! Three values come from the environment: `SERVE_PORT` (only so a dev instance
//! can move off 8080) and the two secrets, `SMTP_PASSWORD` and `ALERT_TOKEN`.
//! Everything else is a constant here, because a relay that sends ten mails a
//! year does not need to be tunable — it needs to be readable.

use std::env;
use std::time::Duration;

/// Port the HTTP server binds. The quadlet always sets `SERVE_PORT=8080`
/// explicitly; the default only exists so `cargo run` works without one.
const DEFAULT_SERVE_PORT: u16 = 8080;

// --- SMTP -------------------------------------------------------------------
// Port 465 is implicit TLS, which is what `AsyncSmtpTransport::relay` builds.
pub const SMTP_HOST: &str = "smtp.fastmail.com";
pub const SMTP_PORT: u16 = 465;
/// Fastmail wants the full address as the SMTP user, not the local part.
pub const SMTP_USER: &str = "feedback@marending.dev";
/// Both ends of the envelope are the same address: `feedback@` is an alias into
/// the `info@` inbox, so no second mailbox and no extra DNS are involved.
pub const MAIL_FROM: &str = "feedback@marending.dev";
pub const MAIL_TO: &str = "feedback@marending.dev";

/// A single send, wall clock. lettre has a connect timeout but no overall
/// deadline, and the sender is one serial task: one stalled send would block
/// the whole queue behind it.
pub const SEND_TIMEOUT: Duration = Duration::from_secs(10);

/// Delivery attempts per mail, and the pause before each *retry*: attempt 1
/// runs immediately, attempt 2 after 2 s, attempt 3 after 2 s + 10 s. The
/// array length is `ATTEMPTS - 1` so the two cannot drift apart.
pub const ATTEMPTS: usize = 3;
pub const BACKOFF: [Duration; ATTEMPTS - 1] = [Duration::from_secs(2), Duration::from_secs(10)];

/// Bounded queue between the handlers and the sender task.
pub const QUEUE_CAPACITY: usize = 32;

/// How long SIGTERM waits for queued mail. The quadlet's `TimeoutStopSec` is
/// larger, so an unresponsive SMTP server cannot hold the unit down.
pub const DRAIN_TIMEOUT: Duration = Duration::from_secs(20);

// --- request bodies ---------------------------------------------------------
pub const FEEDBACK_BODY_LIMIT: usize = 16 * 1024;
/// Larger than feedback: an alert's `body` carries context (a stack trace, a
/// reading dump) that a human comment does not.
pub const ALERT_BODY_LIMIT: usize = 64 * 1024;

// --- field caps, in chars not bytes ----------------------------------------
pub const MAX_CONTENT: usize = 2000;
pub const MAX_PATH: usize = 512;
pub const MAX_EMAIL: usize = 254; // RFC 5321 upper bound on a forward path
pub const MAX_SOURCE: usize = 64;
pub const MAX_SEVERITY: usize = 32;
pub const MAX_TITLE: usize = 200;
pub const MAX_ALERT_BODY: usize = 32_000;

// --- rate limit (`/feedback` only) -----------------------------------------
pub const LIMIT_WINDOW: Duration = Duration::from_secs(60);
pub const LIMIT_PER_WINDOW: u32 = 3;
/// The domain-reputation guard: a burst of form submissions is politeness, a
/// sustained trickle of mail from one domain is a spam complaint.
pub const LIMIT_PER_DAY: u32 = 20;

/// `/alert` is not rate limited, but a flood means a bug in our own client, so
/// it gets logged loudly above this many requests per minute instead.
pub const ALERT_STORM_PER_MINUTE: u32 = 10;

// --- CORS -------------------------------------------------------------------
// Only the sites that host a feedback form. `www.` is a separate origin and
// Caddy serves it from the same vhost, so it has to be listed too.
// dd.rest.quest and ledger.rest.quest are dead and deliberately absent.
pub const CORS_ORIGINS: [&str; 5] = [
    "https://marending.dev",
    "https://www.marending.dev",
    "https://rest.quest",
    "https://go.rest.quest",
    "https://jour.rest.quest",
];

pub struct Settings {
    pub serve_port: u16,
    /// Absent: no SMTP, so the payload is logged instead of sent (dry run).
    pub smtp_password: Option<String>,
    /// Absent: `/alert` answers 503. Fail closed — a missing secret must never
    /// mean "no token required".
    pub alert_token: Option<String>,
}

impl Settings {
    pub fn from_env() -> Result<Self, String> {
        let serve_port = match env::var("SERVE_PORT") {
            Ok(raw) => raw
                .parse()
                .map_err(|_| format!("SERVE_PORT is not a port: {raw}"))?,
            Err(_) => DEFAULT_SERVE_PORT,
        };

        // An empty secret is a secret that was not set: treat it as absent
        // rather than as an empty password.
        Ok(Self {
            serve_port,
            smtp_password: non_empty(env::var("SMTP_PASSWORD")),
            alert_token: non_empty(env::var("ALERT_TOKEN")),
        })
    }
}

fn non_empty(value: Result<String, env::VarError>) -> Option<String> {
    value.ok().filter(|v| !v.trim().is_empty())
}
