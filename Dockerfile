# Shopping Sessions fast pipeline. Build: docker build --platform linux/amd64 -t ss .
# The scorer builds this at the repo root (network allowed during build, up to 10 min),
# then runs it with /data read-only, /output and /scratch writable, uid 1000, no network.
ARG HIGHWAY_SORT=1
FROM --platform=linux/amd64 rust:1.100-slim-bookworm AS build
WORKDIR /src
ARG HIGHWAY_SORT
RUN if [ "$HIGHWAY_SORT" = "1" ]; then \
    apt-get update && apt-get install -y --no-install-recommends \
    libhwy-dev=1.0.3-3+deb12u1 && rm -rf /var/lib/apt/lists/*; \
    fi
COPY fast/Cargo.toml fast/Cargo.lock ./
COPY fast/src ./src
COPY fast/vendor ./vendor
RUN if [ "$HIGHWAY_SORT" = "1" ]; then \
    cargo build --locked --release --features highway-sort; \
    else cargo build --locked --release; \
    fi && strip target/release/shopping_sessions

FROM --platform=linux/amd64 debian:bookworm-slim
ARG HIGHWAY_SORT
RUN if [ "$HIGHWAY_SORT" = "1" ]; then \
    apt-get update && apt-get install -y --no-install-recommends \
    libhwy1=1.0.3-3+deb12u1 && rm -rf /var/lib/apt/lists/*; \
    fi
COPY --from=build /src/target/release/shopping_sessions /usr/local/bin/shopping_sessions
USER 1000
ENV DATA_DIR=/data OUTPUT_DIR=/output
ENTRYPOINT ["/usr/local/bin/shopping_sessions"]
