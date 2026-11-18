# notify: notification service for feedback + Acta alerts

A small standalone HTTP service on eos that accepts a payload and sends it as email via Fastmail SMTP
submission. Two producers: feedback forms in the web apps, and threshold alerts from Acta. It is its
own container on purpose — having Acta POST alerts to the marending.dev static-site container would be
backwards coupling, and folding this into the site service was rejected for exactly that reason.

## Replaces

`../feedback-relay` (axum 0.6, Resend HTTP API, docker compose on the arm host, port 8008). The
transport changes from a third-party API key to Fastmail SMTP with an app password.

## Clients that must keep working untouched

All POST `https://feedback.marending.dev/feedback` with JSON `{project, path, email, content}`, and
treat HTTP `200` as success and `429` as "slow down":

- marending.dev     `project: "marending.dev"`, may send `path: null`
- rest.quest        `project: "rest.quest"`, `path: ""`
- go.rest.quest     `project: "go.rest.quest"`, `path: ""`
- jour.rest.quest   `project: "journal"`, `path: ""`

`dd.rest.quest` and `ledger.rest.quest` are dead; drop them from the CORS allowlist.

## Bugs in the old service not to carry over

- `path: String` in the Deserialize while marending sends `path: null` → the old relay answers 422.
  Use `Option<String>` for `email` and `path`.
- `tower::RateLimitLayer::new(4, 60s)` over the whole router: global, and applied to `/health` too.
- `AllowMethods::any()` + `AllowHeaders::any()`.
- `project` / `path` interpolated into the Subject line unvalidated.
- Sender was `info@feedback.marending.dev`; no subdomain and no extra DNS records are wanted.

## API: two routes, two policies

Public, CORS, rate-limited — `POST /feedback`
`{project, path?, email?, content}`. `project` is a closed enum. `content` ≤ 2000 chars. Requires
`application/json`; `DefaultBodyLimit` 16 KiB. Statuses: `200` queued, `400`/`422` bad payload,
`413` oversized, `429` + `Retry-After`.

Authenticated, not rate-limited — `POST /alert`
`Authorization: Bearer <ALERT_TOKEN>`, body `{source, severity, title, body}`. Statuses: `200` queued,
`401` wrong or missing token, `503` when the token is not configured. Compare with
`subtle::ConstantTimeEq`. The token is written by Acta and verified here; it is installed as a podman
secret on both sides. The token does **not** unlock `/feedback` — one token, one route, no cross-use.
No rate limit on this route: instead a storm guard that only *logs loudly* above ~10 requests/minute,
because a flood here means a bug in our own client and dropping those mails is the wrong answer.

Validation for both: cap all strings, reject CR/LF, never interpolate unvalidated input into a header.

## Transport (settled)

