# notify

A small HTTP service that takes a payload and mails it through Fastmail SMTP
submission. Two producers, two routes, two policies: feedback forms in the web
apps, and threshold alerts from Acta.

It is its own container on purpose. Having Acta POST to the marending.dev
static-site container would be backwards coupling.

Replaces `../feedback-relay` (Resend HTTP API, port 8008). The transport changes
from a third-party API key to Fastmail SMTP with an app password; the client
contract does not change.

The design, and the reasons behind the numbers, are in `design/001-mail-service.md`.

## Routes

| Route | Auth | Limit | Body |
| --- | --- | --- | --- |
| `POST /feedback` | none, CORS-restricted to the form origins | 3/min and 20/day, global | `{project, path?, email?, content}`, 16 KiB |
| `POST /alert` | `Authorization: Bearer $ALERT_TOKEN` | none (a storm is logged) | `{source, severity, title, body}`, 64 KiB |
| `GET /health` | none | none | `{"status":"ok","smtp":"live"\|"dry-run"}` |

Everything else is a 404: there is no static root, so the vhost cannot serve
anything but these three paths.

Statuses on both POSTs: `200` queued, `400` malformed JSON, `413` body too
large, `415` wrong content type, `422` bad payload (unknown `project`, empty or
oversized field, control character in a header-bound field), `429` with
`Retry-After` (feedback only), `503` queue full or sending not configured
(`/alert` without a token is `401`, and `503` when no token is configured).

`200` means queued, not delivered. SMTP happens after the response; the logs are
the record, and they include the full payload so a hard failure is recoverable.

`project` is one of `marending.dev`, `rest.quest`, `go.rest.quest`, `journal`.

## Configuration

Three environment variables, nothing else:

- `SERVE_PORT` — default `8080`; only so a dev instance can move.
- `SMTP_PASSWORD` — Fastmail app password (SMTP scope). Absent: dry run, the
  payload is logged instead of sent, and `/health` says `dry-run`.
- `ALERT_TOKEN` — bearer token for `/alert`. Absent: `/alert` answers `503`.
  Fail closed; a missing secret never means "no token required".

Everything else — SMTP host and port, addresses, CORS origins, both limits,
queue capacity, attempts and backoff, body and field caps, send timeout — is a
constant in `src/config.rs`.

## Development

```sh
cargo test                 # 34 tests, no network, no SMTP
SERVE_PORT=8080 cargo run  # no SMTP_PASSWORD: dry run, payloads in the logs
```

```sh
curl -s localhost:8080/health
curl -s -X POST localhost:8080/feedback \
  -H 'content-type: application/json' \
  -d '{"project":"journal","email":"","content":"hello","path":""}'
```

## Deployment

Same shape as `../acta`: the image is built locally and shipped over ssh as a
tarball, the quadlet is rendered from `notify.container`, and the credentials
are podman secrets.

```sh
mise run deploy:edit-secrets   # once: fill in the two values (sops ciphertext)
mise run deploy:secrets        # install them on the host, restart
mise run deploy                # build, ship, restart, health check
mise run deploy:build          # build the image only
```

Order for the first rollout: secrets → deploy → Caddy → DNS → verify from
outside → decommission the old relay on the arm host and revoke the Resend key.

- `../eos-core/caddy/Caddyfile`: `feedback.marending.dev { reverse_proxy
  localhost:3005 }`, deployed with `../eos-core/caddy/deploy.sh`.
- `feedback` is **not** in `../eos-core/dns/dns.sh` `RECORDS`, so flipping its A
  record to eos is manual.
- The service joins `acta-net` with the alias `notify`, because Acta POSTs
  `/alert` from inside its own container. Run acta's deploy first.

## Sending alerts from Acta

```sh
curl -sf -X POST http://notify:8080/alert \
  -H "Authorization: Bearer $ALERT_TOKEN" \
  -H 'content-type: application/json' \
  -d '{"source":"acta","severity":"warning","title":"co2 above threshold","body":"last hour: 1400 ppm"}'
```

`/alert` is not rate limited, so a flood is never dropped — it is logged as an
alert storm (`per_minute` above 10) because a flood means a bug in our client.

## Secrets

`.deploy.secrets.enc` is committed ciphertext; its age private key is the
1Password item "SOPS age key - mail". This repo has its own key because it holds
the alert token, which Acta also knows.

Podman secrets on eos: `notify-smtp-password`, `notify-alert-token`.
