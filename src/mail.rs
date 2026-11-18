//! The mail itself: what we send, how it is rendered, and the two transports.
//!
//! Everything that knows about SMTP lives here, behind [`Mailer`], so the HTTP
//! layer never touches a socket and the tests never touch a network.

use std::fmt;

use async_trait::async_trait;
use lettre::message::{Mailbox, Message};
use lettre::transport::smtp::Error as SmtpError;
use lettre::transport::smtp::authentication::Credentials;
use lettre::{Address, AsyncSmtpTransport, AsyncTransport, Tokio1Executor};

use crate::api::Project;
use crate::config;

/// One queued message. Built by the handlers, sent by the sender task.
pub enum Mail {
    Feedback {
        project: Project,
        path: Option<String>,
        email: Option<String>,
        content: String,
    },
    Alert {
        source: String,
        severity: String,
        title: String,
        body: String,
    },
}

impl Mail {
    /// `/feedback` and `/alert` differ only in the header line the subject gets.
    pub fn subject(&self) -> String {
        match self {
            Mail::Feedback { project, path, .. } => match path {
                Some(path) => format!("Feedback: {} - {}", project.as_str(), path),
                None => format!("Feedback: {}", project.as_str()),
            },
            Mail::Alert {
                severity, title, ..
            } => format!("Alert ({severity}): {title}"),
        }
    }

    /// Who the mail is from and who receives it. Alerts carry their own sender so
    /// they can be sorted on `From` alone, without parsing the subject.
    pub fn from(&self) -> &'static str {
        match self {
            Mail::Feedback { .. } => config::FEEDBACK_FROM,
            Mail::Alert { .. } => config::ALERT_FROM,
        }
    }

    pub fn to(&self) -> &'static str {
        match self {
            Mail::Feedback { .. } => config::FEEDBACK_TO,
            Mail::Alert { .. } => config::ALERT_TO,
        }
    }

    /// Only feedback carries an author, and only when the address parses.
    /// Reply-To has no deliverability impact: SPF, DKIM and DMARC all key off
    /// `From`, which is always ours.
    pub fn reply_to(&self) -> Option<Address> {
        match self {
            Mail::Feedback { email, .. } => email.as_deref()?.parse().ok(),
            Mail::Alert { .. } => None,
        }
    }

    pub fn text(&self) -> String {
        match self {
            Mail::Feedback {
                project,
                path,
                email,
                content,
            } => format!(
                "project: {}\npath: {}\nemail: {}\n\n{}",
                project.as_str(),
                path.as_deref().unwrap_or("(none)"),
                email.as_deref().unwrap_or("(none)"),
                content,
            ),
            Mail::Alert {
                source,
                severity,
                body,
                ..
            } => format!("source: {source}\nseverity: {severity}\n\n{body}"),
        }
    }

    /// The payload, escaped onto one line, for the log. The client only ever
    /// sees a `200`, so this line is the only record that a message existed:
    /// if SMTP rejects it for good, this is what it gets recovered from.
    pub fn log_line(&self) -> String {
        match self {
            Mail::Feedback {
                project,
                path,
                email,
                content,
            } => format!(
                "feedback project={} path={:?} email={:?} content={:?}",
                project.as_str(),
                path,
                email,
                content
            ),
            Mail::Alert {
                source,
                severity,
                title,
                body,
            } => format!(
                "alert source={:?} severity={:?} title={:?} body={:?}",
                source, severity, title, body
            ),
        }
    }
}

impl fmt::Display for Mail {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.log_line())
    }
}

/// Why a send did not work, split by whether retrying could help.
#[derive(Debug)]
pub enum Failure {
    /// Connect, IO, timeout, SMTP 4xx: worth another attempt.
    Transient(String),
    /// SMTP 5xx, or a message we could not even build: another attempt would
    /// fail identically.
    Permanent(String),
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Failure::Transient(why) => write!(f, "transient: {why}"),
            Failure::Permanent(why) => write!(f, "permanent: {why}"),
        }
    }
}

#[async_trait]
pub trait Mailer: Send + Sync + 'static {
    async fn send(&self, mail: &Mail) -> Result<(), Failure>;
}

/// Real delivery over Fastmail submission.
pub struct Smtp {
    transport: AsyncSmtpTransport<Tokio1Executor>,
}

impl Smtp {
    /// `user` is the Fastmail account's *login* address, not the alias in
    /// the sender constant: aliases receive and can send as themselves, but a 535 comes
    /// back if you try to authenticate as one.
    pub fn new(user: &str, password: &str) -> Result<Self, String> {
        let transport = AsyncSmtpTransport::<Tokio1Executor>::relay(config::SMTP_HOST)
            .map_err(|error| format!("smtp: {error}"))?
            .credentials(Credentials::new(user.to_string(), password.to_string()))
            .port(config::SMTP_PORT)
            // No `pool` feature: one connection per send, by construction.
            .build();

        Ok(Self { transport })
    }
}

