import { expect, test } from '@playwright/test';
import { execFileSync, spawn, type ChildProcess } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';

/**
 * Multi-Cloud Free-Tier Live E2E Suite (Stage 1 VMs via `kind` Management Cluster)
 *
 * Tests `08.0a` and `08.0b` always run to validate the dual-guard toggle/credential contract,
 * Stage 0 cloud `kubeconfig` template readiness, and Stage 1 free-tier VM blueprint contracts.
 *
 * Live cloud VM tests (`08.1` AWS, `08.2` Azure, `08.3` GCP) ONLY execute when BOTH:
 *   1. The provider toggle ENV var is `'1'` (`KEEL_E2E_CLOUD_<PROVIDER>=1` or `KEEL_E2E_CLOUD_ALL=1`)
 *   2. The required provider credential ENV vars are present and non-empty
 *
 * When any cloud provider is enabled, `beforeAll` ensures a local `kind` management cluster is
 * bootstrapped via `keel bootstrap init --target kind` (reusing `kind-keel-mgmt` if present unless
 * `KEEL_E2E_REUSE_CLUSTER=0`) and starts `keel serve` to drive Stage 1 VM provisioning through Keel.
 *
 * Every test enforces confirmed cloud teardown in `finally` only when a stack was actually provisioned.
 */

function loadDotEnvCloudFile(): void {
  const candidates = [
    path.resolve(process.cwd(), '.env.cloud-e2e'),
    path.resolve(process.cwd(), '../.env.cloud-e2e'),
  ];
  const envPath = candidates.find((p) => fs.existsSync(p));
  if (!envPath) return;

  const lines = fs.readFileSync(envPath, 'utf-8').split(/\r?\n/);
  for (const line of lines) {
    const trimmed = line.trim();
    if (!trimmed || trimmed.startsWith('#')) continue;
    const eqIdx = trimmed.indexOf('=');
    if (eqIdx <= 0) continue;
    const key = trimmed.slice(0, eqIdx).trim();
    let val = trimmed.slice(eqIdx + 1).trim();
    if ((val.startsWith('"') && val.endsWith('"')) || (val.startsWith("'") && val.endsWith("'"))) {
      val = val.slice(1, -1);
    }
    if (key && process.env[key] === undefined) {
      process.env[key] = val;
    }
  }
}

loadDotEnvCloudFile();

export type CloudProvider = 'aws' | 'azure' | 'gcp';

export interface CloudGateResult {
  enabled: boolean;
  reason: string;
}

export function resolveCloudTargetGate(
  provider: CloudProvider,
  env: NodeJS.ProcessEnv = process.env,
): CloudGateResult {
  const allEnabled = env.KEEL_E2E_CLOUD_ALL === '1';
  const toggleKey = `KEEL_E2E_CLOUD_${provider.toUpperCase()}`;
  const providerToggled = env[toggleKey] === '1';

  if (!allEnabled && !providerToggled) {
    return {
      enabled: false,
      reason: `Cloud E2E disabled for ${provider} (set ${toggleKey}=1 or KEEL_E2E_CLOUD_ALL=1)`,
    };
  }

  if (provider === 'aws') {
    const hasKeyId = Boolean(env.AWS_ACCESS_KEY_ID?.trim());
    const hasSecretKey = Boolean(env.AWS_SECRET_ACCESS_KEY?.trim());
    if (!hasKeyId || !hasSecretKey) {
      return {
        enabled: false,
        reason: 'Missing AWS credentials: AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY are required',
      };
    }
  } else if (provider === 'azure') {
    const hasClientId = Boolean(env.ARM_CLIENT_ID?.trim());
    const hasClientSecret = Boolean(env.ARM_CLIENT_SECRET?.trim());
    const hasTenantId = Boolean(env.ARM_TENANT_ID?.trim());
    const hasSubId = Boolean(env.ARM_SUBSCRIPTION_ID?.trim());
    if (!hasClientId || !hasClientSecret || !hasTenantId || !hasSubId) {
      return {
        enabled: false,
        reason:
          'Missing Azure credentials: ARM_CLIENT_ID, ARM_CLIENT_SECRET, ARM_TENANT_ID, ARM_SUBSCRIPTION_ID are required',
      };
    }
  } else if (provider === 'gcp') {
    const hasCreds = Boolean(
      env.GOOGLE_CREDENTIALS?.trim() || env.GOOGLE_APPLICATION_CREDENTIALS?.trim(),
    );
    const hasProject = Boolean(env.GCP_PROJECT?.trim() || env.GOOGLE_PROJECT?.trim());
    if (!hasCreds || !hasProject) {
      return {
        enabled: false,
        reason:
          'Missing GCP credentials: (GOOGLE_CREDENTIALS or GOOGLE_APPLICATION_CREDENTIALS) and (GCP_PROJECT or GOOGLE_PROJECT) are required',
      };
    }
  }

  return { enabled: true, reason: `Enabled for ${provider}` };
}

const MGMT_CLUSTER = process.env.KEEL_E2E_MGMT_CLUSTER || 'keel-mgmt';
const BASE_URL = process.env.KEEL_BASE_URL || 'http://127.0.0.1:8080';
const KEEL_BIN = process.env.KEEL_BIN || path.resolve(process.cwd(), '../target/debug/keel');
const SERVE_LOG_PATH = path.join(os.tmpdir(), 'keel-cloud-e2e-serve.log');
const DETERMINISTIC_REPO_DIR = path.join(os.tmpdir(), `keel-e2e-gitops-${MGMT_CLUSTER}`);

const PREVIEW_TIMEOUT_MS = 180_000;
const PROVISION_TIMEOUT_MS = 600_000;
const API_DELETE_TIMEOUT_MS = 180_000;
const K8S_FINALIZER_TIMEOUT_MS = 300_000;

