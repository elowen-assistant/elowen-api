# elowen-api

## Purpose

Rust orchestrator service for Elowen. It owns the HTTP API, Postgres-backed operational state, NATS coordination, authenticated UI event streaming, and orchestrator-side chat assistance.

## Current Responsibilities

- expose thread, message, job, approval, device, note, and authentication endpoints
- persist thread, job, approval, device, and session state in Postgres
- publish authenticated server-sent events for thread, job, and device changes
- dispatch jobs and approval commands over NATS
- consume edge lifecycle events and fold them back into thread and job state
- generate orchestrator-side assistant replies and execution drafts when the OpenAI runtime is configured
- integrate with `elowen-notes` for thread and job context
- issue signed registration challenges and verify trusted edge registration proofs

## Repository Layout

- `src/app/` - startup, router assembly, middleware, SSE wiring, and tracing
- `src/routes/` - HTTP handlers grouped by API surface
- `src/services/` - assistant replies, lifecycle handling, UI events, notes integration, and job dispatch helpers
- `src/db/` - Postgres queries grouped by domain
- `src/models/` - transport models and persistence row types
- `src/trust/` - registration challenge and proof verification helpers
- `migrations/` - Postgres schema migrations

## Runtime And Config Entrypoints

Run locally with:

```bash
cargo run
```

Important environment variables:

- `DATABASE_URL`
- `NATS_URL`
- `ELOWEN_NOTES_URL`
- `OPENAI_API_KEY`
- `ELOWEN_UI_PASSWORD`
- `ELOWEN_ORCHESTRATOR_SIGNING_KEY`
- `ELOWEN_ORCHESTRATOR_SIGNING_KEY_FILE`
- `ELOWEN_ORCHESTRATOR_SIGNING_KEY_FILES`
- `ELOWEN_REQUIRE_TRUSTED_EDGE_REGISTRATION`

`ELOWEN_ORCHESTRATOR_SIGNING_KEY` and `ELOWEN_ORCHESTRATOR_SIGNING_KEYS` remain supported for compatibility. For hardened deployments, mount signer private keys as files and set `ELOWEN_ORCHESTRATOR_SIGNING_KEY_FILE` for one key or `ELOWEN_ORCHESTRATOR_SIGNING_KEY_FILES` for a comma-separated or JSON array list. The API derives public signer metadata from those private keys and persists only public key ids, public keys, status, and lifecycle timestamps.

## Local Verification

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --quiet
cargo doc --no-deps
```

## Related Docs

- `migrations/`
- `../elowen-platform/docs/vps-deployment.md`
- `../elowen-platform/env/`
