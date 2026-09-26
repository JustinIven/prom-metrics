# syntax=docker/dockerfile:1

FROM lukemathwalker/cargo-chef:latest-rust-1-alpine AS chef
RUN apk add --no-cache musl-dev
WORKDIR /src

FROM chef AS planner
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo chef prepare --recipe-path recipe.json

# Builds just the dependency graph, so this layer is only invalidated by Cargo.lock changes.
FROM chef AS builder
COPY --from=planner /src/recipe.json recipe.json
RUN cargo chef cook --release --locked --recipe-path recipe.json
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked --bin prom-metrics

FROM scratch
COPY --from=builder /src/target/release/prom-metrics /prom-metrics
USER 65532:65532
EXPOSE 8443
ENTRYPOINT ["/prom-metrics"]
