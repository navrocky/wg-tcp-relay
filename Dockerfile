# syntax=docker/dockerfile:1

# Static musl build: rust:alpine targets *-linux-musl natively and links statically.
FROM rust:1-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /src

# Build dependencies in a separate layer so source changes do not rebuild them.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src \
    && echo 'fn main() {}' > src/main.rs \
    && touch src/lib.rs \
    && cargo build --release --locked \
    && rm -rf src

COPY src src
RUN touch src/main.rs src/lib.rs \
    && cargo build --release --locked \
    && cp target/release/wg-tcp-relay /wg-tcp-relay

# Distroless: no shell, no libc, runs as uid 65532.
FROM gcr.io/distroless/static-debian12:nonroot
COPY --from=build /wg-tcp-relay /wg-tcp-relay
ENTRYPOINT ["/wg-tcp-relay"]
