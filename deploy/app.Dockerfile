# The app (page, render API, orchestration workflows) as a Node server: app/src/node.ts.
#   docker build -f deploy/app.Dockerfile -t ray-tracer-app .
# Secrets from the environment: HATCHET_CLIENT_TOKEN, HATCHET_SIGNING_SECRET, OPENAI_API_KEY.
# The page's preview module (Rust compiled to wasm, scripts/build-preview.sh) isn't built here:
# this image is for serving the workflows under /hatchet; the page itself is served by the Worker.
FROM node:22-bookworm-slim AS app
WORKDIR /src/app
RUN corepack enable && corepack prepare pnpm@10.30.3 --activate
COPY app/package.json app/pnpm-lock.yaml ./
RUN pnpm install --frozen-lockfile
COPY app/ ./
RUN pnpm run build:node

FROM node:22-bookworm-slim
WORKDIR /app
COPY --from=app /src/app/dist/server.mjs ./server.mjs
COPY --from=app /src/app/public ./public
ENV PORT=8080 NODE_ENV=production
EXPOSE 8080
USER node
CMD ["node", "server.mjs"]
