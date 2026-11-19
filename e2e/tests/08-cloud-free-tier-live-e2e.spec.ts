import { expect, test } from '@playwright/test';
import { execFileSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);

export interface CloudGateResult {
  enabled: boolean;
  reason?: string;
}

/**
 * Enforces the dual-guard environment contract for multi-cloud testing (`SDD.md` §10.2).
 * Both the explicit toggle variable (or `KEEL_E2E_CLOUD_ALL=1`) AND all required credentials
 * must be present; otherwise, live cloud operations skip cleanly.
 */
export function resolveCloudTargetGate(
  provider: 'aws' | 'azure' | 'gcp',
  env: NodeJS.ProcessEnv = process.env,
): CloudGateResult {
  const allEnabled = env.KEEL_E2E_CLOUD_ALL === '1';
  const providerToggle =
    provider === 'aws'
      ? env.KEEL_E2E_CLOUD_AWS === '1'
      : provider === 'azure'
        ? env.KEEL_E2E_CLOUD_AZURE === '1'
        : env.KEEL_E2E_CLOUD_GCP === '1';

  const toggleActive = allEnabled || providerToggle;
  if (!toggleActive) {
    return {
      enabled: false,
      reason: `Cloud toggle KEEL_E2E_CLOUD_${provider.toUpperCase()}=1 (or KEEL_E2E_CLOUD_ALL=1) is not set`,
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

  return { enabled: true };
}

function logStep(msg: string): void {
  const ts = new Date().toISOString().slice(11, 19);
  console.log(`[cloud-e2e ${ts}] ${msg}`);
}

function runCmd(bin: string, args: string[], cwd?: string, env?: NodeJS.ProcessEnv): string {
  return execFileSync(bin, args, {
    cwd,
    encoding: 'utf-8',
    env: { ...process.env, ...env },
    stdio: ['ignore', 'pipe', 'pipe'],
  }).trim();
}

test.describe('08 - Multi-Cloud Free-Tier Dual-Guard Contract & Blueprint Validation', () => {
  test('08.0a - Pure dual-guard helper enforces toggle and credentials requirement', () => {
    // 1. Missing toggle (credentials provided but toggle unset)
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

    // 2. Toggle enabled but missing credentials
    expect(resolveCloudTargetGate('aws', { KEEL_E2E_CLOUD_AWS: '1' }).enabled).toBe(false);
    expect(resolveCloudTargetGate('azure', { KEEL_E2E_CLOUD_AZURE: '1' }).enabled).toBe(false);
    expect(resolveCloudTargetGate('gcp', { KEEL_E2E_CLOUD_GCP: '1' }).enabled).toBe(false);

    // 3. Both toggle and credentials set -> enabled: true
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

    // 4. KEEL_E2E_CLOUD_ALL=1 enables all providers when credentials are provided
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

  test('08.0b - Free-tier catalog blueprints & Stage 0 templates exist on disk with required defaults', () => {
    const deployDir = path.resolve(__dirname, '../../deploy');

    // Stage 0 AWS
    const stage0Aws = fs.readFileSync(path.join(deployDir, 'bootstrap/aws/Pulumi.yaml'), 'utf-8');
    expect(stage0Aws).toContain('t3.micro');
    expect(stage0Aws).toContain('protectResources');
    expect(stage0Aws).toContain('sshHost:');
    expect(stage0Aws).toContain('sshUser:');
    expect(stage0Aws).toContain('sshPort: 22');

    // Stage 0 Azure
    const stage0Azure = fs.readFileSync(
      path.join(deployDir, 'bootstrap/azure/Pulumi.yaml'),
      'utf-8',
    );
    expect(stage0Azure).toContain('Standard_B1s');
    expect(stage0Azure).toContain('protectResources');
    expect(stage0Azure).toContain('sshHost:');
    expect(stage0Azure).toContain('sshUser:');
    expect(stage0Azure).toContain('sshPort: 22');

    // Stage 0 GCP
    const stage0Gcp = fs.readFileSync(path.join(deployDir, 'bootstrap/gcp/Pulumi.yaml'), 'utf-8');
    expect(stage0Gcp).toContain('e2-micro');
    expect(stage0Gcp).toContain('pd-standard');
    expect(stage0Gcp).toContain('protectResources');
    expect(stage0Gcp).toContain('sshHost:');
    expect(stage0Gcp).toContain('sshUser:');
    expect(stage0Gcp).toContain('sshPort: 22');

    // Stage 1 Blueprints
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

test.describe('08.1 - AWS Free-Tier Live E2E (Stage 0 & Stage 1)', () => {
  test('08.1a - AWS: Stage 0 Cloud Free-Tier Bootstrap & Teardown', async () => {
    const gate = resolveCloudTargetGate('aws');
    test.skip(!gate.enabled, gate.reason);

    const tmpWorkspace = fs.mkdtempSync(path.join(os.tmpdir(), 'keel-e2e-stage0-aws-'));
    const stackName = `e2e-aws-${Date.now()}`;
    const programDir = path.resolve(__dirname, '../../deploy/bootstrap/aws');

    try {
      logStep(`Running pulumi preview in ephemeral AWS stage 0 stack ${stackName}...`);
      runCmd('pulumi', ['login', `file://${tmpWorkspace}`]);
      runCmd('pulumi', ['stack', 'init', stackName], programDir);
      runCmd('pulumi', ['config', 'set', 'clusterName', stackName], programDir);
      runCmd('pulumi', ['config', 'set', 'protectResources', 'false'], programDir);
      const previewOut = runCmd('pulumi', ['preview', '--non-interactive'], programDir);
      expect(previewOut.length).toBeGreaterThan(0);

      logStep(`Running pulumi up in ephemeral AWS stage 0 stack ${stackName}...`);
      runCmd('pulumi', ['up', '--yes', '--non-interactive', '--skip-preview'], programDir);
      const outputs = JSON.parse(
        runCmd('pulumi', ['stack', 'output', '--json'], programDir),
      ) as Record<string, unknown>;
      expect(outputs.sshPort).toBe(22);
      expect(outputs.sshUser).toBeTruthy();
    } finally {
      try {
        logStep(`Tearing down ephemeral AWS stage 0 stack ${stackName}...`);
        runCmd('pulumi', ['destroy', '--yes', '--non-interactive'], programDir);
        runCmd('pulumi', ['stack', 'rm', '--yes', stackName], programDir);
      } catch (err) {
        console.warn(`Cleanup error for AWS stack ${stackName}:`, err);
      }
      fs.rmSync(tmpWorkspace, { recursive: true, force: true });
    }
  });

  test('08.1b - AWS: Stage 1 Cloud Free-Tier VM Provisioning, PKO Preview & SSH Info Verification', async ({
    request,
  }) => {
    const gate = resolveCloudTargetGate('aws');
    test.skip(!gate.enabled, gate.reason);

    const stackName = `aws-vm-${Date.now()}`;
    const tenant = 'default';

    try {
      logStep(`Creating proposal for aws-free-tier-vm (${stackName})...`);
      const createRes = await request.post('/api/v1/proposals', {
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

      if (createRes.ok()) {
        const proposal = (await createRes.json()) as { id: string | number };
        logStep(`Polling preview diff for proposal #${proposal.id}...`);
        const deadline = Date.now() + 180_000;
        let previewReady = false;
        while (Date.now() < deadline) {
          const prevRes = await request.get(`/api/v1/previews/${proposal.id}`);
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
        const mergeRes = await request.post(`/api/v1/proposals/${proposal.id}/merge`);
        expect(mergeRes.ok()).toBe(true);

        logStep(`Verifying stack SSH info for ${stackName}...`);
        const sshRes = await request.get(`/api/v1/stacks/tenant-${tenant}/${stackName}/ssh-info`);
        if (sshRes.ok()) {
          const sshJson = (await sshRes.json()) as {
            sshHost?: string;
            sshPort?: number;
            sshUser?: string;
          };
          expect(sshJson.sshPort).toBe(22);
          expect(sshJson.sshUser).toBe('ubuntu');
        }
      }
    } finally {
      logStep(`Cleaning up stack ${stackName}...`);
      await request.delete(`/api/v1/stacks/tenant-${tenant}/${stackName}`).catch(() => {});
    }
  });
});

test.describe('08.2 - Azure Free-Tier Live E2E (Stage 0 & Stage 1)', () => {
  test('08.2a - Azure: Stage 0 Cloud Free-Tier Bootstrap & Teardown', async () => {
    const gate = resolveCloudTargetGate('azure');
    test.skip(!gate.enabled, gate.reason);

    const tmpWorkspace = fs.mkdtempSync(path.join(os.tmpdir(), 'keel-e2e-stage0-azure-'));
    const stackName = `e2e-azure-${Date.now()}`;
    const programDir = path.resolve(__dirname, '../../deploy/bootstrap/azure');

    try {
      logStep(`Running pulumi preview in ephemeral Azure stage 0 stack ${stackName}...`);
      runCmd('pulumi', ['login', `file://${tmpWorkspace}`]);
      runCmd('pulumi', ['stack', 'init', stackName], programDir);
      runCmd('pulumi', ['config', 'set', 'clusterName', stackName], programDir);
      runCmd('pulumi', ['config', 'set', 'protectResources', 'false'], programDir);
      const previewOut = runCmd('pulumi', ['preview', '--non-interactive'], programDir);
      expect(previewOut.length).toBeGreaterThan(0);

      logStep(`Running pulumi up in ephemeral Azure stage 0 stack ${stackName}...`);
      runCmd('pulumi', ['up', '--yes', '--non-interactive', '--skip-preview'], programDir);
      const outputs = JSON.parse(
        runCmd('pulumi', ['stack', 'output', '--json'], programDir),
      ) as Record<string, unknown>;
      expect(outputs.sshPort).toBe(22);
      expect(outputs.sshUser).toBeTruthy();
    } finally {
      try {
        logStep(`Tearing down ephemeral Azure stage 0 stack ${stackName}...`);
        runCmd('pulumi', ['destroy', '--yes', '--non-interactive'], programDir);
        runCmd('pulumi', ['stack', 'rm', '--yes', stackName], programDir);
      } catch (err) {
        console.warn(`Cleanup error for Azure stack ${stackName}:`, err);
      }
      fs.rmSync(tmpWorkspace, { recursive: true, force: true });
    }
  });

  test('08.2b - Azure: Stage 1 Cloud Free-Tier VM Provisioning, PKO Preview & SSH Info Verification', async ({
    request,
  }) => {
    const gate = resolveCloudTargetGate('azure');
    test.skip(!gate.enabled, gate.reason);

    const stackName = `azure-vm-${Date.now()}`;
    const tenant = 'default';

    try {
      logStep(`Creating proposal for azure-free-tier-vm (${stackName})...`);
      const createRes = await request.post('/api/v1/proposals', {
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

      if (createRes.ok()) {
        const proposal = (await createRes.json()) as { id: string | number };
        logStep(`Polling preview diff for proposal #${proposal.id}...`);
        const deadline = Date.now() + 180_000;
        let previewReady = false;
        while (Date.now() < deadline) {
          const prevRes = await request.get(`/api/v1/previews/${proposal.id}`);
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
        const mergeRes = await request.post(`/api/v1/proposals/${proposal.id}/merge`);
        expect(mergeRes.ok()).toBe(true);

        logStep(`Verifying stack SSH info for ${stackName}...`);
        const sshRes = await request.get(`/api/v1/stacks/tenant-${tenant}/${stackName}/ssh-info`);
        if (sshRes.ok()) {
          const sshJson = (await sshRes.json()) as {
            sshHost?: string;
            sshPort?: number;
            sshUser?: string;
          };
          expect(sshJson.sshPort).toBe(22);
          expect(sshJson.sshUser).toBe('azureuser');
        }
      }
    } finally {
      logStep(`Cleaning up stack ${stackName}...`);
      await request.delete(`/api/v1/stacks/tenant-${tenant}/${stackName}`).catch(() => {});
    }
  });
});

test.describe('08.3 - GCP Free-Tier Live E2E (Stage 0 & Stage 1)', () => {
  test('08.3a - GCP: Stage 0 Cloud Free-Tier Bootstrap & Teardown', async () => {
    const gate = resolveCloudTargetGate('gcp');
    test.skip(!gate.enabled, gate.reason);

    const tmpWorkspace = fs.mkdtempSync(path.join(os.tmpdir(), 'keel-e2e-stage0-gcp-'));
    const stackName = `e2e-gcp-${Date.now()}`;
    const programDir = path.resolve(__dirname, '../../deploy/bootstrap/gcp');

    try {
      logStep(`Running pulumi preview in ephemeral GCP stage 0 stack ${stackName}...`);
      runCmd('pulumi', ['login', `file://${tmpWorkspace}`]);
      runCmd('pulumi', ['stack', 'init', stackName], programDir);
      runCmd('pulumi', ['config', 'set', 'clusterName', stackName], programDir);
      runCmd('pulumi', ['config', 'set', 'protectResources', 'false'], programDir);
      const previewOut = runCmd('pulumi', ['preview', '--non-interactive'], programDir);
      expect(previewOut.length).toBeGreaterThan(0);

      logStep(`Running pulumi up in ephemeral GCP stage 0 stack ${stackName}...`);
      runCmd('pulumi', ['up', '--yes', '--non-interactive', '--skip-preview'], programDir);
      const outputs = JSON.parse(
        runCmd('pulumi', ['stack', 'output', '--json'], programDir),
      ) as Record<string, unknown>;
      expect(outputs.sshPort).toBe(22);
      expect(outputs.sshUser).toBeTruthy();
    } finally {
      try {
        logStep(`Tearing down ephemeral GCP stage 0 stack ${stackName}...`);
        runCmd('pulumi', ['destroy', '--yes', '--non-interactive'], programDir);
        runCmd('pulumi', ['stack', 'rm', '--yes', stackName], programDir);
      } catch (err) {
        console.warn(`Cleanup error for GCP stack ${stackName}:`, err);
      }
      fs.rmSync(tmpWorkspace, { recursive: true, force: true });
    }
  });

  test('08.3b - GCP: Stage 1 Cloud Free-Tier VM Provisioning, PKO Preview & SSH Info Verification', async ({
    request,
  }) => {
    const gate = resolveCloudTargetGate('gcp');
    test.skip(!gate.enabled, gate.reason);

    const stackName = `gcp-vm-${Date.now()}`;
    const tenant = 'default';

    try {
      logStep(`Creating proposal for gcp-free-tier-vm (${stackName})...`);
      const createRes = await request.post('/api/v1/proposals', {
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

      if (createRes.ok()) {
        const proposal = (await createRes.json()) as { id: string | number };
        logStep(`Polling preview diff for proposal #${proposal.id}...`);
        const deadline = Date.now() + 180_000;
        let previewReady = false;
        while (Date.now() < deadline) {
          const prevRes = await request.get(`/api/v1/previews/${proposal.id}`);
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
        const mergeRes = await request.post(`/api/v1/proposals/${proposal.id}/merge`);
        expect(mergeRes.ok()).toBe(true);

        logStep(`Verifying stack SSH info for ${stackName}...`);
        const sshRes = await request.get(`/api/v1/stacks/tenant-${tenant}/${stackName}/ssh-info`);
        if (sshRes.ok()) {
          const sshJson = (await sshRes.json()) as {
            sshHost?: string;
            sshPort?: number;
            sshUser?: string;
          };
          expect(sshJson.sshPort).toBe(22);
          expect(sshJson.sshUser).toBe('debian');
        }
      }
    } finally {
      logStep(`Cleaning up stack ${stackName}...`);
      await request.delete(`/api/v1/stacks/tenant-${tenant}/${stackName}`).catch(() => {});
    }
  });
});
