FROM rust:1.87-bookworm AS build
WORKDIR /app

COPY Cargo.toml Cargo.toml
COPY src src

RUN cargo build --release

FROM debian:bookworm-slim
WORKDIR /app

COPY --from=build /app/target/release/elowen-api /usr/local/bin/elowen-api

EXPOSE 8080

CMD ["elowen-api"]
