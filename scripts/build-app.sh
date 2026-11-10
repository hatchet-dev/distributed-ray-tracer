#!/bin/sh
# Builds what deploy/app.Dockerfile copies: the page's preview module (app/public/preview/) and
# the Node server bundle (app/dist/server.mjs).
set -e
cd "$(dirname "$0")/.."
scripts/build-preview.sh
cd app && pnpm run build:node
