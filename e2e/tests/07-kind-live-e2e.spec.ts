import { expect, test } from '@playwright/test';
import { execFileSync, spawn, type ChildProcess } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';

const LIVE_ENABLED = process.env.KEEL_E2E_LIVE === '1';
const MGMT_CLUSTER = process.env.KEEL_E2E_MGMT_CLUSTER || 'keel-mgmt';
const WORKLOAD_CLUSTER = process.env.KEEL_E2E_WORKLOAD_CLUSTER || 'e2e-lab';
const BASE_URL = process.env.KEEL_BASE_URL || 'http://127.0.0.1:8080';
const KEEL_BIN = process.env.KEEL_BIN || path.resolve(process.cwd(), '../target/debug/keel');
const SERVE_LOG_PATH = path.join(os.tmpdir(), 'keel-e2e-serve.log');
const DETERMINISTIC_REPO_DIR = path.join(os.tmpdir(), `keel-e2e-gitops-${MGMT_CLUSTER}`);

function logStep(msg: string): void {
  const ts = new Date().toISOString().slice(11, 19);
  console.log(`[e2e ${ts}] ${msg}`);
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

async function waitForK8sReady(
  kind: 'helmrelease' | 'stack' | 'kustomization',
  name: string,
  namespace: string,
  timeoutMs = 300_000,
): Promise<void> {
  const start = Date.now();
  const deadline = start + timeoutMs;
  let lastLog = 0;

  while (Date.now() < deadline) {
    const res = tryCmd('kubectl', [
      'get',
      kind,
      name,
      '-n',
      namespace,
      '-o',
      'jsonpath={.status.conditions[?(@.type=="Ready")].status}|{.status.conditions[?(@.type=="Ready")].reason}|{.status.conditions[?(@.type=="Ready")].message}',
    ]);
    const [status = '', reason = '', message = ''] = res.stdout.split('|');
    if (res.ok && status === 'True') {
      const elapsed = Math.round((Date.now() - start) / 1000);
      logStep(`Ready: ${kind}/${name} (${namespace}) in ${elapsed}s`);
      return;
    }
    const now = Date.now();
    if (now - lastLog >= 15_000) {
      const elapsed = Math.round((now - start) / 1000);
      const maxSec = Math.round(timeoutMs / 1000);
      const detail = reason || message || 'pending';
      logStep(
        `Waiting: ${kind}/${name} (${namespace}) Ready=${status || 'Unknown'} (${detail}) [${elapsed}s/${maxSec}s]`,
      );
      lastLog = now;
    }
    await new Promise((r) => setTimeout(r, 5000));
  }
  throw new Error(`Timed out waiting for ${kind}/${name} in ${namespace} to become Ready=True`);
}

// Note: `keel bootstrap init --target kind` provisions the `kind` management cluster
// via `deploy/bootstrap/kind/Pulumi.yaml` (`tehcyx/kind`) instead of direct `kind create cluster`.
test.describe('07 - Live kind Management + Workload Cluster E2E (GUI + CLI)', () => {
  test.skip(
    !LIVE_ENABLED,
    'Set KEEL_E2E_LIVE=1 (or run `pnpm --dir e2e test:live`) to run live kind E2E',
  );

  let createdMgmtCluster = false;
  let serveProc: ChildProcess | null = null;
  let tmpRepoDir: string | null = null;
  const adminPassword = process.env.KEEL_ADMIN_PASSWORD || 'KeelE2eAdminPass123!';

  test.beforeAll(async () => {
    const clusters = tryCmd('kind', ['get', 'clusters']).stdout.split(/\r?\n/).filter(Boolean);
    const hasExistingMgmt = clusters.includes(MGMT_CLUSTER);

    if (!hasExistingMgmt || process.env.KEEL_E2E_REUSE_CLUSTER === '0') {
      if (hasExistingMgmt) {
        logStep(`Deleting existing kind cluster "${MGMT_CLUSTER}" (KEEL_E2E_REUSE_CLUSTER=0)...`);
        runCmd('kind', ['delete', 'cluster', '--name', MGMT_CLUSTER], undefined, true);
      }
      fs.rmSync(`/tmp/keel-stage0-pulumi/kind-${MGMT_CLUSTER}`, { recursive: true, force: true });
      if (fs.existsSync(DETERMINISTIC_REPO_DIR)) {
        fs.rmSync(DETERMINISTIC_REPO_DIR, { recursive: true, force: true });
      }
      if (fs.existsSync(`${DETERMINISTIC_REPO_DIR}.git`)) {
        fs.rmSync(`${DETERMINISTIC_REPO_DIR}.git`, { recursive: true, force: true });
      }
      fs.mkdirSync(DETERMINISTIC_REPO_DIR, { recursive: true });
      tmpRepoDir = DETERMINISTIC_REPO_DIR;
      logStep(
        `Bootstrapping fresh kind management cluster "${MGMT_CLUSTER}" via deploy/bootstrap/kind/Pulumi.yaml (repo: ${tmpRepoDir})...`,
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
      createdMgmtCluster = true;
    } else {
      logStep(`Reusing existing kind management cluster "${MGMT_CLUSTER}"`);
    }

    const repoDir =
      process.env.KEEL_E2E_REPO_DIR ||
      tmpRepoDir ||
      (fs.existsSync(DETERMINISTIC_REPO_DIR)
        ? DETERMINISTIC_REPO_DIR
        : path.resolve(process.cwd(), '../keel-gitops'));

    if (!fs.existsSync(path.join(repoDir, 'catalog/bundles/kind-canary-platform/catalog.yaml'))) {
      logStep('Seeding deploy/catalog/{apps,infra,bundles} into GitOps repo...');
      const deployCatalog = path.resolve(process.cwd(), '../deploy/catalog');
      fs.cpSync(deployCatalog, path.join(repoDir, 'catalog'), { recursive: true });
      runCmd('git', ['-C', repoDir, 'add', 'catalog']);
      const diff = tryCmd('git', ['-C', repoDir, 'diff', '--cached', '--quiet']);
      if (!diff.ok) {
        runCmd(
          'git',
          ['-C', repoDir, 'commit', '-m', 'chore(e2e): seed full catalog blueprints and bundles'],
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

    const unpushedCount = Number(
      tryCmd('git', ['-C', repoDir, 'rev-list', '--count', 'origin/main..HEAD']).stdout || '0',
    );
    if (unpushedCount > 0) {
      logStep(`Pushing ${unpushedCount} seeded commit(s) to origin/main...`);
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
        const askpassPath = path.join(os.tmpdir(), 'keel-e2e-askpass.sh');
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
        runCmd('git', ['-C', repoDir, 'push', 'origin', 'main']);
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

    const clustersToClean = [WORKLOAD_CLUSTER, ...(createdMgmtCluster ? [MGMT_CLUSTER] : [])];
    if (process.env.CI) {
      for (const name of clustersToClean) {
        tryCmd('kind', ['delete', 'cluster', '--name', name]);
      }
      if (tmpRepoDir) {
        fs.rmSync(tmpRepoDir, { recursive: true, force: true });
      }
    } else {
      console.log(
        `tests complete - clean up using: kind delete cluster --name ${clustersToClean.join(' ')}`,
      );
    }
  });

  test('07.1 - CLI standalone app on mgmt -> GUI bundle workload kind cluster -> GUI standalone canary app on workload cluster', async ({
    page,
  }) => {
    await test.step('1. Authenticate in GUI via Dex OIDC', async () => {
      logStep('Opening GUI and signing in via Dex OIDC...');
      await page.goto('/');
      await expect(page.getByText('Keel Control Plane')).toBeVisible();

      if (await page.getByText('Authentication Required').isVisible()) {
        await page
          .getByRole('button', { name: /Sign in with OIDC/i })
          .first()
          .click();
        const loginInput = page.locator('input[name="login"], input#login').first();
        await expect(loginInput).toBeVisible({ timeout: 20_000 });
        await loginInput.fill('admin@keel.local');
        await page.locator('input[name="password"], input#password').first().fill(adminPassword);
        await page.locator('button[type="submit"], #submit-login').first().click();

        const grantBtn = page.getByRole('button', { name: /Grant Access/i });
        if (await grantBtn.isVisible({ timeout: 5_000 }).catch(() => false)) {
          await grantBtn.click();
        }
      }

      await expect(page.getByText('management-cluster-core', { exact: true })).toBeVisible({
        timeout: 30_000,
      });
      logStep('Authenticated and management-cluster-core is visible');
    });

    const cookies = await page.context().cookies(BASE_URL);
    const sessionCookie = cookies.find((c) => c.name === 'keel_session');
    const localToken = await page.evaluate(() => localStorage.getItem('keel_token'));
    const sessionToken = sessionCookie?.value || localToken || '';
    expect(sessionToken.length).toBeGreaterThan(0);

    const cliHome = fs.mkdtempSync(path.join(os.tmpdir(), 'keel-e2e-cli-home-'));
    const credsDir = path.join(cliHome, '.config', 'keel');
    fs.mkdirSync(credsDir, { recursive: true });
    fs.writeFileSync(
      path.join(credsDir, 'credentials.json'),
      JSON.stringify({ sessions: { [BASE_URL.replace(/\/+$/, '')]: sessionToken } }, null, 2),
      { mode: 0o600 },
    );
    const cliEnv: NodeJS.ProcessEnv = { HOME: cliHome };

    await test.step('2. CLI: Deploy standalone podinfo on management cluster', async () => {
      logStep('Running `keel app install` for podinfo on management cluster...');
      const valuesPath = path.join(os.tmpdir(), 'keel-e2e-mgmt-podinfo.yaml');
      fs.writeFileSync(
        valuesPath,
        [
          'replicaCount: 1',
          'ingressHost: podinfo.keel.local',
          'apiToken: e2e-podinfo-token-1234',
          'securityContext:',
          '  allowPrivilegeEscalation: false',
          '  runAsNonRoot: true',
          '  runAsUser: 100',
          '  capabilities:',
          '    drop:',
          '      - ALL',
          '  seccompProfile:',
          '    type: RuntimeDefault',
          '',
        ].join('\n'),
        'utf-8',
      );
      const cliInstallOut = runCmd(
        KEEL_BIN,
        [
          'app',
          'install',
          '--app',
          'podinfo',
          '--tenant',
          'default',
          '--values-file',
          valuesPath,
          '--server',
          BASE_URL,
        ],
        cliEnv,
      );
      fs.unlinkSync(valuesPath);
      const prMatch = cliInstallOut.match(/PR\s*#?(\d+)/i);
      if (prMatch?.[1]) {
        logStep(`Checking CLI preview status for PR #${prMatch[1]}...`);
        const previewOut = runCmd(
          KEEL_BIN,
          ['preview', 'status', '--pr', prMatch[1], '--server', BASE_URL],
          cliEnv,
        );
        expect(previewOut.length).toBeGreaterThan(0);
      }

      logStep('Merging management-cluster podinfo PR via GUI...');
      await page.getByRole('button', { name: /PR & Preview Inspector/i }).click();
      await expect(page.getByRole('button', { name: /Merge PR/i }).first()).toBeVisible({
        timeout: 15_000,
      });
      await page
        .getByRole('button', { name: /Merge PR/i })
        .first()
        .click();
      await expect(page.getByText(/merged cleanly/i)).toBeVisible({ timeout: 20_000 });
      await waitForK8sReady('helmrelease', 'podinfo', 'tenant-default', 180_000);
    });

    await test.step('3. GUI Bundle: Provision workload kind cluster + addons (kind-canary-platform)', async () => {
      const existingCluster = tryCmd('kubectl', [
        'get',
        'stack',
        `${WORKLOAD_CLUSTER}-cluster`,
        '-n',
        'tenant-default',
      ]);
      if (existingCluster.ok) {
        logStep(`Bundle "${WORKLOAD_CLUSTER}" already exists; waiting for Ready=True...`);
      } else {
        logStep(`Submitting kind-canary-platform bundle PR for "${WORKLOAD_CLUSTER}"...`);
        await page.getByRole('button', { name: /Catalog & Form Wizard/i }).click();
        const bundleCard = page
          .locator('[data-testid="catalog-list"] > div')
          .filter({ has: page.getByText('kind-canary-platform', { exact: true }) })
          .first();
        await expect(bundleCard).toBeVisible({ timeout: 15_000 });
        await bundleCard.getByRole('button', { name: 'Create' }).click();

        await page.locator('#instance-name-input').fill(WORKLOAD_CLUSTER);
        await page.getByRole('button', { name: /Submit & Open Git PR Proposal/i }).click();
        await expect(page.getByText(/Git PR Proposal Successfully Opened!/i)).toBeVisible({
          timeout: 20_000,
        });

        logStep('Merging kind-canary-platform bundle PR...');
        await page.getByRole('button', { name: /PR & Preview Inspector/i }).click();
        await page
          .getByRole('button', { name: /Merge PR/i })
          .first()
          .click();
        await expect(page.getByText(/merged cleanly/i)).toBeVisible({ timeout: 20_000 });
      }

      await waitForK8sReady('stack', `${WORKLOAD_CLUSTER}-cluster`, 'tenant-default', 300_000);
      await waitForK8sReady(
        'stack',
        `${WORKLOAD_CLUSTER}-canary-config`,
        'tenant-default',
        300_000,
      );
      await waitForK8sReady(
        'helmrelease',
        `${WORKLOAD_CLUSTER}-flagger`,
        'tenant-default',
        300_000,
      );
    });

    const workloadAppName = `${WORKLOAD_CLUSTER}-podinfo`;
    await test.step('4. GUI Standalone App: Deploy canary podinfo onto workload cluster', async () => {
      const existingApp = tryCmd('kubectl', [
        'get',
        'helmrelease',
        workloadAppName,
        '-n',
        'tenant-default',
      ]);
      if (existingApp.ok) {
        logStep(`Workload app "${workloadAppName}" already exists; waiting for Ready=True...`);
      } else {
        logStep(
          `Submitting standalone canary podinfo PR ("${workloadAppName}") on workload cluster...`,
        );
        await page.getByRole('button', { name: /Catalog & Form Wizard/i }).click();
        const podinfoCard = page
          .locator('[data-testid="catalog-list"] > div')
          .filter({ has: page.getByText('podinfo', { exact: true }) })
          .first();
        await expect(podinfoCard).toBeVisible({ timeout: 15_000 });
        await podinfoCard.getByRole('button', { name: 'Create' }).click();

        await page.locator('#instance-name-input').fill(workloadAppName);
        const targetClusterSelect = page.locator('#cluster-select-catalog');
        if (await targetClusterSelect.isVisible()) {
          await targetClusterSelect.selectOption(`${WORKLOAD_CLUSTER}-cluster-kubeconfig`);
        }
        const canaryToggle = page.locator('#deploy-enable-canary');
        if (await canaryToggle.isVisible()) {
          await canaryToggle.check();
        }
        const apiTokenInput = page.getByLabel(/API Secret Token/i);
        if (await apiTokenInput.isVisible()) {
          await apiTokenInput.fill('e2e-workload-token-1234');
        }
        await page.getByRole('button', { name: /Submit & Open Git PR Proposal/i }).click();
        await expect(page.getByText(/Git PR Proposal Successfully Opened!/i)).toBeVisible({
          timeout: 20_000,
        });

        logStep('Merging workload-cluster podinfo PR...');
        await page.getByRole('button', { name: /PR & Preview Inspector/i }).click();
        await page
          .getByRole('button', { name: /Merge PR/i })
          .first()
          .click();
        await expect(page.getByText(/merged cleanly/i)).toBeVisible({ timeout: 20_000 });
      }

      await waitForK8sReady('helmrelease', workloadAppName, 'tenant-default', 240_000);
      await waitForK8sReady(
        'kustomization',
        `${workloadAppName}-canary`,
        'tenant-default',
        180_000,
      );
    });

    await test.step('5. Verify both clusters & apps in Topology Map', async () => {
      logStep('Verifying clusters and apps in Topology Map...');
      await page.getByRole('button', { name: /Topology Map/i }).click();
      await page.getByRole('button', { name: /Refresh Map/i }).click();
      await expect(page.getByText(`${WORKLOAD_CLUSTER}-cluster`).first()).toBeVisible({
        timeout: 20_000,
      });
      await expect(page.getByText(workloadAppName).first()).toBeVisible({ timeout: 20_000 });
      logStep('Topology Map verification complete');
    });
  });
});
