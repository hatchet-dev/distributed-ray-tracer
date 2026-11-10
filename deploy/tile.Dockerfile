# The tile endpoint (render-tile, validate-shape) as a native server: crates/server.
#   docker build -f deploy/tile.Dockerfile -t ray-tile-server .
FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release -p ray-tile-server

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/ray-tile-server /usr/local/bin/ray-tile-server
ENV PORT=8080
EXPOSE 8080
USER nobody
CMD ["ray-tile-server"]
