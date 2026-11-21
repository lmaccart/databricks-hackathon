# Shopping Sessions fast pipeline. Build: docker build --platform linux/amd64 -t ss .
# The scorer builds this at the repo root (network allowed during build, up to 10 min),
# then runs it with /data read-only, /output and /scratch writable, uid 1000, no network.
FROM --platform=linux/amd64 rust:1.100-slim-bookworm AS build
WORKDIR /src
COPY fast/Cargo.toml fast/Cargo.lock ./fast/
COPY fast/src ./fast/src
COPY tools/pgo_build.sh tools/pgo_generate.rs ./tools/
ARG PGO=1
RUN if [ "$PGO" = 1 ]; then \
      rustup component add llvm-tools-preview && sh tools/pgo_build.sh /src/shopping_sessions; \
    elif [ "$PGO" = 0 ]; then \
      cargo build --locked --release --manifest-path fast/Cargo.toml && \
      cp fast/target/release/shopping_sessions /src/shopping_sessions; \
    else echo "PGO must be 0 or 1" >&2; exit 1; fi && \
    strip /src/shopping_sessions

FROM --platform=linux/amd64 debian:bookworm-slim
COPY --from=build /src/shopping_sessions /usr/local/bin/shopping_sessions
USER 1000
ENV DATA_DIR=/data OUTPUT_DIR=/output
ENTRYPOINT ["/usr/local/bin/shopping_sessions"]
