# elowen-api

Rust service responsible for thread/message APIs, job orchestration, device routing, approvals, and summary coordination.

## Initial Scope

- expose thread and job HTTP APIs
- coordinate job lifecycle state
- dispatch work over NATS JetStream
- persist operational state in Postgres

This scaffold is intentionally minimal. It gives the repo a clear entry point without locking framework details too early.
