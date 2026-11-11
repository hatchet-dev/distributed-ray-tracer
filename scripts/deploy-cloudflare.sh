#!/usr/bin/env bash
# Deploys the Cloudflare half of the project, the app Worker (app/: page, render API,
# orchestration workflows), and uploads its secrets from an env file. The tile renderer runs on
# Porter (porter.yaml) and serves render-tiles and validate-shape, so there's no tile Worker.
#
#   scripts/deploy-cloudflare.sh [--env FILE]... [--register https://your.domain] [--no-secrets]
#
#   --env FILE   env file(s) to read secrets from, earlier files win (default: .env, then app/.env)
#   --register   after deploying, create or update the app's Hatchet serverless endpoint
#                (ray-tracer-app) at <url>/hatchet/{healthcheck,trigger}, with the uploaded
#                signing secret, in the tenant HATCHET_CLIENT_TOKEN belongs to
#   --no-secrets deploy the code only
#
# Secrets uploaded (when present): HATCHET_CLIENT_TOKEN, HATCHET_SIGNING_SECRET, OPENAI_API_KEY.
# Values are never printed.
#
# Which Cloudflare account: whatever `wrangler login` is signed into, or set CLOUDFLARE_ACCOUNT_ID
# (and CLOUDFLARE_API_TOKEN for non-interactive use). For a custom domain, add to app/wrangler.toml:
#   routes = [{ pattern = "ray.example.com", custom_domain = true }]
# (the domain's zone must be on that account).
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT=$PWD

SECRETS=(HATCHET_CLIENT_TOKEN HATCHET_SIGNING_SECRET OPENAI_API_KEY)
ENDPOINT_NAME=ray-tracer-app
env_files=()
register=""
upload_secrets=1

while [ $# -gt 0 ]; do
  case "$1" in
    --env) env_files+=("$2"); shift 2 ;;
    --register) register="${2%/}"; shift 2 ;;
    --no-secrets) upload_secrets=0; shift ;;
    -h|--help) sed -n '2,22p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done
[ ${#env_files[@]} -gt 0 ] || env_files=(.env app/.env)
# Absolute, since the deploy runs from app/.
for i in "${!env_files[@]}"; do
  case "${env_files[$i]}" in /*) ;; *) env_files[$i]="$ROOT/${env_files[$i]}" ;; esac
done

# The value of KEY from the first env file that has it: quotes, CR and a repeated `KEY=` stripped.
value() {
  local key=$1 f line
  for f in "${env_files[@]}"; do
    [ -f "$f" ] || continue
    line=$(grep -E "^(export +)?$key=" "$f" | tail -1 || true)
    [ -n "$line" ] || continue
    line=${line#export }
    line=${line#"$key="}
    line=${line#"$key="} # a pasted `KEY=value` line inside the value
    line=$(printf '%s' "$line" | tr -d '"\r' | sed "s/^'//; s/'\$//; s/[[:space:]]*\$//")
    printf '%s' "$line"
    return 0
  done
  return 1
}

export PATH="$HOME/.cargo/bin:$PATH" # the preview module's build (wrangler's [build] step)
command -v cargo >/dev/null || { echo "cargo not found: install Rust (rustup) first" >&2; exit 1; }
rustup target list --installed 2>/dev/null | grep -q wasm32-unknown-unknown \
  || rustup target add wasm32-unknown-unknown

cd "$ROOT/app"
pnpm install --frozen-lockfile >/dev/null
wrangler() { pnpm exec wrangler "$@"; }

echo "Cloudflare account:"
wrangler whoami 2>/dev/null | grep -E "associated with|│" | head -5 || true

echo "Deploying the app Worker..."
deploy_log=$(mktemp)
trap 'rm -f "$deploy_log"' EXIT
wrangler deploy 2>&1 | tee "$deploy_log"
url=$(grep -Eo 'https://[A-Za-z0-9.-]+\.workers\.dev' "$deploy_log" | head -1 || true)

if [ $upload_secrets = 1 ]; then
  for key in "${SECRETS[@]}"; do
    if v=$(value "$key") && [ -n "$v" ]; then
      printf '%s' "$v" | wrangler secret put "$key" >/dev/null && echo "  secret $key uploaded"
    else
      echo "  secret $key not in ${env_files[*]}: left as is" >&2
    fi
  done
fi

if [ -n "$register" ]; then
  token=$(value HATCHET_CLIENT_TOKEN) || { echo "--register needs HATCHET_CLIENT_TOKEN" >&2; exit 1; }
  secret=$(value HATCHET_SIGNING_SECRET) || { echo "--register needs HATCHET_SIGNING_SECRET" >&2; exit 1; }
  echo "Registering $register/hatchet with Hatchet..."
  TOKEN=$token SECRET=$secret BASE="$register/hatchet" NAME=$ENDPOINT_NAME python3 - <<'EOF'
import base64, json, os, ssl, urllib.request
token, base = os.environ["TOKEN"], os.environ["BASE"]
part = token.split(".")[1]
claims = json.loads(base64.urlsafe_b64decode(part + "=" * (-len(part) % 4)))
api, tenant = claims["server_url"].rstrip("/"), claims["sub"]
ctx = ssl.create_default_context(cafile="/etc/ssl/cert.pem") if os.path.exists("/etc/ssl/cert.pem") else None

def call(method, path, body=None):
    req = urllib.request.Request(api + path, method=method, data=json.dumps(body).encode() if body else None,
                                 headers={"Authorization": f"Bearer {token}", "Content-Type": "application/json",
                                          "User-Agent": "ray-tracer-deploy/1.0"})
    try:
        with urllib.request.urlopen(req, context=ctx) as r:
            return json.load(r)
    except urllib.error.HTTPError as e:
        raise SystemExit(f"{method} {path}: {e.code} {e.read().decode()[:300]}")

spec = {"kind": "CLOUDFLARE_WORKERS", "healthcheckUrl": base + "/healthcheck", "triggerUrl": base + "/trigger",
        "signingSecret": os.environ["SECRET"], "requestTimeoutSeconds": 300, "enabled": True}
rows = call("GET", f"/api/v1/stable/tenants/{tenant}/serverless/endpoints?limit=200").get("rows", [])
existing = next((r for r in rows if r.get("name") == os.environ["NAME"]), None)
if existing:
    call("PATCH", f"/api/v1/stable/serverless/endpoints/{existing['metadata']['id']}", spec)
    print(f"  updated endpoint {os.environ['NAME']} -> {spec['triggerUrl']}")
else:
    call("POST", f"/api/v1/stable/tenants/{tenant}/serverless/endpoints", {"name": os.environ["NAME"], **spec})
    print(f"  created endpoint {os.environ['NAME']} -> {spec['triggerUrl']}")
EOF
fi

echo
echo "Done.${url:+ Worker URL: $url}"
[ -n "$register" ] || echo "Register <domain>/hatchet/{healthcheck,trigger} with Hatchet (or rerun with --register https://<domain>)."
