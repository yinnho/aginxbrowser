# Build stage — compile with the same feature set as the release CI
# (cargo build --release --features stealth,screenshot).
FROM rust:1.94-bookworm AS build
WORKDIR /src

# Dependency layer: copy manifests first so Docker layer caching survives
# source edits.
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src && echo 'fn main() {}' > src/main.rs && cargo build --release --features stealth,screenshot || true

COPY . .
RUN touch src/main.rs && cargo build --release --features stealth,screenshot

# Runtime stage — the binary is self-contained (V8 + HTTP stack + rendering
# engine are statically linked in); curl only for the HEALTHCHECK.
FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/aginxbrowser /usr/local/bin/aginxbrowser

ENV AGINXBROWSER_BIND=0.0.0.0:8089
EXPOSE 8089

HEALTHCHECK --interval=30s --timeout=5s --start-period=10s \
  CMD ["curl", "-fsS", "http://127.0.0.1:8089/health"]

ENTRYPOINT ["/usr/local/bin/aginxbrowser"]
CMD []
