# The app (page, render API, orchestration workflows) as a Node server: app/src/node.ts.
# Build the bundle and the page's preview module first, then the image:
#   scripts/build-app.sh
#   docker build -f deploy/app.Dockerfile -t ray-tracer-app .
# (The bundle is built outside Docker because app/ links a local build of the Hatchet TypeScript
# SDK; once the SDK release has streams.subscribe, this can build from source in a stage.)
FROM node:22-bookworm-slim
WORKDIR /app
COPY app/dist/server.mjs ./server.mjs
COPY app/public ./public
ENV PORT=8080 NODE_ENV=production
EXPOSE 8080
USER node
CMD ["node", "server.mjs"]
