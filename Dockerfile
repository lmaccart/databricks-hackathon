# Fast path: Team 67's (MIT) C++ build, github.com/mwkhettab/shopping-sessions-cpp @ 29ee0cc; see cpp/pipeline/LICENSE.
# General path: our Rust pipeline (fast/), used only when cpp/launcher/launch.c finds a non-canonical input layout.
# Scorer contract: linux/amd64, /data read-only, /output writable, uid 1000, no network at run time.
FROM debian:bookworm-slim@sha256:734a72b3d6142a71993b4bc580ca01d8ca687f77a2c422d96a961d3376b179bc AS build
RUN apt-get update && apt-get install -y --no-install-recommends g++ clang-16 lld-16 \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY cpp/pipeline cpp/pipeline
COPY build.sh .
# GCC, as in Team 67's 0.33 s submission (dc216ac / corrected 5230d48): build.sh's
# non-Clang branch is exactly that release's command. Their later Clang build scored 0.976 s for us.
RUN CXX=g++ STATIC=1 sh build.sh
COPY cpp/launcher cpp/launcher
RUN clang-16 -O2 -static -o build/launch cpp/launcher/launch.c

FROM rust:1.100-slim-bookworm AS general
WORKDIR /src
COPY fast/Cargo.toml fast/Cargo.lock ./
COPY fast/src ./src
# Static, so the empty runtime image needs no loader; --target keeps build scripts dynamic.
RUN target=$(rustc -vV | sed -n 's/^host: //p') \
    && RUSTFLAGS="-C target-feature=+crt-static" cargo build --locked --release --target "$target" \
    && strip "target/$target/release/shopping_sessions" \
    && cp "target/$target/release/shopping_sessions" /shopping-general

FROM scratch
COPY --from=build /src/build/shopping /app/shopping
COPY --from=build /src/build/launch /app/launch
COPY --from=general /shopping-general /app/shopping-general
COPY cpp/pipeline/LICENSE /app/LICENSE
USER 1000:1000
ENV DATA_DIR=/data OUTPUT_DIR=/output
ENTRYPOINT ["/app/launch"]
