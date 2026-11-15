FROM rust:1.100-bookworm AS builder
RUN apt-get update && apt-get install -y --no-install-recommends capnproto pkg-config libclang-dev libavformat-dev libavcodec-dev libavutil-dev && rm -rf /var/lib/apt/lists/*
WORKDIR /build
COPY Cargo.toml Cargo.lock build.rs ./
COPY reference/python/roadviewer/schema ./reference/python/roadviewer/schema
COPY src ./src
COPY resources ./resources
RUN cargo build --locked --release

FROM debian:bookworm-slim
ARG BUILD_VERSION=2.0.0-alpha.1
ARG BUILD_ARCH=amd64
LABEL io.hass.version="${BUILD_VERSION}" \
      io.hass.type="app" \
      io.hass.arch="${BUILD_ARCH}" \
      org.opencontainers.image.source="https://github.com/Qualified4/roadviewer-hars"
RUN apt-get update && apt-get install -y --no-install-recommends ffmpeg nginx ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=builder /build/target/release/roadviewer-hars /usr/local/bin/roadviewer-hars
COPY reference/python/roadviewer/app/web /app/web
ENV HARS_DATA=/data/roadviewer-hars HARS_WEB=/app/web HARS_INGRESS_ONLY=1
EXPOSE 8099 8443
STOPSIGNAL SIGTERM
ENTRYPOINT ["/usr/local/bin/roadviewer-hars"]
CMD ["ha-start"]
