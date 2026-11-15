import { spawn, spawnSync } from 'node:child_process';
import fs from 'node:fs';
import http from 'node:http';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);

const SERVER_URL = process.env.KEEL_BASE_URL || 'http://127.0.0.1:18080';
const BIND_ADDR = '127.0.0.1:18080';

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
  const tenantAcmeInfra = path.join(workDir, 'tenants', 'acme', 'infra', 'net');

  fs.mkdirSync(keelDir, { recursive: true });
  fs.mkdirSync(providersDir, { recursive: true });
  fs.mkdirSync(kindCatalogDir, { recursive: true });
  fs.mkdirSync(podinfoCatalogDir, { recursive: true });
  fs.mkdirSync(tenantAcmeInfra, { recursive: true });

  fs.writeFileSync(
    path.join(keelDir, 'config.yaml'),
    `apiVersion: keel.io/v1alpha1
kind: KeelConfig
clusterName: kind-keel-test
bootstrapTarget: kind
authPhase: bootstrap
enabledProviders:
  - proxmox
  - aws
  - kind
forge:
  provider: local
  repoUrl: "http://127.0.0.1:18080/repo.git"
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
        minimum: 1
        default: 2
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
  spawnSync('git', ['push', 'origin', 'HEAD:refs/heads/main'], {
    cwd: workDir,
  });

  const binPath = findKeelBinary();
  const tokenFile = path.join(os.tmpdir(), 'keel-e2e-bootstrap-token.txt');
  try {
    fs.unlinkSync(tokenFile);
  } catch {
    // Ignore if absent
  }

  console.log(`Starting keel serve on ${BIND_ADDR}...`);
  const serverProc = spawn(
    binPath,
    ['serve', '--bind', BIND_ADDR, '--repo-dir', workDir, '--bare-repo', bareRepo],
    { stdio: ['ignore', 'pipe', 'inherit'] },
  );

  serverProc.stdout.on('data', (chunk) => {
    const text = chunk.toString();
    process.stdout.write(text);
    const match = text.match(/Stage 0 Bootstrap Token:\s*([a-fA-F0-9]+)/);
    if (match) {
      fs.writeFileSync(tokenFile, match[1], 'utf8');
    }
  });

  process.on('exit', () => {
    if (serverProc && !serverProc.killed) {
      serverProc.kill();
    }
  });

  const ready = await waitForServer(SERVER_URL);
  if (!ready) {
    serverProc.kill();
    throw new Error(`Keel server failed to start at ${SERVER_URL}`);
  }
  console.log(`Keel test server running at ${SERVER_URL}`);
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