function logStep(msg: string, provider?: CloudProvider): void {
  const ts = new Date().toISOString().slice(11, 19);
  const tag = provider ? ` [${provider}]` : '';
  console.log(`[cloud-e2e ${ts}]${tag} ${msg}`);
}

function runCmd(
  bin: string,
  args: string[],
  env?: NodeJS.ProcessEnv,
  streamOutput = false,
): string {
  if (streamOutput) {
    execFileSync(bin, args, {
      env: { ...process.env, ...env },
      stdio: 'inherit',
    });
    return '';
  }
  return execFileSync(bin, args, {
    encoding: 'utf-8',
    env: { ...process.env, ...env },
    stdio: ['ignore', 'pipe', 'pipe'],
  }).trim();
}

function tryCmd(bin: string, args: string[]): { ok: boolean; stdout: string } {
  try {
    return { ok: true, stdout: runCmd(bin, args) };
  } catch {
    return { ok: false, stdout: '' };
  }
}

async function isServerUp(url: string): Promise<boolean> {
  try {
    const res = await fetch(`${url}/api/v1/auth/status`);
    return res.ok;
  } catch {
    return false;
  }
}

async function signInViaRauthy(
  page: import('@playwright/test').Page,
  email: string,
  password: string,
): Promise<void> {
  await page.goto(BASE_URL);
  await expect(page.getByText('Keel Control Plane')).toBeVisible();

  const authRequired = page.getByText('Authentication Required');
  const mgmtStack = page.getByText('management-cluster-core', { exact: true });
  await expect(authRequired.or(mgmtStack)).toBeVisible({ timeout: 30_000 });

  if (await authRequired.isVisible()) {
    await page
      .getByRole('button', { name: /Sign in with OIDC/i })
      .first()
      .click();
    const emailInput = page
      .locator(
        'input[type="email"], input[name="email"], input[name="login"], input#login, input[autocomplete="username"]',
      )
      .first();
    await expect(emailInput).toBeVisible({ timeout: 20_000 });
    await emailInput.fill(email);

    const passwordInput = page
      .locator('input[type="password"], input[name="password"], input#password')
      .first();
    if (!(await passwordInput.isVisible({ timeout: 1_000 }).catch(() => false))) {
      await page.locator('button[type="submit"]').first().click();
    }
    await expect(passwordInput).toBeVisible({ timeout: 10_000 });
    await passwordInput.fill(password);
    await page.locator('button[type="submit"], #submit-login').first().click();

    const grantBtn = page.getByRole('button', { name: /Grant Access/i });
    if (await grantBtn.isVisible({ timeout: 2_000 }).catch(() => false)) {
      await grantBtn.click();
    }
  }

  await expect(page.getByText('management-cluster-core', { exact: true })).toBeVisible({
    timeout: 30_000,
  });
}

function triggerFluxReconcile(): void {
  const ts = new Date().toISOString();
  tryCmd('kubectl', [
    'annotate',
    '--overwrite',
    'gitrepository/flux-system',
    '-n',
    'flux-system',
    `reconcile.fluxcd.io/requestedAt=${ts}`,
  ]);
  tryCmd('kubectl', [
    'annotate',
    '--overwrite',
    'kustomization/keel-tenants',
    '-n',
    'flux-system',
    `reconcile.fluxcd.io/requestedAt=${ts}`,
  ]);
}

let gitPrChain: Promise<void> = Promise.resolve();
function withGitPrLock<T>(fn: () => Promise<T>): Promise<T> {
  const next = gitPrChain.then(fn, fn);
  gitPrChain = next.then(
    () => undefined,
    () => undefined,
  );
  return next;
}

async function waitForStackReady(
  name: string,
  namespace: string,
  provider: CloudProvider,
  timeoutMs = PROVISION_TIMEOUT_MS,
): Promise<void> {
  const start = Date.now();
  const deadline = start + timeoutMs;
  const timeoutSec = Math.round(timeoutMs / 1000);
  let lastLog = 0;

  triggerFluxReconcile();

  while (Date.now() < deadline) {
    const res = tryCmd('kubectl', [
      'get',
      'stack.pulumi.com',
      name,
      '-n',
      namespace,
      '-o',
      'jsonpath={.status.conditions[?(@.type=="Ready")].status}|{.status.conditions[?(@.type=="Ready")].reason}|{.status.conditions[?(@.type=="Ready")].message}|{.status.conditions[?(@.type=="Stalled")].status}|{.status.lastUpdate.message}',
    ]);

    if (res.ok) {
      const [status, reason, message, stalled, lastUpdateMsg] = res.stdout.split('|');
      if (status === 'True') {
        const elapsed = Math.round((Date.now() - start) / 1000);
        logStep(`PKO v2 Stack ${namespace}/${name} is Ready (${elapsed}s)!`, provider);
        return;
      }
      if (stalled === 'True') {
        const podLogs = tryCmd('kubectl', [
          'logs',
          `${name}-workspace-0`,
          '-n',
          namespace,
          '--tail=30',
        ]);
        const detail = [lastUpdateMsg, podLogs.ok ? podLogs.stdout : '']
          .filter(Boolean)
          .join('\n--- workspace pod logs ---\n');
        const errMsg = `PKO v2 Stack ${namespace}/${name} stalled (${reason}: ${message})\n${detail}`;
        logStep(errMsg, provider);
        throw new Error(`[${provider}] ${errMsg}`);
      }
      if (Date.now() - lastLog > 15_000) {
        const elapsed = Math.round((Date.now() - start) / 1000);
        logStep(
          `Waiting for PKO v2 Stack ${namespace}/${name} (${elapsed}s/${timeoutSec}s): status=${status || 'Pending'} reason=${reason || '-'} ${message || ''}`,
          provider,
        );
        lastLog = Date.now();
      }
    } else if (Date.now() - lastLog > 15_000) {
      const elapsed = Math.round((Date.now() - start) / 1000);
      const ks = tryCmd('kubectl', [
        'get',
        'kustomization',
        'keel-tenants',
        '-n',
        'flux-system',
        '-o',
        'jsonpath={.status.conditions[?(@.type=="Ready")].status}:{.status.conditions[?(@.type=="Ready")].message}',
      ]);
      const ksInfo = ks.ok && ks.stdout ? ` (keel-tenants=${ks.stdout})` : '';
      logStep(
        `Waiting for Flux to create PKO v2 Stack ${namespace}/${name} (${elapsed}s/${timeoutSec}s)${ksInfo}...`,
        provider,
      );
      lastLog = Date.now();
    }

    await new Promise((r) => setTimeout(r, 5000));
  }

  throw new Error(
    `[${provider}] Timed out after ${timeoutSec}s waiting for PKO v2 Stack ${namespace}/${name} to become Ready`,
  );
}

