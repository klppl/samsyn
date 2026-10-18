# Build context is the repository root, so the vendored third_party/yrs that
# server/Cargo.toml patches in (`../third_party/yrs`) is available.
#   docker build -f server/Dockerfile -t samsyn .
# cargo-chef splits the build so dependencies compile in a layer that only
# changes when Cargo.toml/Cargo.lock/third_party do, not on every source edit.
FROM lukemathwalker/cargo-chef:latest-rust-1-bookworm AS chef
WORKDIR /src/server

FROM chef AS planner
COPY server/Cargo.toml server/Cargo.lock ./
COPY server/src src
COPY third_party /src/third_party
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS build
COPY --from=planner /src/server/recipe.json recipe.json
COPY third_party /src/third_party
RUN cargo chef cook --release --recipe-path recipe.json
COPY server/Cargo.toml server/Cargo.lock ./
COPY server/src src
RUN cargo build --release --bin samsyn

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/server/target/release/samsyn /usr/local/bin/samsyn
ENV SAMSYN_DATA_DIR=/data SAMSYN_BIND=0.0.0.0:8787
VOLUME ["/data"]
EXPOSE 8787
EXPOSE 8788/udp
ENTRYPOINT ["samsyn"]
CMD ["serve"]
