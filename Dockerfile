# Shopping Sessions fast pipeline. Build: docker build --platform linux/amd64 -t ss .
# The scorer builds this at the repo root (network allowed during build, up to 10 min),
# then runs it with /data read-only, /output and /scratch writable, uid 1000, no network.
FROM --platform=linux/amd64 rust:1.100-slim-bookworm AS build
WORKDIR /src
COPY fast/Cargo.toml fast/Cargo.lock ./
COPY fast/src ./src
RUN cargo build --locked --release && strip target/release/shopping_sessions

FROM --platform=linux/amd64 debian:bookworm-slim
COPY --from=build /src/target/release/shopping_sessions /usr/local/bin/shopping_sessions
USER 1000
ENV DATA_DIR=/data OUTPUT_DIR=/output
# Trial: let subsequent stages reuse freed allocator pages for this short run.
ENV MIMALLOC_PURGE_DELAY=-1
ENTRYPOINT ["/usr/local/bin/shopping_sessions"]
