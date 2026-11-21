import { Buffer } from 'node:buffer';
import { spawn, spawnSync } from 'node:child_process';
import crypto from 'node:crypto';
import fs from 'node:fs';
import http from 'node:http';
import os from 'node:os';
import path from 'node:path';
import { URLSearchParams, fileURLToPath } from 'node:url';

const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);

const SERVER_URL = process.env.KEEL_BASE_URL || 'http://127.0.0.1:18080';
const BIND_ADDR = '127.0.0.1:18080';
const OIDC_PORT = 18081;
const OIDC_ISSUER = `http://127.0.0.1:${OIDC_PORT}`;
/** Well-known file the specs read to locate the seeded bare Git repo (git-level assertions). */
const HARNESS_STATE_FILE = path.join(os.tmpdir(), 'keel-e2e-harness.json');

/**
 * Mock IdP users. The username typed into the mock login form selects the profile, so specs can
 * authenticate as a cluster admin, a tenant member, or a tenant admin (`tenant-<id>[-admin]`).
 */
const MOCK_USERS = {
  'admin@keel.local': { name: 'Keel E2E Admin', groups: ['keel-admins'] },
  'acme-dev@keel.local': { name: 'Acme Developer', groups: ['tenant-acme'] },
  'acme-admin@keel.local': { name: 'Acme Tenant Admin', groups: ['tenant-acme-admin'] },
  'default-dev@keel.local': { name: 'Default Developer', groups: ['tenant-default'] },
};

