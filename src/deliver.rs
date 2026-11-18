//! The sender task: one mail at a time, retried, with a deadline around each
//! attempt.
//!
//! Serial on purpose. Ten mails a year do not need concurrency, and a single
//! task makes "the queue is full" mean exactly one thing.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::mpsc::Receiver;
use tokio::time::{sleep, timeout};

use crate::config;
use crate::mail::{Failure, Mail, Mailer};

/// Consumes the queue until every sender is gone, then stops. The server drops
/// the router — and with it the last `Sender` — before the drain deadline is
/// applied to this task, so what is left here is exactly the backlog.
pub async fn run(mut queue: Receiver<Mail>, mailer: Arc<dyn Mailer>, dry_run: bool) {
    while let Some(mail) = queue.recv().await {
        deliver(&*mailer, &mail, &config::BACKOFF, dry_run).await;
    }
    tracing::info!("queue closed and drained; sender stopping");
}

async fn deliver(mailer: &dyn Mailer, mail: &Mail, backoff: &[Duration], dry_run: bool) {
    let started = Instant::now();
    let attempts = backoff.len() + 1;

    for attempt in 1..=attempts {
        match timeout(config::SEND_TIMEOUT, mailer.send(mail)).await {
            Ok(Ok(())) => {
                tracing::info!(
                    attempt,
                    ms = started.elapsed().as_millis() as u64,
                    outcome = if dry_run { "dry-run" } else { "sent" },
                    mail = %mail,
                    "mail delivered"
                );
                return;
            }
            // A 5xx, or a message we could not build: retrying changes nothing.
            Ok(Err(Failure::Permanent(why))) => {
                tracing::error!(
                    attempt,
                    why = %why,
                    mail = %mail,
                    "mail rejected permanently, not retrying"
                );
                return;
            }
            Ok(Err(Failure::Transient(why))) => {
                tracing::warn!(attempt, why = %why, "mail send failed");
            }
            Err(_) => {
                tracing::warn!(
                    attempt,
                    timeout_s = config::SEND_TIMEOUT.as_secs(),
                    "mail send timed out (a duplicate is possible if the server got the message)"
                );
            }
        }

        // No pause after the last attempt: the index simply runs out.
        if let Some(pause) = backoff.get(attempt - 1) {
            sleep(*pause).await;
        }
    }

    tracing::error!(
        attempts,
        mail = %mail,
        "mail dropped after every attempt failed"
    );
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use async_trait::async_trait;

    use super::*;

    /// Plays back a script of outcomes, then repeats the last one, and counts
    /// the calls.
    struct Mock {
        outcomes: Mutex<VecDeque<Outcome>>,
        repeat: Outcome,
        calls: Mutex<usize>,
    }

    #[derive(Clone, Copy)]
    enum Outcome {
        Ok,
        Transient,
        Permanent,
    }

    impl Mock {
        fn new(outcomes: &[Outcome]) -> Arc<Self> {
            Arc::new(Self {
                outcomes: Mutex::new(outcomes.iter().copied().collect()),
                repeat: *outcomes.last().unwrap_or(&Outcome::Ok),
                calls: Mutex::new(0),
            })
        }

        fn calls(&self) -> usize {
            *self.calls.lock().unwrap()
        }
    }

    #[async_trait]
    impl Mailer for Mock {
        async fn send(&self, _mail: &Mail) -> Result<(), Failure> {
            *self.calls.lock().unwrap() += 1;
            match self
                .outcomes
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(self.repeat)
            {
                Outcome::Ok => Ok(()),
                Outcome::Transient => Err(Failure::Transient("4xx".into())),
                Outcome::Permanent => Err(Failure::Permanent("5xx".into())),
            }
        }
    }

    fn mail() -> Mail {
        Mail::Alert {
            source: "acta".into(),
            severity: "info".into(),
            title: "test".into(),
            body: "body".into(),
        }
    }

    // No waiting: the pause schedule is a parameter, so the retry logic can be
    // tested without spending 12 seconds per case.
    const FAST: [Duration; 2] = [Duration::ZERO, Duration::ZERO];

    #[tokio::test]
    async fn a_send_that_works_is_attempted_once() {
        let mock = Mock::new(&[Outcome::Ok]);
        deliver(&*mock, &mail(), &FAST, false).await;
        assert_eq!(mock.calls(), 1);
    }

    #[tokio::test]
    async fn a_transient_failure_is_retried_until_it_works() {
        let mock = Mock::new(&[Outcome::Transient, Outcome::Ok]);
        deliver(&*mock, &mail(), &FAST, false).await;
        assert_eq!(mock.calls(), 2);
    }

    #[tokio::test]
    async fn a_permanent_failure_is_never_retried() {
        let mock = Mock::new(&[Outcome::Permanent, Outcome::Ok]);
        deliver(&*mock, &mail(), &FAST, false).await;
        assert_eq!(mock.calls(), 1);
    }

    #[tokio::test]
    async fn every_attempt_failing_gives_up_after_three() {
        let mock = Mock::new(&[Outcome::Transient]);
        deliver(&*mock, &mail(), &FAST, false).await;
        assert_eq!(mock.calls(), config::ATTEMPTS);
    }
}
