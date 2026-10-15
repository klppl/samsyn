# Build context is the repository root, so the vendored third_party/yrs that
# server/Cargo.toml patches in (`../third_party/yrs`) is available.
#   docker build -f server/Dockerfile -t samsyn .
FROM rust:1-bookworm AS build
WORKDIR /src
COPY server/Cargo.toml server/Cargo.lock server/
COPY server/src server/src
COPY third_party third_party
WORKDIR /src/server
RUN cargo build --release --bin samsyn

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/replica-server /usr/local/bin/replica-server
ENV REPLICA_DATA_DIR=/data REPLICA_BIND=0.0.0.0:8787
VOLUME ["/data"]
EXPOSE 8787
EXPOSE 8788/udp
ENTRYPOINT ["replica-server"]
CMD ["serve"]
