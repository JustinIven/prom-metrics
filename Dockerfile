# syntax=docker/dockerfile:1

FROM rust:1-alpine AS builder
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked --bin prom-metrics \
    && strip target/release/prom-metrics

FROM scratch
COPY --from=builder /src/target/release/prom-metrics /prom-metrics
USER 65532:65532
EXPOSE 8443
ENTRYPOINT ["/prom-metrics"]
