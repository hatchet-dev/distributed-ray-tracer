# One image for both Porter services (porter.yaml): the Rust tile server (`ray-tile-server`) and
# the Node app that serves the orchestration workflows (`node /app/server.mjs`). Porter builds one
# image per app, so both live here; each service runs its own command.
#   docker build -f deploy/porter.Dockerfile -t ray-tracer .
FROM rust:1-bookworm AS tiles
WORKDIR /src
COPY . .
RUN cargo build --release -p ray-tile-server

FROM node:22-bookworm-slim AS app
WORKDIR /src/app
RUN corepack enable && corepack prepare pnpm@10.30.3 --activate
COPY app/package.json app/pnpm-lock.yaml ./
RUN pnpm install --frozen-lockfile
COPY app/ ./
RUN pnpm run build:node

FROM node:22-bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=tiles /src/target/release/ray-tile-server /usr/local/bin/ray-tile-server
WORKDIR /app
COPY --from=app /src/app/dist/server.mjs ./server.mjs
COPY --from=app /src/app/public ./public
ENV PORT=8080 NODE_ENV=production
EXPOSE 8080
USER node
CMD ["ray-tile-server"]
