FROM rust:1.88-bookworm AS build
WORKDIR /app/elowen-api

COPY elowen-api/Cargo.toml Cargo.toml
COPY elowen-api/Cargo.lock Cargo.lock
COPY elowen-api/migrations migrations
COPY elowen-api/src src
COPY elowen-platform/contracts/rust/elowen-contracts ../elowen-platform/contracts/rust/elowen-contracts

RUN cargo build --release

FROM debian:bookworm-slim
WORKDIR /app

COPY --from=build /app/elowen-api/target/release/elowen-api /usr/local/bin/elowen-api

EXPOSE 8080

CMD ["elowen-api"]
