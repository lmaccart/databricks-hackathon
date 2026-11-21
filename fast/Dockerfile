# Shopping Sessions fast pipeline. Build: docker build --platform linux/amd64 -t ss .
# The scorer builds this at the repo root (network allowed during build, up to 10 min),
# then runs it with /data read-only, /output and /scratch writable, uid 1000, no network.
FROM --platform=linux/amd64 rust:1-slim-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
# cache dependencies in their own layer
RUN mkdir src && echo 'fn main(){}' > src/main.rs && cargo build --release && rm -rf src
COPY src ./src
RUN touch src/main.rs && cargo build --release && strip target/release/shopping_sessions

FROM --platform=linux/amd64 debian:bookworm-slim
COPY --from=build /src/target/release/shopping_sessions /usr/local/bin/shopping_sessions
USER 1000
ENV DATA_DIR=/data OUTPUT_DIR=/output
ENTRYPOINT ["/usr/local/bin/shopping_sessions"]