function escapeHtml(str) {
  return String(str)
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;')
    .replace(/'/g, '&#39;');
}

function startMockOidcServer() {
  const b64url = (obj) => Buffer.from(JSON.stringify(obj)).toString('base64url');
  const { publicKey, privateKey } = crypto.generateKeyPairSync('rsa', {
    modulusLength: 2048,
  });
  const publicJwk = {
    ...publicKey.export({ format: 'jwk' }),
    kid: 'e2e-key-1',
    use: 'sig',
    alg: 'RS256',
  };
  const server = http.createServer((req, res) => {
    const url = new URL(req.url || '/', OIDC_ISSUER);
    if (req.method === 'GET' && url.pathname === '/.well-known/openid-configuration') {
      res.writeHead(200, { 'Content-Type': 'application/json' });
      res.end(
        JSON.stringify({
          issuer: OIDC_ISSUER,
          authorization_endpoint: `${OIDC_ISSUER}/auth`,
          token_endpoint: `${OIDC_ISSUER}/token`,
          jwks_uri: `${OIDC_ISSUER}/keys`,
        }),
      );
      return;
    }
    if (req.method === 'GET' && url.pathname === '/keys') {
      res.writeHead(200, { 'Content-Type': 'application/json' });
      res.end(JSON.stringify({ keys: [publicJwk] }));
      return;
    }
    if (req.method === 'GET' && url.pathname === '/auth') {
      const redirectUri =
        url.searchParams.get('redirect_uri') || `${SERVER_URL}/api/v1/auth/callback`;
      const state = url.searchParams.get('state') || '';
      const prompt = url.searchParams.get('prompt') || '';
      const nonce = url.searchParams.get('nonce') || '';
      const ssoMatch = /(?:^|;\s*)mock_idp_sso=([^;]+)/.exec(req.headers.cookie || '');
      const ssoUser = ssoMatch ? decodeURIComponent(ssoMatch[1]) : '';
      if (ssoUser && MOCK_USERS[ssoUser] && prompt !== 'login') {
        const target = new URL(redirectUri);
        target.searchParams.set('code', encodeCode(ssoUser, nonce));
        target.searchParams.set('state', state);
        res.writeHead(302, { Location: target.toString() });
        res.end();
        return;
      }
      res.writeHead(200, { 'Content-Type': 'text/html' });
      res.end(`<!doctype html>
<html>
  <body>
    <h1>Dex Login</h1>
    <form method="POST" action="/auth">
      <input type="hidden" name="redirect_uri" value="${escapeHtml(redirectUri)}" />
      <input type="hidden" name="state" value="${escapeHtml(state)}" />
      <input type="hidden" name="nonce" value="${escapeHtml(nonce)}" />
      <label>Username <input name="username" type="text" required /></label>
      <label>Password <input name="password" type="password" required /></label>
      <button type="submit">Sign In</button>
    </form>
  </body>
</html>`);
      return;
    }
    if (req.method === 'POST' && url.pathname === '/auth') {
      let body = '';
      req.on('data', (chunk) => {
        body += chunk.toString();
      });
      req.on('end', () => {
        const params = new URLSearchParams(body);
        const redirectUri = params.get('redirect_uri') || `${SERVER_URL}/api/v1/auth/callback`;
        const state = params.get('state') || '';
        const nonce = params.get('nonce') || '';
        const username = (params.get('username') || '').trim().toLowerCase();
        if (!MOCK_USERS[username]) {
          res.writeHead(401, { 'Content-Type': 'text/html' });
          res.end('<!doctype html><html><body><h1>Invalid credentials</h1></body></html>');
          return;
        }
        const target = new URL(redirectUri);
        target.searchParams.set('code', encodeCode(username, nonce));
        target.searchParams.set('state', state);
        res.writeHead(302, {
          Location: target.toString(),
          'Set-Cookie': `mock_idp_sso=${encodeURIComponent(username)}; Path=/; HttpOnly`,
        });
        res.end();
      });
      return;
    }
    if (req.method === 'POST' && url.pathname === '/token') {
      let body = '';
      req.on('data', (chunk) => {
        body += chunk.toString();
      });
      req.on('end', () => issueToken(res, new URLSearchParams(body).get('code') || ''));
      return;
    }
    res.writeHead(404);
    res.end('Not found');
  });

  function encodeCode(username, nonce) {
    return Buffer.from(JSON.stringify({ u: username, n: nonce })).toString('base64url');
  }

  function issueToken(res, code) {
    let decoded;
    try {
      decoded = JSON.parse(Buffer.from(code, 'base64url').toString('utf8'));
    } catch {
      decoded = null;
    }
    const profile = decoded && MOCK_USERS[decoded.u];
    if (!profile) {
      res.writeHead(400, { 'Content-Type': 'application/json' });
      res.end(JSON.stringify({ error: 'invalid_grant' }));
      return;
    }
    const now = Math.floor(Date.now() / 1000);
    const header = b64url({ alg: 'RS256', typ: 'JWT', kid: 'e2e-key-1' });
    const payload = b64url({
      iss: OIDC_ISSUER,
      aud: 'keel-server',
      iat: now,
      exp: now + 3600,
      sub: decoded.u,
      email: decoded.u,
      name: profile.name,
      groups: profile.groups,
      ...(decoded.n ? { nonce: decoded.n } : {}),
    });
    const signingInput = `${header}.${payload}`;
    const signature = crypto
      .sign('RSA-SHA256', Buffer.from(signingInput), privateKey)
      .toString('base64url');
    const signedJwt = `${signingInput}.${signature}`;
    res.writeHead(200, { 'Content-Type': 'application/json' });
    res.end(
      JSON.stringify({
        id_token: signedJwt,
        access_token: signedJwt,
        token_type: 'Bearer',
      }),
    );
  }

  return new Promise((resolve) => {
    server.listen(OIDC_PORT, '127.0.0.1', () => resolve(server));
  });
}

async function isServerRunning(urlStr) {
  return new Promise((resolve) => {
    const healthUrl = new URL('/healthz', urlStr).toString();
    const req = http.get(healthUrl, (res) => {
      resolve(res.statusCode === 200);
    });
    req.on('error', () => resolve(false));
    req.setTimeout(1000, () => {
      req.destroy();
      resolve(false);
    });
  });
}

async function waitForServer(urlStr, maxAttempts = 30) {
  for (let i = 0; i < maxAttempts; i++) {
    const ok = await isServerRunning(urlStr);
    if (ok) return true;
    await new Promise((r) => setTimeout(r, 500));
  }
  return false;
}

function findCargoBinary() {
  const homeCargo = path.join(os.homedir(), '.cargo', 'bin', 'cargo');
  if (fs.existsSync(homeCargo)) return homeCargo;
  return 'cargo';
}

function findKeelBinary() {
  const rootDir = path.resolve(__dirname, '../..');
  const binPath = path.join(rootDir, 'target', 'debug', 'keel');
  if (fs.existsSync(binPath)) {
    return binPath;
  }
  console.log('Building keel binary via cargo...');
  const cargoCmd = findCargoBinary();
  const buildResult = spawnSync(cargoCmd, ['build'], {
    cwd: rootDir,
    stdio: 'inherit',
    shell: false,
  });
  if (buildResult.status !== 0) {
    throw new Error(`cargo build failed with status ${buildResult.status}`);
  }
  if (!fs.existsSync(binPath)) {
    throw new Error(`keel binary not found at ${binPath} after cargo build`);
  }
  return binPath;
}

async function main() {
  if (await isServerRunning(SERVER_URL)) {
    console.log(`Keel server is already running at ${SERVER_URL}`);
    return;
  }

  const tmpDir = fs.mkdtempSync(path.join(os.tmpdir(), 'keel-e2e-'));
  const bareRepo = path.join(tmpDir, 'remote.git');
  const workDir = path.join(tmpDir, 'workdir');

  fs.mkdirSync(bareRepo, { recursive: true });
  fs.mkdirSync(workDir, { recursive: true });

  console.log(`Initializing bare seed Git repo at ${bareRepo}...`);
  spawnSync('git', ['init', '--bare', bareRepo], { check: true });
  spawnSync('git', ['clone', bareRepo, workDir], { check: true });

  const keelDir = path.join(workDir, '.keel');
  const providersDir = path.join(workDir, 'providers');
  const kindCatalogDir = path.join(workDir, 'catalog', 'infra', 'kind-cluster');
  const podinfoCatalogDir = path.join(workDir, 'catalog', 'apps', 'podinfo');
  const s3CatalogDir = path.join(workDir, 'catalog', 'core', 's3-state-backend');
  const staticSiteCatalogDir = path.join(workDir, 'catalog', 'apps', 'static-site');
  const rauthyCatalogDir = path.join(workDir, 'catalog', 'core', 'rauthy');
  const certManagerCatalogDir = path.join(workDir, 'catalog', 'apps', 'cert-manager');
  // Self-service Helm twins of management-plane base add-ons (built-in catalog entries).
  const selfServiceHelmApps = ['forgejo', 'seaweedfs'];
  const coreAddonsDir = path.join(workDir, 'clusters', 'management', 'core-addons');
  const tenantAcmeInfra = path.join(workDir, 'tenants', 'acme', 'infra', 'net');

  fs.mkdirSync(keelDir, { recursive: true });
  fs.mkdirSync(providersDir, { recursive: true });
  fs.mkdirSync(kindCatalogDir, { recursive: true });
  fs.mkdirSync(podinfoCatalogDir, { recursive: true });
  fs.mkdirSync(s3CatalogDir, { recursive: true });
  fs.mkdirSync(staticSiteCatalogDir, { recursive: true });
  fs.mkdirSync(rauthyCatalogDir, { recursive: true });
  fs.mkdirSync(certManagerCatalogDir, { recursive: true });
  fs.mkdirSync(coreAddonsDir, { recursive: true });
  fs.mkdirSync(tenantAcmeInfra, { recursive: true });

  fs.writeFileSync(
    path.join(keelDir, 'config.yaml'),
    `apiVersion: keel.io/v1alpha1
kind: KeelConfig
clusterName: kind-keel-test
bootstrapTarget: kind
authPhase: oidc-active
enabledProviders:
  - proxmox
  - aws
  - kind
forge:
  provider: local
  repoUrl: "http://127.0.0.1:18080/repo.git"
oidc:
  enabled: true
  issuerUrl: "${OIDC_ISSUER}"
  clientId: "keel-server"
  redirectUrl: "${SERVER_URL}/api/v1/auth/callback"
`,
  );

  fs.writeFileSync(
    path.join(providersDir, 'registry.yaml'),
    `apiVersion: keel.io/v1alpha1
kind: ProviderRegistry
providers:
  - id: proxmox
    name: Proxmox VE
    enabled: true
    version: "0.71.0"
    plugin: bpg/proxmox
    terraformBridge: true
  - id: aws
    name: AWS
    enabled: true
    version: "6.68.0"
    plugin: pulumi/aws
    terraformBridge: false
  - id: kind
    name: Local kind
    enabled: true
    version: "0.20.0"
    plugin: pulumi/kubernetes
    terraformBridge: false
`,
  );

  fs.writeFileSync(
    path.join(kindCatalogDir, 'catalog.yaml'),
    `apiVersion: keel.io/v1alpha1
kind: CatalogInfraBlueprint
metadata:
  id: kind-cluster
  name: Local kind Cluster
  category: kubernetes
  provider: kind
spec:
  pulumiRuntime: yaml
  entrypoint: Pulumi.yaml
  schema:
    type: object
    required:
      - nodeCount
    properties:
      nodeCount:
        type: integer
        minimum: 1
        default: 1
`,
  );

  fs.writeFileSync(
    path.join(podinfoCatalogDir, 'catalog.yaml'),
    `apiVersion: keel.io/v1alpha1
kind: CatalogApplication
metadata:
  id: podinfo
  name: Podinfo App
  description: Microservice application for testing
  category: apps
  runtime: helm
spec:
  upstream:
    repoURL: https://stefanprodan.github.io/podinfo
    chart: podinfo
    version: "6.5.0"
  progressiveDelivery:
    flaggerSupported: true
    defaultCanary: false
  schema:
    type: object
    required:
      - replicas
    properties:
      replicas:
        type: integer
        title: Replica Count
        minimum: 1
        maximum: 10
        default: 2
      ingress:
        type: object
        title: Ingress
        properties:
          enabled:
            type: boolean
            title: Enable Ingress
            default: false
          host:
            type: string
            title: Ingress Hostname
            pattern: "^[a-z0-9.-]+$"
      backendUrl:
        type: string
        title: Backend Service URL
      adminPassword:
        type: string
        title: Admin Secret Password
        format: password
        writeOnly: true
`,
  );

  fs.writeFileSync(
    path.join(staticSiteCatalogDir, 'catalog.yaml'),
    `apiVersion: keel.io/v1alpha1
kind: CatalogApplication
metadata:
  id: static-site
  name: Static Site
  description: Stateless nginx site without progressive delivery support
  category: apps
  runtime: helm
spec:
  upstream:
    repoURL: https://charts.bitnami.com/bitnami
    chart: nginx
    version: "18.2.0"
  progressiveDelivery:
    flaggerSupported: false
    defaultCanary: false
  schema:
    type: object
    properties:
      replicaCount:
        type: integer
        title: Replicas
        default: 1
`,
  );

  // Built-in cluster-scoped app (`spec.placement.scope: cluster`): remote workload clusters only.
  fs.copyFileSync(
    path.resolve(__dirname, '../../deploy/catalog/apps/cert-manager/catalog.yaml'),
    path.join(certManagerCatalogDir, 'catalog.yaml'),
  );
  for (const id of selfServiceHelmApps) {
    const dir = path.join(workDir, 'catalog', 'apps', id);
    fs.mkdirSync(dir, { recursive: true });
    fs.copyFileSync(
      path.resolve(__dirname, `../../deploy/catalog/apps/${id}/catalog.yaml`),
      path.join(dir, 'catalog.yaml'),
    );
  }

  fs.writeFileSync(
    path.join(rauthyCatalogDir, 'catalog.yaml'),
    `apiVersion: keel.io/v1alpha1
kind: CatalogApplication
metadata:
  id: rauthy
  name: Rauthy OIDC Provider
  category: management-core
  runtime: kustomize
spec:
  upstream:
    image: ghcr.io/sebadob/rauthy
    version: "0.37.2"
  progressiveDelivery:
    flaggerSupported: false
    defaultCanary: false
  serviceUrl: "https://idp.keel.local:8888/auth/v1/admin"
  schema:
    type: object
    properties: {}
    additionalProperties: false
`,
  );

  // Image-based plain-manifest add-on: one Flux Kustomization whose version is spec.images[].newTag.
  // Seeded at 0.37.1 and bumped to 0.37.2 in a second commit so the GUI rollback has a target.
  const rauthyAddonPath = path.join(coreAddonsDir, 'rauthy.yaml');
  const rauthyAddon = (tag) =>
    `apiVersion: kustomize.toolkit.fluxcd.io/v1
kind: Kustomization
metadata:
  name: rauthy
  namespace: flux-system
  labels:
    keel.io/protect: "true"
spec:
  interval: 10m
  path: ./clusters/management/addons/rauthy
  targetNamespace: keel-system
  prune: false
  serviceAccountName: kustomize-controller
  sourceRef:
    kind: GitRepository
    name: flux-system
  images:
    - name: ghcr.io/sebadob/rauthy
      newTag: "${tag}"
`;
  fs.writeFileSync(rauthyAddonPath, rauthyAddon('0.37.1'));

  fs.writeFileSync(
    path.join(s3CatalogDir, 'catalog.yaml'),
    `apiVersion: keel.io/v1alpha1
kind: CatalogApplication
metadata:
  id: s3-state-backend
  name: SeaweedFS S3 State Backend
  category: management-core
  runtime: helm
spec:
  upstream:
    repoURL: https://seaweedfs.github.io/seaweedfs/helm
    chart: seaweedfs
    version: "4.53.0"
  progressiveDelivery:
    flaggerSupported: false
    defaultCanary: false
  # Only resource sizing / scheduling is mutable for s3-state-backend through the management
  # API allowlist (keel_core::management_addon_mutable_values); S3 auth/endpoints are locked.
  schema:
    type: object
    properties:
      master:
        type: object
        title: Master Server
        properties:
          resources:
            type: object
            title: Resources
            properties:
              limits:
                type: object
                properties:
                  memory:
                    type: string
                    title: Memory Limit
                    default: 512Mi
`,
  );

  fs.writeFileSync(
    path.join(coreAddonsDir, 's3-seaweedfs.yaml'),
    `apiVersion: helm.toolkit.fluxcd.io/v2
kind: HelmRelease
metadata:
  name: s3-state-backend
  namespace: flux-system
  labels:
    keel.io/protect: "true"
spec:
  releaseName: s3-state-backend
  targetNamespace: keel-system
  storageNamespace: keel-system
  serviceAccountName: helm-controller
  chart:
    spec:
      chart: seaweedfs
      version: "4.53.0"
`,
  );

  fs.writeFileSync(
    path.join(tenantAcmeInfra, 'stack.yaml'),
    `apiVersion: pulumi.com/v1
kind: Stack
metadata:
  name: acme-net
  namespace: tenant-acme
  labels:
    keel.io/tenant: acme
    keel.io/provider: kind
spec:
  stack: keel/acme/net
  protect: false
`,
  );

  spawnSync('git', ['add', '.'], { cwd: workDir });
  spawnSync(
    'git',
    [
      '-c',
      'user.name=Keel Seed',
      '-c',
      'user.email=seed@keel.local',
      'commit',
      '-m',
      'chore: seed test repository',
    ],
    { cwd: workDir },
  );
  fs.writeFileSync(rauthyAddonPath, rauthyAddon('0.37.2'));
  spawnSync('git', ['add', rauthyAddonPath], { cwd: workDir });
  spawnSync(
    'git',
    [
      '-c',
      'user.name=Keel Seed',
      '-c',
      'user.email=seed@keel.local',
      'commit',
      '-m',
      'chore: bump rauthy to 0.37.2',
    ],
    { cwd: workDir },
  );
  spawnSync('git', ['push', 'origin', 'HEAD:refs/heads/main'], {
    cwd: workDir,
  });

  const oidcServer = await startMockOidcServer();
  const binPath = findKeelBinary();

  const isolatedEnv = { ...process.env, KUBECONFIG: path.join(tmpDir, 'no-kubeconfig') };
  delete isolatedEnv.KUBERNETES_SERVICE_HOST;
  delete isolatedEnv.KUBERNETES_SERVICE_PORT;

  console.log(`Starting keel serve on ${BIND_ADDR}...`);
  const serverProc = spawn(
    binPath,
    ['serve', '--bind', BIND_ADDR, '--repo-dir', workDir, '--bare-repo', bareRepo],
    {
      stdio: ['ignore', 'pipe', 'inherit'],
      // Isolate the mocked suite from any developer kubeconfig / live kind cluster: the server
      // must run in "no cluster connected" mode (synthesized previews, git-derived state).
      env: isolatedEnv,
    },
  );

  serverProc.stdout.on('data', (chunk) => {
    process.stdout.write(chunk.toString());
  });

  process.on('exit', () => {
    oidcServer.close();
    if (serverProc && !serverProc.killed) {
      serverProc.kill();
    }
  });
  // Never leave an orphaned `keel serve` behind without its mock IdP (a reused orphan would
  // break every login in the next run).
  for (const sig of ['SIGINT', 'SIGTERM', 'SIGHUP']) {
    process.on(sig, () => process.exit(0));
  }
  serverProc.on('exit', (code) => {
    console.error(`keel serve exited with code ${code}`);
    process.exit(code ?? 1);
  });

  const ready = await waitForServer(SERVER_URL);
  if (!ready) {
    serverProc.kill();
    throw new Error(`Keel server failed to start at ${SERVER_URL}`);
  }
  fs.writeFileSync(
    HARNESS_STATE_FILE,
    JSON.stringify({ serverUrl: SERVER_URL, oidcIssuer: OIDC_ISSUER, bareRepo, workDir }, null, 2),
  );
  console.log(`Keel test server running at ${SERVER_URL}`);
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
