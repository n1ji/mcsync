# Build from the repo root:  docker build -f server/Dockerfile -t mcsync-server .
FROM rust:1.99-slim-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release --locked -p mcsync-server \
 && mkdir -p /out/data \
 && cp target/release/mcsync-server /out/

# Distroless: no shell, no package manager, runs as non-root (uid 65532).
FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /out/mcsync-server /usr/local/bin/mcsync-server
COPY --from=build --chown=65532:65532 /out/data /data
ENV MCSYNC_DATA=/data \
    MCSYNC_BIND=0.0.0.0:8080
VOLUME ["/data"]
EXPOSE 8080
USER nonroot
ENTRYPOINT ["/usr/local/bin/mcsync-server"]
CMD ["serve"]
