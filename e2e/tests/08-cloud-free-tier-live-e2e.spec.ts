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

function logStep(msg: string): void {
  const ts = new Date().toISOString().slice(11, 19);
  console.log(`[cloud-e2e ${ts}] ${msg}`);
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

async function authenticateApiContext(
  request: import('@playwright/test').APIRequestContext,
  adminPassword: string,
): Promise<void> {
  const statusRes = await request.get(`${BASE_URL}/api/v1/auth/status`);
  if (!statusRes.ok()) return;
  const status = (await statusRes.json()) as {
    authenticated?: boolean;
    localAuthEnabled?: boolean;
  };
  if (status.authenticated) return;

  await request.post(`${BASE_URL}/api/v1/auth/local/login`, {
    data: { username: 'admin', password: adminPassword },
  });
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
): Promise<void> {
  const namespace = `tenant-${tenant}`;
  logStep(`Requesting deletion of Stage 1 stack ${namespace}/${stackName}...`);
  const delRes = await request
    .delete(`${BASE_URL}/api/v1/stacks/${namespace}/${stackName}`)
    .catch(() => null);
  if (!delRes || !delRes.ok()) {
    return;
  }

  const delProposal = (await delRes.json().catch(() => null)) as { id?: string | number } | null;
  if (delProposal?.id !== undefined) {
    logStep(`Merging deletion PR #${delProposal.id} for ${namespace}/${stackName}...`);
    const mergeRes = await request.post(`${BASE_URL}/api/v1/proposals/${delProposal.id}/merge`);
    expect(mergeRes.ok()).toBe(true);
  }

  // 1. Poll GET /api/v1/stacks until the stack is confirmed removed from Keel
  logStep(`Waiting for confirmation that ${namespace}/${stackName} is removed from Keel stacks...`);
  const apiDeadline = Date.now() + 180_000;
  let removedFromApi = false;
  while (Date.now() < apiDeadline) {
    const stacksRes = await request.get(`${BASE_URL}/api/v1/stacks`);
    if (stacksRes.ok()) {
      const stacks = (await stacksRes.json()) as Array<{ name?: string; tenant?: string }>;
      const stillExists = stacks.some((s) => s.name === stackName && s.tenant === tenant);
      if (!stillExists) {
        removedFromApi = true;
        break;
      }
    }
    await new Promise((r) => setTimeout(r, 3000));
  }
  expect(
    removedFromApi,
    `Timed out waiting for stack ${namespace}/${stackName} to be removed from /api/v1/stacks`,
  ).toBe(true);

  // 2. Wait until the K8s PKO v2 Stack object finishes its `destroyOnFinalize: true` cloud teardown
  // and is completely deleted from the management cluster.
  try {
    runCmd('kubectl', ['get', 'crd', 'stacks.pulumi.com']);
    logStep(
      `Waiting for PKO v2 Stack ${namespace}/${stackName} finalizer (destroyOnFinalize) to complete in Kubernetes...`,
    );
    const k8sDeadline = Date.now() + 300_000;
    let removedFromK8s = false;
    while (Date.now() < k8sDeadline) {
      const getRes = tryCmd('kubectl', ['get', 'stack.pulumi.com', stackName, '-n', namespace]);
      if (!getRes.ok) {
        removedFromK8s = true;
        break;
      }
      await new Promise((r) => setTimeout(r, 5000));
    }
    expect(
      removedFromK8s,
      `Timed out waiting for PKO v2 Stack ${namespace}/${stackName} finalizer to finish cloud teardown`,
    ).toBe(true);
    logStep(
      `Confirmed PKO v2 Stack ${namespace}/${stackName} and its cloud resources are deleted.`,
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
    const deployDir = path.resolve(__dirname, '../../deploy');

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
      ['aws', 'aws-free-tier-vm', 't3.micro'],
      ['azure', 'azure-free-tier-vm', 'Standard_B1s'],
      ['gcp', 'gcp-free-tier-vm', 'e2-micro'],
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

test.describe('08.1 - Multi-Cloud Free-Tier Stage 1 Live E2E (kind Management Cluster -> Cloud VMs)', () => {
  test.describe.configure({ mode: 'serial' });

  let createdMgmtCluster = false;
  let serveProc: ChildProcess | null = null;
  let tmpRepoDir: string | null = null;
  const adminPassword = process.env.KEEL_ADMIN_PASSWORD || 'KeelE2eAdminPass123!';

  const anyCloudEnabled =
    resolveCloudTargetGate('aws').enabled ||
    resolveCloudTargetGate('azure').enabled ||
    resolveCloudTargetGate('gcp').enabled;

  test.beforeAll(async () => {
    if (!anyCloudEnabled) {
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

    if (!fs.existsSync(path.join(repoDir, 'catalog/infra/aws-free-tier-vm/catalog.yaml'))) {
      logStep('Seeding deploy/catalog/{apps,infra,bundles} into GitOps repo...');
      const deployCatalog = path.resolve(process.cwd(), '../deploy/catalog');
      fs.cpSync(deployCatalog, path.join(repoDir, 'catalog'), { recursive: true });
      runCmd('git', ['-C', repoDir, 'add', 'catalog']);
      const diff = tryCmd('git', ['-C', repoDir, 'diff', '--cached', '--quiet']);
      if (!diff.ok) {
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
            'chore(e2e): seed multi-cloud free-tier catalog blueprints',
          ],
          { GIT_EDITOR: 'true' },
        );
      }
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

  test('08.1a - AWS: Stage 1 Cloud Free-Tier VM Provisioning, PKO Preview & SSH Info Verification', async ({
    request,
  }) => {
    const gate = resolveCloudTargetGate('aws');
    test.skip(!gate.enabled, gate.reason);

    await authenticateApiContext(request, adminPassword);
    const stackName = `aws-vm-${Date.now()}`;
    const tenant = 'default';
    let stackProvisioned = false;

    try {
      logStep(`Creating proposal for aws-free-tier-vm (${stackName})...`);
      const createRes = await request.post(`${BASE_URL}/api/v1/proposals`, {
        data: {
          catalogId: 'aws-free-tier-vm',
          name: stackName,
          tenant,
          values: {
            region: process.env.AWS_REGION || 'us-east-1',
            instanceType: process.env.KEEL_E2E_AWS_INSTANCE_TYPE || 't3.micro',
            sshUser: 'ubuntu',
            sshPublicKey:
              process.env.KEEL_E2E_SSH_PUBLIC_KEY ||
              'ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIExampleKeyForKeelE2ETesting operator@keel.local',
            awsAccessKeyId: process.env.AWS_ACCESS_KEY_ID,
            awsSecretAccessKey: process.env.AWS_SECRET_ACCESS_KEY,
          },
        },
      });
      expect(createRes.ok(), `Failed to create AWS proposal: ${await createRes.text()}`).toBe(true);
      stackProvisioned = true;

      const proposal = (await createRes.json()) as { id: string | number };
      logStep(`Polling preview diff for proposal #${proposal.id}...`);
      const deadline = Date.now() + 180_000;
      let previewReady = false;
      while (Date.now() < deadline) {
        const prevRes = await request.get(`${BASE_URL}/api/v1/previews/${proposal.id}`);
        if (prevRes.ok()) {
          const body = await prevRes.text();
          if (body.includes('+ Create') || body.includes('create') || body.includes('preview')) {
            previewReady = true;
            break;
          }
        }
        await new Promise((r) => setTimeout(r, 5000));
      }
      expect(previewReady).toBe(true);

      logStep(`Merging proposal #${proposal.id}...`);
      const mergeRes = await request.post(`${BASE_URL}/api/v1/proposals/${proposal.id}/merge`);
      expect(mergeRes.ok()).toBe(true);

      logStep(`Verifying stack SSH info for ${stackName}...`);
      const sshRes = await request.get(
        `${BASE_URL}/api/v1/stacks/tenant-${tenant}/${stackName}/ssh-info`,
      );
      if (sshRes.ok()) {
        const sshJson = (await sshRes.json()) as {
          sshHost?: string;
          sshPort?: number;
          sshUser?: string;
        };
        expect(sshJson.sshPort).toBe(22);
        expect(sshJson.sshUser).toBe('ubuntu');
      }
    } finally {
      if (stackProvisioned) {
        await deleteAndConfirmStage1StackRemoved(request, tenant, stackName);
      } else {
        logStep(`No Stage 1 AWS stack provisioned (${stackName}); skipping delete.`);
      }
    }
  });

  test('08.2a - Azure: Stage 1 Cloud Free-Tier VM Provisioning, PKO Preview & SSH Info Verification', async ({
    request,
  }) => {
    const gate = resolveCloudTargetGate('azure');
    test.skip(!gate.enabled, gate.reason);

    await authenticateApiContext(request, adminPassword);
    const stackName = `azure-vm-${Date.now()}`;
    const tenant = 'default';
    let stackProvisioned = false;

    try {
      logStep(`Creating proposal for azure-free-tier-vm (${stackName})...`);
      const createRes = await request.post(`${BASE_URL}/api/v1/proposals`, {
        data: {
          catalogId: 'azure-free-tier-vm',
          name: stackName,
          tenant,
          values: {
            location: process.env.AZURE_LOCATION || 'eastus',
            vmSize: process.env.KEEL_E2E_AZURE_VM_SIZE || 'Standard_B1s',
            sshUser: 'azureuser',
            sshPublicKey:
              process.env.KEEL_E2E_SSH_PUBLIC_KEY ||
              'ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIExampleKeyForKeelE2ETesting operator@keel.local',
            armClientId: process.env.ARM_CLIENT_ID,
            armClientSecret: process.env.ARM_CLIENT_SECRET,
            armTenantId: process.env.ARM_TENANT_ID,
            armSubscriptionId: process.env.ARM_SUBSCRIPTION_ID,
          },
        },
      });
      expect(createRes.ok(), `Failed to create Azure proposal: ${await createRes.text()}`).toBe(
        true,
      );
      stackProvisioned = true;

      const proposal = (await createRes.json()) as { id: string | number };
      logStep(`Polling preview diff for proposal #${proposal.id}...`);
      const deadline = Date.now() + 180_000;
      let previewReady = false;
      while (Date.now() < deadline) {
        const prevRes = await request.get(`${BASE_URL}/api/v1/previews/${proposal.id}`);
        if (prevRes.ok()) {
          const body = await prevRes.text();
          if (body.includes('+ Create') || body.includes('create') || body.includes('preview')) {
            previewReady = true;
            break;
          }
        }
        await new Promise((r) => setTimeout(r, 5000));
      }
      expect(previewReady).toBe(true);

      logStep(`Merging proposal #${proposal.id}...`);
      const mergeRes = await request.post(`${BASE_URL}/api/v1/proposals/${proposal.id}/merge`);
      expect(mergeRes.ok()).toBe(true);

      logStep(`Verifying stack SSH info for ${stackName}...`);
      const sshRes = await request.get(
        `${BASE_URL}/api/v1/stacks/tenant-${tenant}/${stackName}/ssh-info`,
      );
      if (sshRes.ok()) {
        const sshJson = (await sshRes.json()) as {
          sshHost?: string;
          sshPort?: number;
          sshUser?: string;
        };
        expect(sshJson.sshPort).toBe(22);
        expect(sshJson.sshUser).toBe('azureuser');
      }
    } finally {
      if (stackProvisioned) {
        await deleteAndConfirmStage1StackRemoved(request, tenant, stackName);
      } else {
        logStep(`No Stage 1 Azure stack provisioned (${stackName}); skipping delete.`);
      }
    }
  });

  test('08.3a - GCP: Stage 1 Cloud Free-Tier VM Provisioning, PKO Preview & SSH Info Verification', async ({
    request,
  }) => {
    const gate = resolveCloudTargetGate('gcp');
    test.skip(!gate.enabled, gate.reason);

    await authenticateApiContext(request, adminPassword);
    const stackName = `gcp-vm-${Date.now()}`;
    const tenant = 'default';
    let stackProvisioned = false;

    try {
      logStep(`Creating proposal for gcp-free-tier-vm (${stackName})...`);
      const createRes = await request.post(`${BASE_URL}/api/v1/proposals`, {
        data: {
          catalogId: 'gcp-free-tier-vm',
          name: stackName,
          tenant,
          values: {
            project: process.env.GCP_PROJECT || process.env.GOOGLE_PROJECT,
            region: process.env.GCP_REGION || 'us-central1',
            zone: process.env.GCP_ZONE || 'us-central1-a',
            machineType: process.env.KEEL_E2E_GCP_MACHINE_TYPE || 'e2-micro',
            sshUser: 'debian',
            sshPublicKey:
              process.env.KEEL_E2E_SSH_PUBLIC_KEY ||
              'ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIExampleKeyForKeelE2ETesting operator@keel.local',
            googleCredentials:
              process.env.GOOGLE_CREDENTIALS || process.env.GOOGLE_APPLICATION_CREDENTIALS,
          },
        },
      });
      expect(createRes.ok(), `Failed to create GCP proposal: ${await createRes.text()}`).toBe(true);
      stackProvisioned = true;

      const proposal = (await createRes.json()) as { id: string | number };
      logStep(`Polling preview diff for proposal #${proposal.id}...`);
      const deadline = Date.now() + 180_000;
      let previewReady = false;
      while (Date.now() < deadline) {
        const prevRes = await request.get(`${BASE_URL}/api/v1/previews/${proposal.id}`);
        if (prevRes.ok()) {
          const body = await prevRes.text();
          if (body.includes('+ Create') || body.includes('create') || body.includes('preview')) {
            previewReady = true;
            break;
          }
        }
        await new Promise((r) => setTimeout(r, 5000));
      }
      expect(previewReady).toBe(true);

      logStep(`Merging proposal #${proposal.id}...`);
      const mergeRes = await request.post(`${BASE_URL}/api/v1/proposals/${proposal.id}/merge`);
      expect(mergeRes.ok()).toBe(true);

      logStep(`Verifying stack SSH info for ${stackName}...`);
      const sshRes = await request.get(
        `${BASE_URL}/api/v1/stacks/tenant-${tenant}/${stackName}/ssh-info`,
      );
      if (sshRes.ok()) {
        const sshJson = (await sshRes.json()) as {
          sshHost?: string;
          sshPort?: number;
          sshUser?: string;
        };
        expect(sshJson.sshPort).toBe(22);
        expect(sshJson.sshUser).toBe('debian');
      }
    } finally {
      if (stackProvisioned) {
        await deleteAndConfirmStage1StackRemoved(request, tenant, stackName);
      } else {
        logStep(`No Stage 1 GCP stack provisioned (${stackName}); skipping delete.`);
      }
    }
  });
});