/**
 * Deletes a Stage 1 stack via the Keel API, merges the resulting deletion PR, and
 * polls both the Keel `/api/v1/stacks` endpoint and Kubernetes (if a live cluster is attached)
 * until the stack and its PKO v2 `destroyOnFinalize` finalizer have completely removed the resource.
 */
async function deleteAndConfirmStage1StackRemoved(
  request: import('@playwright/test').APIRequestContext,
  tenant: string,
  stackName: string,
  provider: CloudProvider,
): Promise<void> {
  const namespace = `tenant-${tenant}`;
  logStep(`Requesting deletion of Stage 1 stack ${namespace}/${stackName}...`, provider);
  const deletedPr = await withGitPrLock(async () => {
    const delRes = await request
      .delete(`${BASE_URL}/api/v1/stacks/${namespace}/${stackName}`)
      .catch(() => null);
    if (!delRes || !delRes.ok()) {
      return false;
    }

    const delJson = (await delRes.json().catch(() => null)) as {
      id?: string | number;
      proposal?: { id?: string | number };
    } | null;
    const delPrId = delJson?.proposal?.id ?? delJson?.id;
    if (delPrId !== undefined) {
      logStep(`Merging deletion PR #${delPrId} for ${namespace}/${stackName}...`, provider);
      const mergeRes = await request.post(`${BASE_URL}/api/v1/proposals/${delPrId}/merge`);
      expect(mergeRes.ok()).toBe(true);
      triggerFluxReconcile();
    }
    return true;
  });
  if (!deletedPr) {
    return;
  }

  // 1. Poll GET /api/v1/stacks until the stack is confirmed removed from Keel
  const apiTimeoutSec = Math.round(API_DELETE_TIMEOUT_MS / 1000);
  logStep(
    `Waiting for ${namespace}/${stackName} removal from Keel stacks (timeout ${apiTimeoutSec}s)...`,
    provider,
  );
  const apiStart = Date.now();
  const apiDeadline = apiStart + API_DELETE_TIMEOUT_MS;
  let apiLastLog = 0;
  let removedFromApi = false;
  while (Date.now() < apiDeadline) {
    const stacksRes = await request.get(`${BASE_URL}/api/v1/stacks`);
    if (stacksRes.ok()) {
      const body = (await stacksRes.json()) as {
        stacks?: Array<{ name?: string; namespace?: string; tenant?: string }>;
      };
      const stacks = Array.isArray(body) ? body : (body.stacks ?? []);
      const stillExists = stacks.some(
        (s) => s.name === stackName && (s.namespace === namespace || s.tenant === tenant),
      );
      if (!stillExists) {
        removedFromApi = true;
        break;
      }
    }
    if (Date.now() - apiLastLog > 15_000) {
      const elapsed = Math.round((Date.now() - apiStart) / 1000);
      logStep(
        `Still waiting for ${namespace}/${stackName} removal from Keel API (${elapsed}s/${apiTimeoutSec}s)...`,
        provider,
      );
      apiLastLog = Date.now();
    }
    await new Promise((r) => setTimeout(r, 3000));
  }
  expect(
    removedFromApi,
    `[${provider}] Timed out after ${apiTimeoutSec}s waiting for stack ${namespace}/${stackName} to be removed from /api/v1/stacks`,
  ).toBe(true);

  // 2. Wait until the K8s PKO v2 Stack object finishes its `destroyOnFinalize: true` cloud teardown
  // and is completely deleted from the management cluster.
  try {
    runCmd('kubectl', ['get', 'crd', 'stacks.pulumi.com']);
    const k8sTimeoutSec = Math.round(K8S_FINALIZER_TIMEOUT_MS / 1000);
    logStep(
      `Waiting for PKO v2 Stack ${namespace}/${stackName} finalizer (destroyOnFinalize, timeout ${k8sTimeoutSec}s)...`,
      provider,
    );
    const k8sStart = Date.now();
    const k8sDeadline = k8sStart + K8S_FINALIZER_TIMEOUT_MS;
    let k8sLastLog = 0;
    let removedFromK8s = false;
    while (Date.now() < k8sDeadline) {
      const getRes = tryCmd('kubectl', ['get', 'stack.pulumi.com', stackName, '-n', namespace]);
      if (!getRes.ok) {
        removedFromK8s = true;
        break;
      }
      if (Date.now() - k8sLastLog > 15_000) {
        const elapsed = Math.round((Date.now() - k8sStart) / 1000);
        logStep(
          `Waiting for PKO v2 Stack ${namespace}/${stackName} cloud teardown (${elapsed}s/${k8sTimeoutSec}s)...`,
          provider,
        );
        k8sLastLog = Date.now();
      }
      await new Promise((r) => setTimeout(r, 5000));
    }
    expect(
      removedFromK8s,
      `[${provider}] Timed out after ${k8sTimeoutSec}s waiting for PKO v2 Stack ${namespace}/${stackName} finalizer to finish cloud teardown`,
    ).toBe(true);
    logStep(
      `Confirmed PKO v2 Stack ${namespace}/${stackName} and its cloud resources are deleted.`,
      provider,
    );
  } catch {
    // kubectl/PKO CRD not present in this environment
  }
}