#[async_trait]
impl Mailer for Smtp {
    async fn send(&self, mail: &Mail) -> Result<(), Failure> {
        let message = build(mail).map_err(|error| Failure::Permanent(error.to_string()))?;

        match self.transport.send(message).await {
            Ok(_response) => Ok(()),
            // `is_permanent` is true for a 5xx reply and for the client-side
            // errors that cannot succeed on a retry. Everything else — connect,
            // IO, timeout, 4xx — is transient.
            Err(error) if error.is_permanent() => {
                let mut why = describe(&error);
                // 535 is the one permanent error whose cause is a constant in
                // this file rather than anything about the message: Fastmail
                // authenticates the account's login address, and aliases such as
                // this service's own feedback@ address are only sending
                // identities. Without this hint the log reads like a wrong
                // password.
                if error.status().is_some_and(is_auth_failure) {
                    why.push_str(
                        " (535 is an authentication failure: SMTP_USER must be the Fastmail \
                         account's login address, not an alias; the password must be an app \
                         password with SMTP scope and no surrounding whitespace). Both arrive \
                         as podman secrets: notify-smtp-user and notify-smtp-password",
                    );
                }
                Err(Failure::Permanent(why))
            }
            Err(error) => Err(Failure::Transient(describe(&error))),
        }
    }
}

fn build(mail: &Mail) -> Result<Message, lettre::error::Error> {
    let from: Address = mail
        .from()
        .parse()
        .expect("the envelope constants must parse");
    let to: Address = mail
        .to()
        .parse()
        .expect("the envelope constants must parse");

    let mut builder = Message::builder()
        .from(Mailbox::new(None, from))
        .to(Mailbox::new(None, to))
        .subject(mail.subject());

    if let Some(reply_to) = mail.reply_to() {
        builder = builder.reply_to(Mailbox::new(None, reply_to));
    }

    builder.body(mail.text())
}

fn describe(error: &SmtpError) -> String {
    match error.status() {
        Some(code) => format!("smtp {code}: {error}"),
        None => error.to_string(),
    }
}

/// `Code` exposes its three digits as fields but has no numeric accessor, and
/// its `Display` is exactly the three digits: `535` is authentication failed.
fn is_auth_failure(code: lettre::transport::smtp::response::Code) -> bool {
    code.to_string() == "535"
}

/// Stand-in used when `SMTP_PASSWORD` is absent: nothing is sent, and the
/// sender task logs the payload instead. Always succeeds, so a dev instance
/// answers `200` exactly like production.
pub struct LogOnly;

#[async_trait]
impl Mailer for LogOnly {
    async fn send(&self, _mail: &Mail) -> Result<(), Failure> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feedback(path: Option<&str>, email: Option<&str>) -> Mail {
        Mail::Feedback {
            project: Project::Journal,
            path: path.map(str::to_string),
            email: email.map(str::to_string),
            content: "two\nlines".to_string(),
        }
    }

    #[test]
    fn subject_omits_an_absent_path() {
        assert_eq!(feedback(None, None).subject(), "Feedback: journal");
        assert_eq!(
            feedback(Some("/x"), None).subject(),
            "Feedback: journal - /x"
        );
    }

    #[test]
    fn reply_to_only_for_a_parseable_author() {
        assert!(feedback(None, Some("a@example.com")).reply_to().is_some());
        assert!(feedback(None, Some("not an address")).reply_to().is_none());
        assert!(feedback(None, None).reply_to().is_none());
    }

    #[test]
    fn message_builds_and_carries_reply_to() {
        let message = build(&feedback(Some("/here"), Some("a@example.com"))).unwrap();
        let rendered = message.formatted();
        let rendered = String::from_utf8_lossy(&rendered);
        assert!(rendered.contains("Reply-To: a@example.com"), "{rendered}");
        assert!(
            rendered.contains("Subject: Feedback: journal - /here"),
            "{rendered}"
        );
        assert!(
            rendered.contains("To: feedback@marending.dev"),
            "{rendered}"
        );
    }

    #[test]
    fn an_alert_is_from_the_alerts_alias_and_feedback_is_not() {
        let alert = Mail::Alert {
            source: "acta-eos".to_string(),
            severity: "warn".to_string(),
            title: "eos board temperature above 35 °C".to_string(),
            body: "42 °C".to_string(),
        };
        let rendered = build(&alert).unwrap().formatted();
        let rendered = String::from_utf8_lossy(&rendered);
        // The alias, so a mail client can sort alerts on `From` alone; the
        // envelope address is unchanged from feedback's.
        assert!(rendered.contains("From: alerts@marending.dev"), "{rendered}");
        assert!(
            rendered.contains("To: feedback@marending.dev"),
            "{rendered}"
        );
        assert!(rendered.contains("Subject: Alert (warn):"), "{rendered}");
        // No author on an alert, so no Reply-To to answer.
        assert!(!rendered.contains("Reply-To:"), "{rendered}");

        let rendered = build(&feedback(None, None)).unwrap().formatted();
        let rendered = String::from_utf8_lossy(&rendered);
        assert!(
            rendered.contains("From: feedback@marending.dev"),
            "{rendered}"
        );
    }

    #[test]
    fn only_535_counts_as_an_authentication_failure() {
        use lettre::transport::smtp::response::{Category, Code, Detail, Severity};

        assert!(is_auth_failure(Code::new(
            Severity::PermanentNegativeCompletion,
            Category::Unspecified3,
            Detail::Five,
        )));
        // 550 is a permanent failure too, but not an authentication one.
        assert!(!is_auth_failure(Code::new(
            Severity::PermanentNegativeCompletion,
            Category::MailSystem,
            Detail::Zero,
        )));
    }

    #[test]
    fn log_line_keeps_the_payload_on_one_line() {
        assert!(!feedback(None, None).log_line().contains('\n'));
    }
}
