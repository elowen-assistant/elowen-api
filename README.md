# elowen-api

Rust orchestrator service responsible for Elowen's thread, job, device, approval, authentication, and realtime state APIs.

## Current Responsibilities

- expose thread, message, job, approval, device, note, and authentication HTTP APIs
- provide orchestrator-side conversational replies for Workflow #2 when the OpenAI runtime is configured
- generate and persist execution drafts that can be promoted into laptop-backed jobs
- coordinate explicit handoff from conversational chat into real edge execution
- dispatch jobs and approval commands over NATS
- persist operational state in Postgres
- consume edge lifecycle events and publish thread/job/device changes to the UI over authenticated server-sent events
- integrate with `elowen-notes` for thread and job context
- enforce web UI session authentication when configured
- issue signed registration challenges and verify edge-signed registration proofs for trusted edge registration

## Runtime Notes

The VPS deployment runs `elowen-api` from a prebuilt GHCR image rather than compiling on the server. Local development still uses normal Cargo workflows.

Important environment variables include:

- `DATABASE_URL`
- `NATS_URL`
- `ELOWEN_NOTES_URL`
- `OPENAI_API_KEY`
- `ELOWEN_UI_PASSWORD`
- `ELOWEN_ORCHESTRATOR_SIGNING_KEY`
- `ELOWEN_REQUIRE_TRUSTED_EDGE_REGISTRATION`

## Verification

Useful local checks:

```bash
cargo fmt --check
cargo test --quiet
cargo clippy --all-targets -- -D warnings
cargo doc --no-deps
```