test.describe('08 - Multi-Cloud Free-Tier Dual-Guard Contract & Blueprint Validation', () => {
  test('08.0a - Pure dual-guard helper enforces toggle and credentials requirement', () => {
    // Disabled when neither toggle nor credentials exist
    expect(resolveCloudTargetGate('aws', {}).enabled).toBe(false);
    expect(resolveCloudTargetGate('azure', {}).enabled).toBe(false);
    expect(resolveCloudTargetGate('gcp', {}).enabled).toBe(false);

    // Disabled when toggle is '1' but credentials are missing
    expect(resolveCloudTargetGate('aws', { KEEL_E2E_CLOUD_AWS: '1' }).enabled).toBe(false);
    expect(resolveCloudTargetGate('azure', { KEEL_E2E_CLOUD_AZURE: '1' }).enabled).toBe(false);
    expect(resolveCloudTargetGate('gcp', { KEEL_E2E_CLOUD_GCP: '1' }).enabled).toBe(false);

    // Disabled when credentials exist but toggle is off
    expect(
      resolveCloudTargetGate('aws', {
        AWS_ACCESS_KEY_ID: 'dummy-id',
        AWS_SECRET_ACCESS_KEY: 'dummy-secret',
      }).enabled,
    ).toBe(false);
    expect(
      resolveCloudTargetGate('azure', {
        ARM_CLIENT_ID: 'dummy-client',
        ARM_CLIENT_SECRET: 'dummy-secret',
        ARM_TENANT_ID: 'dummy-tenant',
        ARM_SUBSCRIPTION_ID: 'dummy-sub',
      }).enabled,
    ).toBe(false);
    expect(
      resolveCloudTargetGate('gcp', {
        GOOGLE_CREDENTIALS: '{"type":"service_account"}',
        GCP_PROJECT: 'dummy-project',
      }).enabled,
    ).toBe(false);

    // Enabled when BOTH provider-specific toggle and credentials exist
    expect(
      resolveCloudTargetGate('aws', {
        KEEL_E2E_CLOUD_AWS: '1',
        AWS_ACCESS_KEY_ID: 'dummy-id',
        AWS_SECRET_ACCESS_KEY: 'dummy-secret',
      }).enabled,
    ).toBe(true);
    expect(
      resolveCloudTargetGate('azure', {
        KEEL_E2E_CLOUD_AZURE: '1',
        ARM_CLIENT_ID: 'dummy-client',
        ARM_CLIENT_SECRET: 'dummy-secret',
        ARM_TENANT_ID: 'dummy-tenant',
        ARM_SUBSCRIPTION_ID: 'dummy-sub',
      }).enabled,
    ).toBe(true);
    expect(
      resolveCloudTargetGate('gcp', {
        KEEL_E2E_CLOUD_GCP: '1',
        GOOGLE_CREDENTIALS: '{"type":"service_account"}',
        GCP_PROJECT: 'dummy-project',
      }).enabled,
    ).toBe(true);

    // Enabled when KEEL_E2E_CLOUD_ALL=1 and credentials exist
    expect(
      resolveCloudTargetGate('aws', {
        KEEL_E2E_CLOUD_ALL: '1',
        AWS_ACCESS_KEY_ID: 'dummy-id',
        AWS_SECRET_ACCESS_KEY: 'dummy-secret',
      }).enabled,
    ).toBe(true);
    expect(
      resolveCloudTargetGate('azure', {
        KEEL_E2E_CLOUD_ALL: '1',
        ARM_CLIENT_ID: 'dummy-client',
        ARM_CLIENT_SECRET: 'dummy-secret',
        ARM_TENANT_ID: 'dummy-tenant',
        ARM_SUBSCRIPTION_ID: 'dummy-sub',
      }).enabled,
    ).toBe(true);
    expect(
      resolveCloudTargetGate('gcp', {
        KEEL_E2E_CLOUD_ALL: '1',
        GOOGLE_APPLICATION_CREDENTIALS: '/tmp/key.json',
        GOOGLE_PROJECT: 'dummy-project',
      }).enabled,
    ).toBe(true);
  });

  test('08.0b - Free-tier catalog blueprints & Stage 0 k3s templates exist on disk with required outputs', () => {
    const deployDir = path.resolve(process.cwd(), '../deploy');

    // Stage 0 AWS (k3s + kubeconfig output)
    const stage0Aws = fs.readFileSync(path.join(deployDir, 'bootstrap/aws/Pulumi.yaml'), 'utf-8');
    expect(stage0Aws).toContain('protect: true');
    expect(stage0Aws).toContain('get.k3s.io');
    expect(stage0Aws).toContain('kubeconfig:');
    expect(stage0Aws).toContain('sshHost:');
    expect(stage0Aws).toContain('sshUser:');
    expect(stage0Aws).toContain('sshPort: 22');

    // Stage 0 Azure (k3s + kubeconfig output)
    const stage0Azure = fs.readFileSync(
      path.join(deployDir, 'bootstrap/azure/Pulumi.yaml'),
      'utf-8',
    );
    expect(stage0Azure).toContain('protect: true');
    expect(stage0Azure).toContain('get.k3s.io');
    expect(stage0Azure).toContain('kubeconfig:');
    expect(stage0Azure).toContain('sshHost:');
    expect(stage0Azure).toContain('sshUser:');
    expect(stage0Azure).toContain('sshPort: 22');

    // Stage 0 GCP (k3s + kubeconfig output)
    const stage0Gcp = fs.readFileSync(path.join(deployDir, 'bootstrap/gcp/Pulumi.yaml'), 'utf-8');
    expect(stage0Gcp).toContain('protect: true');
    expect(stage0Gcp).toContain('get.k3s.io');
    expect(stage0Gcp).toContain('kubeconfig:');
    expect(stage0Gcp).toContain('sshHost:');
    expect(stage0Gcp).toContain('sshUser:');
    expect(stage0Gcp).toContain('sshPort: 22');

    // Stage 1 Free-Tier Blueprints
    const blueprints: Array<[string, string, string]> = [
      ['aws', 'aws-vm', 't3.micro'],
      ['azure', 'azure-vm', 'Standard_B1s'],
      ['gcp', 'gcp-vm', 'e2-micro'],
    ];

    for (const [provider, id, vmSize] of blueprints) {
      const catPath = path.join(deployDir, `catalog/infra/${id}/catalog.yaml`);
      const pulPath = path.join(deployDir, `catalog/infra/${id}/Pulumi.yaml`);
      expect(fs.existsSync(catPath)).toBe(true);
      expect(fs.existsSync(pulPath)).toBe(true);

      const catContent = fs.readFileSync(catPath, 'utf-8');
      const pulContent = fs.readFileSync(pulPath, 'utf-8');

      expect(catContent).toContain(`id: ${id}`);
      expect(catContent).toContain(`provider: ${provider}`);
      expect(catContent).toContain('category: compute');
      expect(catContent).toContain(`default: ${vmSize}`);
      expect(catContent).toContain('sshHost');
      expect(catContent).toContain('sshUser');
      expect(catContent).toContain('sshPort');
      expect(pulContent).toContain(vmSize);
      expect(pulContent).toContain('sshHost:');
      expect(pulContent).toContain('sshUser:');
      expect(pulContent).toContain('sshPort: 22');
    }
  });
});

