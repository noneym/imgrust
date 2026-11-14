# syntax=docker/dockerfile:1

# ---- build -----------------------------------------------------------------
FROM rust:1-trixie AS build

RUN apt-get update \
 && apt-get install -y --no-install-recommends libvips-dev pkg-config \
 && rm -rf /var/lib/apt/lists/*

WORKDIR /app

# Build dependencies first so they stay cached while the source changes.
COPY Cargo.toml Cargo.lock build.rs ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs \
 && cargo build --release --locked \
 && rm -rf src

COPY src ./src
RUN touch src/main.rs && cargo build --release --locked

# ---- runtime ---------------------------------------------------------------
FROM debian:trixie-slim

RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      libvips42t64 \
      libheif-plugin-aomenc \
      libheif-plugin-dav1d \
      libheif-plugin-libde265 \
      ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --no-create-home --uid 10001 imgrust

COPY --from=build /app/target/release/imgrust /usr/local/bin/imgrust

# glibc keeps one malloc arena per thread by default, which fragments badly
# with image-sized allocations; two arenas keep RSS flat under load.
ENV MALLOC_ARENA_MAX=2 \
    IMGRUST_BIND=0.0.0.0:8080

USER imgrust
EXPOSE 8080
STOPSIGNAL SIGTERM

HEALTHCHECK --interval=10s --timeout=3s --start-period=5s --retries=3 \
  CMD ["imgrust", "health"]

ENTRYPOINT ["imgrust"]
