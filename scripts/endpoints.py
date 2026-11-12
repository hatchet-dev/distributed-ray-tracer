#!/usr/bin/env python3
"""Hatchet serverless endpoints for this tenant (from HATCHET_CLIENT_TOKEN in .env).

  scripts/endpoints.py list                       each endpoint, enabled or not, and its last events
  scripts/endpoints.py enable NAME | disable NAME switch one on or off
  scripts/endpoints.py register NAME KIND BASE    create or update one (KIND: GENERIC_HTTP or
                                                  CLOUDFLARE_WORKERS; BASE: https://host/hatchet)

The app's workflows (describe-and-render, render-scene) are served by two endpoints:
ray-tracer-app (the Cloudflare Worker) and ray-tracer-app-porter (porter.yaml's chat service).
Keep one enabled: e.g. `disable ray-tracer-app && enable ray-tracer-app-porter`.
"""
import base64, json, os, ssl, sys, urllib.request, urllib.error
env=dict(l.split('=',1) for l in open(os.path.join(os.path.dirname(os.path.abspath(__file__)), '..', '.env')).read().splitlines() if '=' in l)
token=env['HATCHET_CLIENT_TOKEN'].strip().strip('"'); secret=env['HATCHET_SIGNING_SECRET'].strip().strip('"')
part=token.split('.')[1]; claims=json.loads(base64.urlsafe_b64decode(part+'='*(-len(part)%4)))
api, tenant = claims['server_url'].rstrip('/'), claims['sub']
ctx = ssl.create_default_context(cafile='/etc/ssl/cert.pem') if os.path.exists('/etc/ssl/cert.pem') else None
def call(method, path, body=None):
    req = urllib.request.Request(api+path, method=method, data=json.dumps(body).encode() if body else None,
        headers={'Authorization': f'Bearer {token}','Content-Type':'application/json','User-Agent':'ray-tracer-deploy/1.0'})
    try:
        with urllib.request.urlopen(req, context=ctx) as r: return json.load(r)
    except urllib.error.HTTPError as e: return {'error': f'{e.code} {e.read().decode()[:300]}'}
def endpoints(): return {r['name']: r for r in call('GET', f'/api/v1/stable/tenants/{tenant}/serverless/endpoints?limit=200').get('rows', [])}
cmd = sys.argv[1]
if cmd == 'register':
    name, kind, base = sys.argv[2:5]
    spec = {'kind': kind, 'healthcheckUrl': base+'/healthcheck', 'triggerUrl': base+'/trigger', 'signingSecret': secret, 'requestTimeoutSeconds': 300, 'enabled': True}
    ex = endpoints().get(name)
    out = call('PATCH', f"/api/v1/stable/serverless/endpoints/{ex['metadata']['id']}", spec) if ex else call('POST', f'/api/v1/stable/tenants/{tenant}/serverless/endpoints', {'name': name, **spec})
    print(name, '->', out.get('error') or ('updated' if ex else 'created'))
elif cmd in ('enable', 'disable'):
    ex = endpoints()[sys.argv[2]]
    out = call('PATCH', f"/api/v1/stable/serverless/endpoints/{ex['metadata']['id']}", {'enabled': cmd == 'enable'})
    print(sys.argv[2], '->', out.get('error') or cmd + 'd')
elif cmd == 'list':
    for n, e in endpoints().items():
        ev = call('GET', f"/api/v1/stable/serverless/endpoints/{e['metadata']['id']}/events?limit=3").get('rows', [])
        print(f"{n:24} enabled={e.get('enabled')} {e.get('kind')} {e.get('triggerUrl')}  last: {[(x['kind'], x['message'][:60]) for x in ev[:2]]}")