- `lettre` v0.11, async: features `tokio1`, `tokio1-rustls-tls`, `builder`, `hostname`.
  rustls keeps `ca-certificates`/OpenSSL out of the runtime image; the crypto provider resolves to
  `ring`, so the builder needs a C compiler (present in `rust:bookworm`) but no cmake (unlike
  rustls' default `aws-lc-rs` provider).
- Port **465**, implicit TLS: `AsyncSmtpTransport::relay("smtp.fastmail.com")`.
  `Credentials::new("feedback@marending.dev", app_password)` — Fastmail wants the full address.
- One connection per send; no pooling.
- Whole send wrapped in `tokio::time::timeout(10s)`: lettre has a connect timeout but no global
  deadline, and the worker is a single serial task, so one stalled send would block the queue.
- Addresses: `From: feedback@marending.dev`, `To: feedback@marending.dev` (an alias into the `info@`
  inbox — no second mailbox, no new DNS), `Reply-To: <author>` for feedback when the address parses,
  with the author address repeated in the body. Reply-To has no deliverability impact: SPF, DKIM and
  DMARC alignment all use `From`.
- Subjects: feedback `Feedback: <project> - <path>`, alert `Alert (<severity>): <title>`.

## Delivery (settled)

- Bounded `mpsc` (32) into one sender task, spawned before the server starts.
- Handler returns `200` on enqueue; `503` when the queue is full or sending is disabled.
- Retry 3 attempts with 2 s / 10 s backoff, only for transient failures (connect, IO, timeout,
  SMTP 4xx). Never retry a 5xx rejection. A duplicate after a post-`DATA` timeout is accepted; a lost
  message is not.
- Drain the queue on SIGTERM with a deadline, shaped like the exporter flush in
  `../marending.dev/service/src/main.rs`. Stop accepting new requests first.

## Rate limiting: global, in-memory, no per-IP

One `Mutex` holding a 60 s window and a daily counter, applied to `/feedback` only: **3 per 60 s** and
**20 per day** across all clients, `429` with `Retry-After`. Client IPs are useless behind Caddy and
volume is ~10/year, so per-IP buys nothing. In-memory state with one replica is fine. The daily cap is
the domain-reputation guard; the per-minute cap is politeness.

## Configuration: one plain knob, two secrets

`SERVE_PORT` — default `8080`, only so a dev instance can be started on any port. Everything else is a
constant in the source: SMTP host and port, all addresses, CORS origins, both limits, queue capacity,
attempt count and backoff, body and content caps, send timeout.

- `SMTP_PASSWORD` — podman secret. When missing: log the payload instead of sending, print a dry-run
  banner at startup so it is visible in `journalctl`. `mise run deploy` refuses to deploy without it,
  so production cannot reach that state.
- `ALERT_TOKEN` — podman secret. When missing: `/alert` answers `503`. Fail closed — an unset secret
  must never mean "no token required".

## Secrets and deployment

Follow `../acta` exactly: committed sops ciphertext, age private key in 1Password, decrypted only by
`mise run deploy:secrets`, installed as podman secrets on eos, `Secret=...,type=env,target=...` in the
quadlet. Files to create: `.sops.yaml`, `.deploy.secrets` (gitignored), `.deploy.secrets.enc`,
`mise-tasks/lib/env.sh`, and the `deploy`, `deploy:build`, `deploy:secrets`, `deploy:edit-secrets`
tasks. Add `.deploy.secrets*` to `.containerignore` — marending's ignores `.deploy.env` but not this.

Mint a **new** age key (1Password item, e.g. "SOPS age key - mail"): this repo holds the alert token,
which Acta also knows, so it must not share a key with Acta or marending. Fastmail app password scoped
to SMTP only. (Reference: marending's retired recipient was
`age1tlteuzp4uwd7ld28s2ncstgmh92jja84y4a4frrl0hhwc04vpyzsqpqkyl`.)

## Hosting on eos

- Quadlet `notify.container`: `PublishPort=127.0.0.1:3005:8080` (3000–3004 are taken), `SERVE_PORT=8080`,
  `User=1000:1000`, `UserNS=keep-id`, `NoNewPrivileges`, `DropCapability=all`, `Restart=on-failure`.
- Caddy, `../eos-core/caddy/Caddyfile`: `feedback.marending.dev { reverse_proxy localhost:3005 }`,
  deployed with `../eos-core/caddy/deploy.sh`. No host routing needed — the app serves only its two
  routes and 404s everything else, so the vhost root cannot serve site content.
- DNS: `feedback` is not in `../eos-core/dns/dns.sh` `RECORDS` (only the three rest.quest subdomains
  are), so flipping its A record to eos is manual. Order: secrets → deploy → Caddy → DNS → verify from
  outside → decommission the old relay on the arm host and revoke the Resend key. Keep the old relay
  deployable for a week in case of a flip back.

## Testing

Mailer behind a small trait injected through router state, so `cargo test` covers
`200/400/401/413/422/429/503` offline (`tower::ServiceExt::oneshot`). Unit tests for validation
(null `path`, oversized content, unknown `project`, CR/LF), the limiter window and day rollover, and
constant-time token rejection.

## Observability

Logs are the baseline: route, `project`/`source`, outcome, attempts, SMTP code, duration — never
content. Since the client only sees a `200`, a failing SMTP path is visible only in these logs.
Wide events to Acta are a later stretch (needs `ACTA_URL`/`ACTA_SOURCE` and Acta's sensor secrets, and
a standalone `Event::new` + `Sink::send`, as in `../marending.dev/service/src/wide/`).

---

# Decisions taken while building this

Recorded here because they are departures from, or silences in, the brief above.

1. **Name.** Crate, binary and image are `notify`; the quadlet is `notify.container`; the podman
   secrets are `notify-smtp-password` and `notify-alert-token`. `feedback-relay` stays as it is until
   it is decommissioned.
2. **`/health` exists** (`GET`, unauthenticated, unlimited) as a third route, because the deploy
   script needs a probe and there is no static root to fall back on. It reports `smtp: live|dry-run`
   so a dry-run production instance is visible from outside the logs.
3. **Dry run is not an error.** With no `SMTP_PASSWORD` the sender uses a `LogOnly` mailer: the
   payload is logged and the handler still answers `200`, so a dev instance behaves exactly like
   production. There is no separate "disabled" switch — one fewer knob to set wrong.
4. **Payloads are always logged**, in one escaped line per mail, including on success. At ten mails a
   year this is the recovery record for a client that was told `200` while SMTP failed. The "never
   log content" line above is superseded by this decision.
5. **Caps** (chars): `content` 2000, `path` 512, `email` 254, `source` 64, `severity` 32, `title` 200,
   `body` 32 000. `/alert` gets a 64 KiB body limit of its own, because alert bodies carry context
   rather than a human comment.
6. **CR/LF is refused only in header-bound fields** (`project`, `path`, `email`, `source`, `severity`,
   `title`); `content` and `body` keep their newlines, which is the point of them. `char::is_control`
   covers CR, LF, tab and NUL at once.
7. **Status codes stay those axum already produces**: malformed JSON is `400`, a data error is `422`,
   a wrong content type is `415`, an oversized body is `413`. The brief's `400`/`422` split is kept
   rather than flattened.
8. **Validation happens before the rate limit**, so a rejected payload never spends anyone's quota,
   and a rejected request never counts as an alert.
9. **The daily cap resets at UTC midnight**, not local midnight: a domain-reputation guard should not
   move when the host's timezone changes. `Retry-After` is the time to the reset.
10. **`https://www.marending.dev` is in the CORS allowlist.** Caddy serves it from the same vhost with
    no redirect, so the form is reachable there and would otherwise fail its preflight.
11. **The service joins `acta-net` as `notify`.** Acta POSTs `/alert` from inside its own container, so
    a loopback-published port is not reachable; the deploy checks that acta's deploy installed the
    network unit, exactly as marending's does.
12. **No plaintext secrets file ever exists.** `deploy:edit-secrets` writes a template on first use and
    encrypts it immediately; afterwards it sops-edits the ciphertext in place.
13. **Drain**: 20 s queue deadline, `TimeoutStopSec=40` (drain plus one in-flight 10 s send).
14. **`SMTP_USER` is a secret, not a constant.** The brief has every non-`SERVE_PORT`
    setting as a constant in the source, but the Fastmail *login* address is a personal
    address and this repository is public. It arrives as the `notify-smtp-user` podman
    secret, with the app password as `notify-smtp-password`; the pair is required for
    sending, and either half missing means dry run. Fastmail also refuses to authenticate
    an alias, and `feedback@marending.dev` — the only address that could stay in the
    source, since it is already on the site — is an alias by design, so this was going
    to be a second value from the start.
15. **The Acta sender is a follow-up.** This repo documents the `POST /alert` contract and a `curl`
    example; nothing in `../acta` was touched.
