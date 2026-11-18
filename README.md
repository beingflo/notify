# notify

A small HTTP service that takes a payload and mails it through Fastmail SMTP
submission.

## Routes

| Route            | Auth                                      | Limit                    | Body                                        |
| ---------------- | ----------------------------------------- | ------------------------ | ------------------------------------------- |
| `POST /feedback` | none, CORS-restricted to the form origins | 3/min and 20/day, global | `{project, path?, email?, content}`, 16 KiB |
| `POST /alert`    | `Authorization: Bearer $ALERT_TOKEN`      | none (a storm is logged) | `{source, severity, title, body}`, 64 KiB   |
| `GET /health`    | none                                      | none                     | `{"status":"ok","smtp":"live"\|"dry-run"}`  |

## Configuration

- `SERVE_PORT` — default `8080`.
- `SMTP_USER` — the Fastmail account's *login* address: the row marked
  "Username" in Settings → My email addresses. `feedback@marending.dev` is an
  alias, and aliases can send as themselves but cannot authenticate (`535`).
- `SMTP_PASSWORD` — app password for that account, SMTP scope.
- `ALERT_TOKEN` — bearer token for `/alert`. Absent: `/alert` answers `503`.
  Fail closed; a missing secret never means "no token required".

`SMTP_USER` and `SMTP_PASSWORD` are a pair: with either half missing the service
runs in dry run, logs the payload instead of sending it, and `/health` says
`dry-run`. Both are podman secrets, because the login address is personal and
this repository is public.

Everything else — SMTP host and port, addresses, CORS origins, both limits,
queue capacity, attempts and backoff, body and field caps, send timeout — is a
constant in `src/config.rs`.

## Development

```sh
cargo test                 # 37 tests, no network, no SMTP
SERVE_PORT=8080 cargo run  # no SMTP_USER/SMTP_PASSWORD: dry run, payloads logged
```

```sh
curl -s localhost:8080/health
curl -s -X POST localhost:8080/feedback \
  -H 'content-type: application/json' \
  -d '{"project":"journal","email":"","content":"hello","path":""}'
```

## Deployment

```sh
mise run deploy:edit-secrets   # once: fill in the three values (sops ciphertext)
mise run deploy:secrets        # install them on the host, restart
mise run deploy                # build, ship, restart, health check
mise run deploy:build          # build the image only
```
