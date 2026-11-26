FROM rust:1.100-bookworm AS builder
RUN apt-get update && apt-get install -y --no-install-recommends capnproto && rm -rf /var/lib/apt/lists/*
WORKDIR /build
COPY Cargo.toml Cargo.lock build.rs ./
COPY src ./src
COPY resources ./resources
# The symbol table only names functions in a backtrace; a panic still reports its message and
# source location (docs/hars/DEVELOPMENT.md).
RUN cargo build --locked --release && strip target/release/roadviewer-hars

FROM debian:bookworm-slim
ARG BUILD_VERSION=2.1.0
ARG BUILD_ARCH=amd64
LABEL io.hass.version="${BUILD_VERSION}" \
      io.hass.type="app" \
      io.hass.arch="${BUILD_ARCH}" \
      org.opencontainers.image.source="https://github.com/Qualified4/roadviewer-hars"
RUN apt-get update && apt-get install -y --no-install-recommends nginx ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=builder /build/target/release/roadviewer-hars /usr/local/bin/roadviewer-hars
COPY web /app/web
ENV HARS_DATA=/data/roadviewer-hars HARS_WEB=/app/web HARS_INGRESS_ONLY=1
EXPOSE 8099 8443
STOPSIGNAL SIGTERM
ENTRYPOINT ["/usr/local/bin/roadviewer-hars"]
CMD ["ha-start"]