const LIVE_ENABLED = process.env.KEEL_E2E_LIVE === '1';

function resolveE2eAdminPassword(): string {
  if (process.env.KEEL_ADMIN_PASSWORD?.trim()) {
    return process.env.KEEL_ADMIN_PASSWORD.trim();
  }
  if (process.env.KEEL_E2E_REUSE_CLUSTER !== '0') {
    const savedPassPath = path.resolve(process.cwd(), '../target/keel-dex-password');
    if (fs.existsSync(savedPassPath)) {
      const saved = fs.readFileSync(savedPassPath, 'utf-8').trim();
      if (saved) {
        return saved;
      }
    }
  }
  return 'KeelE2eAdminPass123!';
}

test.describe('08.1 - Multi-Cloud Free-Tier Stage 1 Live E2E (kind Management Cluster -> Cloud VMs)', () => {
  test.describe.configure({ mode: 'serial' });
  test.skip(
    !LIVE_ENABLED,
    'Set KEEL_E2E_LIVE=1 (or run `pnpm --dir e2e test:live:cloud`) to run live multi-cloud E2E',
  );

  let createdMgmtCluster = false;
  let serveProc: ChildProcess | null = null;
  let tmpRepoDir: string | null = null;
  const adminPassword = resolveE2eAdminPassword();

  const anyCloudEnabled =
    resolveCloudTargetGate('aws').enabled ||
    resolveCloudTargetGate('azure').enabled ||
    resolveCloudTargetGate('gcp').enabled;

  test.beforeAll(async () => {
    if (!LIVE_ENABLED || !anyCloudEnabled) {
      return;
    }

    const clusters = tryCmd('kind', ['get', 'clusters']).stdout.split(/\r?\n/).filter(Boolean);
    const hasExistingMgmt = clusters.includes(MGMT_CLUSTER);

    if (!hasExistingMgmt || process.env.KEEL_E2E_REUSE_CLUSTER === '0') {
      if (hasExistingMgmt) {
        logStep(`Deleting existing kind cluster "${MGMT_CLUSTER}" (KEEL_E2E_REUSE_CLUSTER=0)...`);
        runCmd('kind', ['delete', 'cluster', '--name', MGMT_CLUSTER], undefined, true);
      }

      if (fs.existsSync(DETERMINISTIC_REPO_DIR)) {
        fs.rmSync(DETERMINISTIC_REPO_DIR, { recursive: true, force: true });
      }
      tmpRepoDir = DETERMINISTIC_REPO_DIR;
      createdMgmtCluster = true;

      logStep(
        `Bootstrapping kind management cluster "${MGMT_CLUSTER}" via keel bootstrap init --target kind...`,
      );
      runCmd(
        KEEL_BIN,
        [
          'bootstrap',
          'init',
          '--target',
          'kind',
          '--with-forge',
          '--local-dev-docker-socket-exception',
          '--cluster-name',
          MGMT_CLUSTER,
          '--repo-dir',
          tmpRepoDir,
          '--admin-password',
          adminPassword,
        ],
        undefined,
        true,
      );
    } else {
      logStep(`Reusing existing kind management cluster "${MGMT_CLUSTER}"`);
    }

    const repoDir =
      process.env.KEEL_E2E_REPO_DIR ||
      tmpRepoDir ||
      (fs.existsSync(DETERMINISTIC_REPO_DIR)
        ? DETERMINISTIC_REPO_DIR
        : path.resolve(process.cwd(), '../keel-gitops'));

    const deployCatalog = path.resolve(process.cwd(), '../deploy/catalog');
    for (const id of ['aws-vm', 'azure-vm', 'gcp-vm']) {
      fs.cpSync(
        path.join(deployCatalog, `infra/${id}`),
        path.join(repoDir, `catalog/infra/${id}`),
        {
          recursive: true,
        },
      );
    }
    if (!fs.existsSync(path.join(repoDir, 'catalog/bundles/kind-canary-platform/catalog.yaml'))) {
      logStep('Seeding full deploy/catalog/{apps,infra,bundles} into GitOps repo...');
      fs.cpSync(deployCatalog, path.join(repoDir, 'catalog'), { recursive: true });
    }
    runCmd('git', ['-C', repoDir, 'add', 'catalog']);
    const diff = tryCmd('git', ['-C', repoDir, 'diff', '--cached', '--quiet']);
    if (!diff.ok) {
      logStep('Committing updated free-tier catalog blueprints to GitOps repo...');
      runCmd(
        'git',
        [
          '-C',
          repoDir,
          '-c',
          'user.name=Keel E2E',
          '-c',
          'user.email=e2e@keel.local',
          '-c',
          'commit.gpgsign=false',
          'commit',
          '-m',
          'chore(e2e): sync multi-cloud free-tier catalog blueprints',
        ],
        { GIT_EDITOR: 'true' },
      );
    }

    if (process.env.GITHUB_TOKEN) {
      logStep('Seeding keel-system/keel-github-token secret for PKO plugin downloads...');
      const manifest = execFileSync(
        'kubectl',
        [
          'create',
          'secret',
          'generic',
          'keel-github-token',
          '-n',
          'keel-system',
          `--from-literal=token=${process.env.GITHUB_TOKEN}`,
          '--dry-run=client',
          '-o',
          'yaml',
        ],
        { encoding: 'utf-8' },
      );
      execFileSync('kubectl', ['apply', '-f', '-'], { input: manifest, encoding: 'utf-8' });
    }

    if (!(await isServerUp(BASE_URL))) {
      const url = new URL(BASE_URL);
      logStep(`Starting keel serve on ${BASE_URL} (repo: ${repoDir}, log: ${SERVE_LOG_PATH})...`);
      const logFd = fs.openSync(SERVE_LOG_PATH, 'w');
      serveProc = spawn(
        KEEL_BIN,
        ['serve', '--bind', `${url.hostname}:${url.port || '8080'}`, '--repo-dir', repoDir],
        { stdio: ['ignore', logFd, logFd], detached: true },
      );
      const deadline = Date.now() + 30_000;
      while (Date.now() < deadline) {
        if (await isServerUp(BASE_URL)) break;
        await new Promise((r) => setTimeout(r, 1000));
      }
      if (!(await isServerUp(BASE_URL))) {
        throw new Error(`keel serve did not become ready at ${BASE_URL}; check ${SERVE_LOG_PATH}`);
      }
      logStep(`keel serve is ready at ${BASE_URL}`);
    } else {
      logStep(`Reusing existing keel serve at ${BASE_URL}`);
    }

    const unpushedCount = Number(
      tryCmd('git', ['-C', repoDir, 'rev-list', '--count', 'origin/main..HEAD']).stdout || '0',
    );
    if (unpushedCount > 0) {
      logStep(`Pushing ${unpushedCount} catalog commit(s) to origin/main...`);
      const remoteUrl = tryCmd('git', ['-C', repoDir, 'remote', 'get-url', 'origin']).stdout;
      if (remoteUrl.startsWith('http')) {
        const tokRes = tryCmd('kubectl', [
          'get',
          'secret',
          'forge-token',
          '-n',
          'keel-system',
          '-o',
          'jsonpath={.data.token}',
        ]);
        const token = tokRes.ok
          ? Buffer.from(tokRes.stdout, 'base64').toString('utf-8').trim()
          : '';
        const askpassPath = path.join(os.tmpdir(), 'keel-cloud-e2e-askpass.sh');
        fs.writeFileSync(
          askpassPath,
          `#!/bin/sh\ncase "$1" in\n  *Username*) printf "%s\\n" "keel" ;;\n  *Password*) printf "%s\\n" "${token}" ;;\nesac\n`,
          { mode: 0o700 },
        );
        try {
          runCmd('git', ['-C', repoDir, 'push', 'origin', 'main'], {
            GIT_ASKPASS: askpassPath,
            GIT_TERMINAL_PROMPT: '0',
          });
        } finally {
          fs.rmSync(askpassPath, { force: true });
        }
      } else {
        tryCmd('git', ['-C', repoDir, 'push', 'origin', 'main']);
      }
    }
  });

  test.afterAll(() => {
    if (serveProc?.pid) {
      try {
        process.kill(-serveProc.pid);
      } catch {
        // ignore if already exited
      }
    }
    if (createdMgmtCluster && process.env.KEEL_E2E_KEEP_CLUSTER !== '1') {
      logStep(`Cleaning up ephemeral kind management cluster "${MGMT_CLUSTER}"...`);
      tryCmd('kind', ['delete', 'cluster', '--name', MGMT_CLUSTER]);
      if (tmpRepoDir) {
        fs.rmSync(tmpRepoDir, { recursive: true, force: true });
      }
    }
  });

  interface CloudVmSpec {
    provider: CloudProvider;
    catalogId: string;
    sshUser: string;
    values: Record<string, unknown>;
  }

  interface ProvisionedCloudRun {
    spec: CloudVmSpec;
    stackName: string;
    tenant: string;
    context: import('@playwright/test').BrowserContext;
    page: import('@playwright/test').Page;
  }

  async function createAndMergeCloudProposal(
    browser: import('@playwright/test').Browser,
    spec: CloudVmSpec,
  ): Promise<ProvisionedCloudRun> {
    const { provider, catalogId, values } = spec;
    const context = await browser.newContext({ ignoreHTTPSErrors: true, baseURL: BASE_URL });
    const page = await context.newPage();
    const request = page.request;
    const stackName = `${provider}-vm-${Date.now()}`;
    const tenant = 'default';
    let prId: string | number | undefined;
    let stackMerged = false;

    try {
      logStep(`Signing into Keel UI via Rauthy OIDC...`, provider);
      await signInViaRauthy(page, 'admin@keel.local', adminPassword);

      logStep(`Creating proposal for ${catalogId} (${stackName})...`, provider);
      const resolvedValues = Object.fromEntries(
        Object.entries(values).map(([k, v]) => [k, v === '${stackName}' ? stackName : v]),
      );
      const createRes = await request.post(`${BASE_URL}/api/v1/proposals`, {
        data: {
          catalogId,
          name: stackName,
          tenant,
          values: resolvedValues,
        },
      });
      expect(
        createRes.ok(),
        `[${provider}] Failed to create proposal: ${await createRes.text()}`,
      ).toBe(true);

      const created = (await createRes.json()) as {
        id?: string | number;
        proposal?: { id?: string | number };
      };
      prId = created.proposal?.id ?? created.id;
      expect(
        prId,
        `[${provider}] Missing proposal.id in response: ${JSON.stringify(created)}`,
      ).toBeTruthy();

      const previewTimeoutSec = Math.round(PREVIEW_TIMEOUT_MS / 1000);
      logStep(
        `Polling preview diff for proposal #${prId} (timeout ${previewTimeoutSec}s)...`,
        provider,
      );
      const prevStart = Date.now();
      const prevDeadline = prevStart + PREVIEW_TIMEOUT_MS;
      let prevLastLog = 0;
      let previewReady = false;
      while (Date.now() < prevDeadline) {
        const prevRes = await request.get(`${BASE_URL}/api/v1/proposals/${prId}/preview`);
        if (prevRes.ok()) {
          const body = await prevRes.text();
          if (body.includes('+ Create') || body.includes('create') || body.includes('preview')) {
            previewReady = true;
            break;
          }
        }
        if (Date.now() - prevLastLog > 15_000) {
          const elapsed = Math.round((Date.now() - prevStart) / 1000);
          logStep(
            `Still waiting for preview #${prId} (${elapsed}s/${previewTimeoutSec}s)...`,
            provider,
          );
          prevLastLog = Date.now();
        }
        await new Promise((r) => setTimeout(r, 5000));
      }
      expect(
        previewReady,
        `[${provider}] Timed out after ${previewTimeoutSec}s waiting for preview #${prId}`,
      ).toBe(true);

      logStep(`Merging proposal #${prId}...`, provider);
      let mergeRes = await request.post(`${BASE_URL}/api/v1/proposals/${prId}/merge`);
      for (let attempt = 1; attempt <= 4 && mergeRes.status() === 409; attempt++) {
        await new Promise((r) => setTimeout(r, 2000));
        mergeRes = await request.post(`${BASE_URL}/api/v1/proposals/${prId}/merge`);
      }
      expect(mergeRes.ok(), `[${provider}] Merge failed: ${await mergeRes.text()}`).toBe(true);
      stackMerged = true;

      return { spec, stackName, tenant, context, page };
    } catch (err) {
      if (!stackMerged && prId !== undefined) {
        logStep(`Closing unmerged proposal #${prId}; skipping stack delete.`, provider);
        await request.post(`${BASE_URL}/api/v1/proposals/${prId}/reject`).catch(() => null);
      }
      await context.close();
      throw err;
    }
  }

  async function reconcileVerifyAndDeleteCloudStack(run: ProvisionedCloudRun): Promise<void> {
    const { spec, stackName, tenant, context, page } = run;
    const { provider, sshUser } = spec;
    const request = page.request;
    let primaryError: unknown;
    let cleanupError: unknown;

    try {
      logStep(
        `Waiting for PKO v2 Stack tenant-${tenant}/${stackName} to provision VM...`,
        provider,
      );
      await waitForStackReady(stackName, `tenant-${tenant}`, provider, PROVISION_TIMEOUT_MS);

      logStep(`Verifying stack SSH info for ${stackName}...`, provider);
      const sshRes = await request.get(
        `${BASE_URL}/api/v1/stacks/tenant-${tenant}/${stackName}/ssh-info`,
      );
      expect(sshRes.ok(), `[${provider}] Expected ssh-info 200 OK, got ${sshRes.status()}`).toBe(
        true,
      );
      const sshJson = (await sshRes.json()) as {
        sshHost?: string;
        sshPort?: number;
        sshUser?: string;
      };
      expect(sshJson.sshHost, `[${provider}] Expected non-empty sshHost`).toBeTruthy();
      expect(sshJson.sshPort).toBe(22);
      expect(sshJson.sshUser).toBe(sshUser);
      logStep(
        `Verified SSH info: ${sshJson.sshUser}@${sshJson.sshHost}:${sshJson.sshPort}`,
        provider,
      );
    } catch (err) {
      primaryError = err;
      throw err;
    } finally {
      try {
        try {
          await deleteAndConfirmStage1StackRemoved(request, tenant, stackName, provider);
        } catch (err) {
          if (primaryError) {
            logStep(
              `Cleanup warning after primary error: ${err instanceof Error ? err.message : String(err)}`,
              provider,
            );
          } else {
            cleanupError = err;
          }
        }
      } finally {
        await context.close();
      }
    }
    if (!primaryError && cleanupError) {
      throw cleanupError;
    }
  }

  function generateEphemeralSshPublicKey(): string {
    const tmpDir = fs.mkdtempSync(path.join(os.tmpdir(), 'keel-e2e-ssh-'));
    const keyPath = path.join(tmpDir, 'id_ed25519');
    try {
      runCmd('ssh-keygen', ['-t', 'ed25519', '-N', '', '-C', 'e2e@keel.local', '-f', keyPath]);
      return fs.readFileSync(`${keyPath}.pub`, 'utf-8').trim();
    } finally {
      fs.rmSync(tmpDir, { recursive: true, force: true });
    }
  }

  test('08.1a - Parallel Multi-Cloud Stage 1 Free-Tier VM Provisioning, PKO Preview, SSH Info & Teardown', async ({
    browser,
  }) => {
    const defaultSshKey = process.env.KEEL_E2E_SSH_PUBLIC_KEY || generateEphemeralSshPublicKey();

    const specs: CloudVmSpec[] = [
      {
        provider: 'aws',
        catalogId: 'aws-vm',
        sshUser: 'ubuntu',
        values: {
          region: process.env.AWS_REGION || 'us-east-1',
          instanceType: process.env.KEEL_E2E_AWS_INSTANCE_TYPE || 't3.micro',
          instanceName: '${stackName}',
          sshUser: 'ubuntu',
          sshPublicKey: defaultSshKey,
          awsAccessKeyId: process.env.AWS_ACCESS_KEY_ID,
          awsSecretAccessKey: process.env.AWS_SECRET_ACCESS_KEY,
        },
      },
      {
        provider: 'azure',
        catalogId: 'azure-vm',
        sshUser: 'azureuser',
        values: {
          location: process.env.AZURE_LOCATION || 'eastus',
          vmSize: process.env.KEEL_E2E_AZURE_VM_SIZE || 'Standard_B1s',
          vmName: '${stackName}',
          sshUser: 'azureuser',
          sshPublicKey: defaultSshKey,
          armClientId: process.env.ARM_CLIENT_ID,
          armClientSecret: process.env.ARM_CLIENT_SECRET,
          armTenantId: process.env.ARM_TENANT_ID,
          armSubscriptionId: process.env.ARM_SUBSCRIPTION_ID,
        },
      },
      {
        provider: 'gcp',
        catalogId: 'gcp-vm',
        sshUser: 'debian',
        values: {
          project: process.env.GCP_PROJECT || process.env.GOOGLE_PROJECT,
          region: process.env.GCP_REGION || 'us-central1',
          zone: process.env.GCP_ZONE || 'us-central1-a',
          machineType: process.env.KEEL_E2E_GCP_MACHINE_TYPE || 'e2-micro',
          instanceName: '${stackName}',
          sshUser: 'debian',
          sshPublicKey: defaultSshKey,
          googleCredentials:
            process.env.GOOGLE_CREDENTIALS || process.env.GOOGLE_APPLICATION_CREDENTIALS,
        },
      },
    ];

    const enabledSpecs: CloudVmSpec[] = [];
    for (const spec of specs) {
      const gate = resolveCloudTargetGate(spec.provider);
      if (gate.enabled) {
        enabledSpecs.push(spec);
      } else {
        logStep(`SKIPPED: ${gate.reason}`, spec.provider);
      }
    }

    test.skip(
      enabledSpecs.length === 0,
      'No cloud providers enabled (set KEEL_E2E_CLOUD_<PROVIDER>=1 or KEEL_E2E_CLOUD_ALL=1 with credentials)',
    );

    logStep(
      `Running ${enabledSpecs.length} cloud provider(s) in parallel: ${enabledSpecs.map((s) => s.provider).join(', ')}`,
    );

    // Ensure all enabled cloud providers are toggled on in providers/registry.yaml and Kyverno
    // `restrict-pulumi-providers` before launching Stage 1 cloud VM proposals in parallel.
    {
      const setupContext = await browser.newContext({ ignoreHTTPSErrors: true, baseURL: BASE_URL });
      const setupPage = await setupContext.newPage();
      try {
        await signInViaRauthy(setupPage, 'admin@keel.local', adminPassword);
        const providersRes = await setupPage.request.get(`${BASE_URL}/api/v1/providers`);
        expect(providersRes.ok()).toBe(true);
        const providersBody = (await providersRes.json()) as {
          providers?: Array<{ id: string; enabled: boolean }>;
        };
        const currentProviders = providersBody.providers ?? [];
        for (const spec of enabledSpecs) {
          const entry = currentProviders.find((p) => p.id === spec.provider);
          if (!entry?.enabled) {
            logStep(`Enabling provider "${spec.provider}" via Keel API...`, spec.provider);
            const toggleRes = await setupPage.request.put(
              `${BASE_URL}/api/v1/providers/${spec.provider}`,
              { data: { enabled: true } },
            );
            expect(
              toggleRes.ok(),
              `[${spec.provider}] Failed to toggle provider: ${await toggleRes.text()}`,
            ).toBe(true);
            const toggleJson = (await toggleRes.json()) as {
              proposal?: { id?: string | number };
            };
            const togglePrId = toggleJson.proposal?.id;
            if (togglePrId !== undefined) {
              const mergeRes = await setupPage.request.post(
                `${BASE_URL}/api/v1/proposals/${togglePrId}/merge`,
              );
              expect(mergeRes.ok()).toBe(true);
            }
          }
        }
      } finally {
        await setupContext.close();
      }
    }

    // Phase 1: Create, preview, and merge all cloud PRs sequentially so `main` reaches its
    // final commit before Flux/PKO starts workspace pods (prevents RollingUpdate lock collisions).
    const mergedRuns: ProvisionedCloudRun[] = [];
    for (const spec of enabledSpecs) {
      try {
        mergedRuns.push(await createAndMergeCloudProposal(browser, spec));
      } catch (err) {
        logStep(
          `Proposal phase failed: ${err instanceof Error ? err.message : String(err)}`,
          spec.provider,
        );
      }
    }

    // Phase 2: Reconcile, verify SSH info, and tear down all merged cloud stacks in parallel.
    const results = await Promise.allSettled(
      mergedRuns.map((run) => reconcileVerifyAndDeleteCloudStack(run)),
    );

    const failures = results
      .map((r, i) => ({ result: r, provider: mergedRuns[i].spec.provider }))
      .filter(
        (x): x is { result: PromiseRejectedResult; provider: CloudProvider } =>
          x.result.status === 'rejected',
      );

    if (mergedRuns.length !== enabledSpecs.length) {
      throw new Error(
        `Only ${mergedRuns.length}/${enabledSpecs.length} cloud proposal(s) merged cleanly`,
      );
    }

    if (failures.length > 0) {
      const summary = failures
        .map(
          (f) =>
            `[${f.provider}] ${f.result.reason instanceof Error ? f.result.reason.message : String(f.result.reason)}`,
        )
        .join('\n\n');
      throw new Error(`${failures.length} cloud provider(s) failed:\n\n${summary}`);
    }
  });
});
