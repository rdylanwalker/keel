import { expect, test } from '@playwright/test';
import { execFileSync, spawn, type ChildProcess } from 'node:child_process';
import { randomUUID } from 'node:crypto';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';

const LIVE_ENABLED = process.env.KEEL_E2E_LIVE === '1';
const RECORD_DEMO = process.env.KEEL_E2E_RECORD_DEMO === '1';
const DEMO_VIDEO_PATH = path.resolve(
  process.cwd(),
  process.env.KEEL_DEMO_VIDEO_PATH || '../docs/assets/demo.webm',
);

if (RECORD_DEMO) {
  test.use({
    viewport: { width: 1280, height: 800 },
    video: {
      mode: 'on',
      size: { width: 1280, height: 800 },
    },
  });
}
const MGMT_CLUSTER = process.env.KEEL_E2E_MGMT_CLUSTER || 'keel-mgmt';
const WORKLOAD_CLUSTER = process.env.KEEL_E2E_WORKLOAD_CLUSTER || 'e2e-lab';
/** kind cluster provisioned by tenant acme's PKO blueprint (07.6). */
const ACME_CLUSTER = 'e2e-acme';
const BASE_URL = process.env.KEEL_BASE_URL || 'http://127.0.0.1:8080';
const KEEL_BIN = process.env.KEEL_BIN || path.resolve(process.cwd(), '../target/debug/keel');
const SERVE_LOG_PATH = path.join(os.tmpdir(), 'keel-e2e-serve.log');
const DETERMINISTIC_REPO_DIR = path.join(os.tmpdir(), `keel-e2e-gitops-${MGMT_CLUSTER}`);
/** Forge organisation holding every tenant repository (`keel_core::TENANT_REPOS_ORG`). */
const TENANT_REPOS_ORG = 'keel-tenants';
/** Public HTTPS origin of the in-cluster Forgejo (Envoy Gateway on the kind host port). */
const FORGEJO_ORIGIN = process.env.KEEL_E2E_FORGEJO_ORIGIN || 'https://forgejo.keel.local:8888';
/** Files of a tenant's platform-owned stub in the monorepo (`TENANT_REPO_STUB_FILES`). */
const TENANT_STUB_FILES = ['git-repository.yaml', 'rbac.yaml', 'sync.yaml'];

interface ScopedProposalBody {
  id: string;
  title: string;
  scope: string;
  headSha?: string;
  repository: { label: string; fullName?: string };
}

interface CreateTenantBody {
  proposal: ScopedProposalBody | null;
  repository: {
    layout: string;
    owner?: string;
    name?: string;
    url?: string;
    transport?: string;
    skeletonProposal?: ScopedProposalBody;
    notes: string[];
  };
}

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

/** Origin header for cookie-authenticated mutations (the server's CSRF guard requires it). */
const SAME_ORIGIN = { Origin: new URL(BASE_URL).origin };

/** Mirrors `keel_core::keel_config_dir()` ($KEEL_CONFIG_DIR, $XDG_CONFIG_HOME/keel, ~/.config/keel). */
function keelConfigDir(): string {
  if (process.env.KEEL_CONFIG_DIR) return process.env.KEEL_CONFIG_DIR;
  if (process.env.XDG_CONFIG_HOME) return path.join(process.env.XDG_CONFIG_HOME, 'keel');
  return path.join(os.homedir(), '.config', 'keel');
}

/** Runs a long command, streaming its output while also capturing it (stdout + stderr). */
function runCmdTee(bin: string, args: string[], env?: NodeJS.ProcessEnv): Promise<string> {
  return new Promise((resolve, reject) => {
    const child = spawn(bin, args, {
      env: { ...process.env, ...env },
      stdio: ['ignore', 'pipe', 'pipe'],
    });
    let out = '';
    child.stdout.on('data', (d: Buffer) => {
      out += d.toString();
      process.stdout.write(d);
    });
    child.stderr.on('data', (d: Buffer) => {
      out += d.toString();
      process.stderr.write(d);
    });
    child.on('error', reject);
    child.on('close', (code) =>
      code === 0 ? resolve(out) : reject(new Error(`${bin} ${args[0]} exited with ${code}`)),
    );
  });
}

/** Runs a command and returns exit status plus combined stdout/stderr (never throws). */
function runCmdAll(
  bin: string,
  args: string[],
  env?: NodeJS.ProcessEnv,
): { ok: boolean; out: string } {
  try {
    const out = execFileSync(bin, args, {
      encoding: 'utf-8',
      env: { ...process.env, ...env },
      stdio: ['ignore', 'pipe', 'pipe'],
    });
    return { ok: true, out };
  } catch (err) {
    const e = err as { stdout?: string; stderr?: string };
    return { ok: false, out: `${e.stdout ?? ''}${e.stderr ?? ''}` };
  }
}

/** Runs a command with `input` on stdin; returns exit status plus combined output (never throws). */
function runCmdStdin(bin: string, args: string[], input: string): { ok: boolean; out: string } {
  try {
    const out = execFileSync(bin, args, {
      encoding: 'utf-8',
      input,
      stdio: ['pipe', 'pipe', 'pipe'],
    });
    return { ok: true, out };
  } catch (err) {
    const e = err as { stdout?: string; stderr?: string };
    return { ok: false, out: `${e.stdout ?? ''}${e.stderr ?? ''}` };
  }
}

/** Decoded value of `key` in Secret `ns/name` (never logged by callers); '' when absent. */
function secretValue(ns: string, name: string, key: string): string {
  const res = tryCmd('kubectl', [
    'get',
    'secret',
    name,
    '-n',
    ns,
    '-o',
    `jsonpath={.data.${key.replace(/\./g, '\\.')}}`,
  ]);
  return res.ok ? Buffer.from(res.stdout, 'base64').toString('utf-8') : '';
}

/** Basic-auth Git identity, handed to git through GIT_ASKPASS (never on a command line). */
interface GitBasicAuth {
  username: string;
  password: string;
}

/** The forge admin identity (`keel-system/forge-token`, the platform repository owner). */
function forgeAdminAuth(): GitBasicAuth {
  return { username: 'keel', password: secretValue('keel-system', 'forge-token', 'token').trim() };
}

/** The read-only credential of `tenant-<id>` (`keel-gitops-credentials`). */
function tenantRepoAuth(tenant: string): GitBasicAuth {
  const ns = `tenant-${tenant}`;
  return {
    username: secretValue(ns, 'keel-gitops-credentials', 'username'),
    password: secretValue(ns, 'keel-gitops-credentials', 'password'),
  };
}

/** Public Keel CA (`keel-system/keel-ca-bundle`) written to a temp file for host-side git. */
function keelCaFile(): string {
  const file = path.join(os.tmpdir(), 'keel-e2e-keel-ca.crt');
  const ca = runCmd('kubectl', [
    'get',
    'configmap',
    'keel-ca-bundle',
    '-n',
    'keel-system',
    '-o',
    'jsonpath={.data.ca\\.crt}',
  ]);
  fs.writeFileSync(file, `${ca}\n`);
  return file;
}

/**
 * Runs `git <args>` against the forge as `auth` (askpass script in a private temp dir, removed
 * afterwards). Returns exit status and combined output; git never prints the password.
 */
function gitAs(auth: GitBasicAuth, args: string[], caFile: string): { ok: boolean; out: string } {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'keel-e2e-askpass-'));
  const askpass = path.join(dir, 'askpass.sh');
  fs.writeFileSync(
    askpass,
    '#!/bin/sh\ncase "$1" in\n  *Username*) printf "%s\\n" "$KEEL_E2E_GIT_USER" ;;\n  *) printf "%s\\n" "$KEEL_E2E_GIT_PASS" ;;\nesac\n',
    { mode: 0o700 },
  );
  try {
    return runCmdAll(
      'git',
      ['-c', `http.sslCAInfo=${caFile}`, '-c', 'credential.helper=', ...args],
      {
        GIT_ASKPASS: askpass,
        GIT_TERMINAL_PROMPT: '0',
        KEEL_E2E_GIT_USER: auth.username,
        KEEL_E2E_GIT_PASS: auth.password,
      },
    );
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
}

/** All files (repo-relative, sorted) under `dir` of a working tree, ignoring `.git`. */
function listFiles(root: string, dir = ''): string[] {
  const abs = path.join(root, dir);
  if (!fs.existsSync(abs)) return [];
  const out: string[] = [];
  for (const entry of fs.readdirSync(abs, { withFileTypes: true })) {
    if (entry.name === '.git') continue;
    const rel = dir ? `${dir}/${entry.name}` : entry.name;
    if (entry.isDirectory()) out.push(...listFiles(root, rel));
    else out.push(rel);
  }
  return out.sort();
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
  kind: 'helmrelease' | 'stack' | 'kustomization' | 'gitrepository',
  name: string,
  namespace: string,
  timeoutMs = 300_000,
): Promise<void> {
  const start = Date.now();
  let lastLog = 0;
  try {
    await expect
      .poll(
        () => {
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
          const now = Date.now();
          if (now - lastLog >= 15_000) {
            const elapsed = Math.round((now - start) / 1000);
            const maxSec = Math.round(timeoutMs / 1000);
            const detail =
              reason && message ? `${reason}: ${message}` : reason || message || 'pending';
            logStep(
              `Waiting: ${kind}/${name} (${namespace}) Ready=${status || 'Unknown'} (${detail}) [${elapsed}s/${maxSec}s]`,
            );
            lastLog = now;
          }
          return res.ok ? status : '';
        },
        { timeout: timeoutMs, intervals: [5_000] },
      )
      .toBe('True');
    logStep(`Ready: ${kind}/${name} (${namespace}) in ${Math.round((Date.now() - start) / 1000)}s`);
    return;
  } catch {
    // fall through to diagnostics below
  }
  const desc = tryCmd('kubectl', ['describe', kind, name, '-n', namespace]);
  if (desc.stdout) {
    logStep(`${kind}/${name} (${namespace}) describe at timeout:\n${desc.stdout}`);
  }
  if (kind === 'stack') {
    const pods = tryCmd('kubectl', ['get', 'pods', '-A', '-o', 'wide']);
    if (pods.stdout) {
      logStep(`Cluster pods at timeout:\n${pods.stdout}`);
    }
    const pkoLogs = tryCmd('kubectl', [
      'logs',
      '-n',
      'flux-system',
      '-l',
      'app.kubernetes.io/name=pulumi-kubernetes-operator',
      '--tail=80',
    ]);
    if (pkoLogs.stdout) {
      logStep(`PKO controller logs at timeout:\n${pkoLogs.stdout}`);
    }
    const stackPods = tryCmd('kubectl', [
      'get',
      'pods',
      '-n',
      namespace,
      '-o',
      'jsonpath={.items[*].metadata.name}',
    ]);
    if (stackPods.ok && stackPods.stdout) {
      for (const podName of stackPods.stdout.split(/\s+/).filter(Boolean)) {
        const podLogs = tryCmd('kubectl', [
          'logs',
          '-n',
          namespace,
          podName,
          '--all-containers',
          '--tail=80',
        ]);
        if (podLogs.stdout) {
          logStep(`Pod ${namespace}/${podName} logs at timeout:\n${podLogs.stdout}`);
        }
      }
    }
    const containers = tryCmd('docker', ['ps', '-a']);
    if (containers.stdout) {
      logStep(`Docker containers at timeout:\n${containers.stdout}`);
    }
  }
  throw new Error(`Timed out waiting for ${kind}/${name} in ${namespace} to become Ready=True`);
}

async function waitForNamespace(name: string, timeoutMs = 120_000): Promise<void> {
  try {
    await expect
      .poll(
        () =>
          tryCmd('kubectl', ['get', 'namespace', name, '-o', 'jsonpath={.status.phase}']).stdout,
        { timeout: timeoutMs, intervals: [3_000] },
      )
      .toBe('Active');
    logStep(`Namespace ${name} is Active`);
    return;
  } catch {
    // fall through to diagnostics below
  }
  const fluxDiag = tryCmd('kubectl', [
    'get',
    'gitrepository,kustomization,helmrelease,stack',
    '-A',
  ]);
  if (fluxDiag.stdout) {
    logStep(`Flux/PKO resources at timeout:\n${fluxDiag.stdout}`);
  }
  const gitRepoDesc = tryCmd('kubectl', [
    'get',
    'gitrepository',
    'flux-system',
    '-n',
    'flux-system',
    '-o',
    'jsonpath={.status.conditions}',
  ]);
  if (gitRepoDesc.stdout) {
    logStep(`flux-system GitRepository conditions: ${gitRepoDesc.stdout}`);
  }
  const ksDesc = tryCmd('kubectl', [
    'get',
    'kustomization',
    'keel-tenants',
    '-n',
    'flux-system',
    '-o',
    'jsonpath={.status.conditions}',
  ]);
  if (ksDesc.stdout) {
    logStep(`keel-tenants Kustomization conditions: ${ksDesc.stdout}`);
  }
  throw new Error(`Timed out waiting for namespace/${name} to become Active`);
}

async function installDemoCursorOverlay(page: import('@playwright/test').Page): Promise<void> {
  await page.addInitScript(() => {
    const init = () => {
      if (document.getElementById('keel-demo-cursor')) return;
      const cursor = document.createElement('div');
      cursor.id = 'keel-demo-cursor';
      cursor.style.cssText = [
        'position:fixed',
        'left:-100px',
        'top:-100px',
        'width:24px',
        'height:24px',
        'pointer-events:none',
        'z-index:2147483647',
        'transform:translate(-2px,-2px)',
        'transition:transform 60ms ease-out',
      ].join(';');
      cursor.innerHTML =
        '<svg viewBox="0 0 24 24" width="24" height="24" fill="none" xmlns="http://www.w3.org/2000/svg">' +
        '<path d="M4 2L20 12L12.5 14L9.5 21L4 2Z" fill="#ffffff" stroke="#0f172a" stroke-width="1.8" stroke-linejoin="round"/>' +
        '</svg>';
      document.documentElement.appendChild(cursor);

      const move = (x: number, y: number) => {
        cursor.style.left = `${x}px`;
        cursor.style.top = `${y}px`;
      };

      window.addEventListener(
        'mousemove',
        (e) => {
          move(e.clientX, e.clientY);
        },
        { capture: true, passive: true },
      );

      window.addEventListener(
        'mousedown',
        (e) => {
          move(e.clientX, e.clientY);
          cursor.style.transform = 'translate(-2px,-2px) scale(0.88)';
          const ripple = document.createElement('div');
          ripple.style.cssText = [
            'position:fixed',
            `left:${e.clientX}px`,
            `top:${e.clientY}px`,
            'width:14px',
            'height:14px',
            'border-radius:9999px',
            'border:2.5px solid #38bdf8',
            'background:rgba(56,189,248,0.25)',
            'pointer-events:none',
            'z-index:2147483646',
            'transform:translate(-50%,-50%) scale(0.6)',
            'opacity:1',
            'transition:transform 380ms ease-out, opacity 380ms ease-out',
          ].join(';');
          document.documentElement.appendChild(ripple);
          requestAnimationFrame(() => {
            ripple.style.transform = 'translate(-50%,-50%) scale(2.4)';
            ripple.style.opacity = '0';
          });
          setTimeout(() => ripple.remove(), 420);
        },
        { capture: true, passive: true },
      );

      window.addEventListener(
        'mouseup',
        () => {
          cursor.style.transform = 'translate(-2px,-2px) scale(1)';
        },
        { capture: true, passive: true },
      );
    };

    if (document.documentElement) {
      init();
    } else {
      window.addEventListener('DOMContentLoaded', init, { once: true });
    }
  });
}

async function signInViaRauthy(
  page: import('@playwright/test').Page,
  email: string,
  password: string,
): Promise<void> {
  await page.goto('/');
  await expect(page.getByText('Keel Control Plane')).toBeVisible();

  const authRequired = page.getByText('Authentication Required');
  const logout = page.getByRole('button', { name: 'Logout' });
  await expect(authRequired.or(logout)).toBeVisible({ timeout: 30_000 });

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

  await expect(page.getByRole('button', { name: 'Logout' })).toBeVisible({ timeout: 30_000 });
}

/**
 * Drives `keel auth login --no-browser`: reads the printed `/api/v1/auth/cli/login` URL, opens it
 * in a Playwright page (signing in to Rauthy when prompted), and waits for the CLI to exit 0 after
 * the loopback callback + code exchange.
 */
async function cliLoopbackLogin(
  context: import('@playwright/test').BrowserContext,
  env: NodeJS.ProcessEnv,
  email: string,
  password: string,
): Promise<void> {
  const child = spawn(KEEL_BIN, ['auth', 'login', '--no-browser', '--timeout-secs', '120'], {
    env: { ...process.env, ...env },
    stdio: ['ignore', 'pipe', 'pipe'],
  });
  let output = '';
  child.stdout.on('data', (d: Buffer) => (output += d.toString()));
  child.stderr.on('data', (d: Buffer) => (output += d.toString()));
  const exited = new Promise<number | null>((resolve) => child.on('close', resolve));
  try {
    await expect
      .poll(() => /https?:\/\/\S+\/api\/v1\/auth\/cli\/login\S*/.exec(output)?.[0] ?? '', {
        timeout: 15_000,
        message: `keel auth login printed no login URL:\n${output}`,
      })
      .not.toBe('');
    const loginUrl = /https?:\/\/\S+\/api\/v1\/auth\/cli\/login\S*/.exec(output)![0];
    const loginPage = await context.newPage();
    // The CLI's RFC 8252 loopback listener (`http://127.0.0.1:<port>/callback`).
    const loopbackCallback = /^http:\/\/(127\.0\.0\.1|\[::1\]):\d+\/callback/;
    try {
      await loginPage.goto(loginUrl, { waitUntil: 'domcontentloaded' }).catch(() => undefined);
      // With an existing Rauthy session (the GUI already signed in with this browser context)
      // the IdP completes the flow on its own and redirects straight to the loopback callback;
      // Rauthy may briefly render its login form before redirecting, so wait for the callback
      // first and only drive the form when we are still on the IdP afterwards.
      const autoCompleted = await loginPage
        .waitForURL(loopbackCallback, { timeout: 8_000 })
        .then(() => true)
        .catch(() => false);
      if (!autoCompleted && !loopbackCallback.test(loginPage.url())) {
        const emailInput = loginPage
          .locator('input[type="email"], input[name="email"], input[autocomplete="username"]')
          .first();
        await emailInput.fill(email, { timeout: 15_000 });
        const passwordInput = loginPage
          .locator('input[type="password"], input[name="password"]')
          .first();
        if (!(await passwordInput.isVisible({ timeout: 2_000 }).catch(() => false))) {
          await loginPage.locator('button[type="submit"]').first().click({ timeout: 10_000 });
        }
        if (!loopbackCallback.test(loginPage.url())) {
          await passwordInput.fill(password, { timeout: 15_000 });
          await loginPage
            .locator('button[type="submit"], #submit-login')
            .first()
            .click({ timeout: 10_000 });
        }
        await loginPage.waitForURL(loopbackCallback, { timeout: 30_000 });
      }
      const code = await Promise.race([
        exited,
        new Promise<'timeout'>((r) => setTimeout(() => r('timeout'), 90_000)),
      ]);
      if (code !== 0) throw new Error(`keel auth login exited with ${code}:\n${output}`);
    } finally {
      await loginPage.close();
    }
  } finally {
    if (child.exitCode === null) child.kill();
  }
}

/**
 * Admin credentials: `KEEL_ADMIN_PASSWORD` / `KEEL_ADMIN_USERNAME` env overrides, else the files the
 * justfile writes after bootstrap (`target/keel-admin-password`, `target/keel-admin-username`) when
 * reusing a cluster, else the deterministic password passed to a fresh `bootstrap init` below.
 */
function readTargetFile(name: string): string {
  const p = path.resolve(process.cwd(), '../target', name);
  return fs.existsSync(p) ? fs.readFileSync(p, 'utf-8').trim() : '';
}

function resolveE2eAdminPassword(): string {
  if (process.env.KEEL_ADMIN_PASSWORD?.trim()) {
    return process.env.KEEL_ADMIN_PASSWORD.trim();
  }
  if (process.env.KEEL_E2E_REUSE_CLUSTER !== '0') {
    const saved = readTargetFile('keel-admin-password');
    if (saved) return saved;
  }
  return 'KeelE2eAdminPass123!';
}

function resolveE2eAdminUsername(): string {
  return (
    process.env.KEEL_ADMIN_USERNAME?.trim() ||
    (process.env.KEEL_E2E_REUSE_CLUSTER !== '0' ? readTargetFile('keel-admin-username') : '') ||
    'admin@keel.local'
  );
}

// Note: `keel bootstrap init --target kind` provisions the `kind` management cluster
// via `deploy/bootstrap/kind/Pulumi.yaml` (`tehcyx/kind`) instead of direct `kind create cluster`.
test.describe('07 - Live kind Management + Workload Cluster E2E (GUI + CLI)', () => {
  test.describe.configure({ mode: 'serial' });
  test.skip(
    !LIVE_ENABLED,
    'Set KEEL_E2E_LIVE=1 (or run `pnpm --dir e2e test:live`) to run live kind E2E',
  );

  let createdMgmtCluster = false;
  let serveProc: ChildProcess | null = null;
  let tmpRepoDir: string | null = null;
  const adminPassword = resolveE2eAdminPassword();
  const adminUsername = resolveE2eAdminUsername();
  /** Captured `keel bootstrap init` output (fresh bootstrap only). */
  let bootstrapOutput = '';
  /** Isolated CLI config (credentials.json) shared by the serial CLI steps. */
  let cliEnv: NodeJS.ProcessEnv = {};
  const acmePassword = process.env.KEEL_ACME_PASSWORD || 'KeelE2eAcmePass123!';

  test.beforeAll(async () => {
    const clusters = tryCmd('kind', ['get', 'clusters']).stdout.split(/\r?\n/).filter(Boolean);
    const hasExistingMgmt = clusters.includes(MGMT_CLUSTER);

    if (!hasExistingMgmt || process.env.KEEL_E2E_REUSE_CLUSTER === '0') {
      if (hasExistingMgmt) {
        logStep(`Deleting existing kind cluster "${MGMT_CLUSTER}" (KEEL_E2E_REUSE_CLUSTER=0)...`);
        runCmd('kind', ['delete', 'cluster', '--name', MGMT_CLUSTER], undefined, true);
      }
      if (clusters.includes('workload-dev')) {
        logStep('Deleting stale workload kind cluster "workload-dev"...');
        runCmd('kind', ['delete', 'cluster', '--name', 'workload-dev'], undefined, true);
      }
      for (const stale of [WORKLOAD_CLUSTER, ACME_CLUSTER]) {
        if (clusters.includes(stale)) {
          logStep(`Deleting stale workload kind cluster "${stale}"...`);
          runCmd('kind', ['delete', 'cluster', '--name', stale], undefined, true);
        }
      }
      // Stage 0 Pulumi state lives in <config>/stage0/<cluster>/ (escrowed to S3); stale local
      // state for a deleted cluster would make a fresh bootstrap believe resources still exist.
      fs.rmSync(path.join(keelConfigDir(), 'stage0', MGMT_CLUSTER), {
        recursive: true,
        force: true,
      });
      if (fs.existsSync(DETERMINISTIC_REPO_DIR)) {
        fs.rmSync(DETERMINISTIC_REPO_DIR, { recursive: true, force: true });
      }
      if (fs.existsSync(`${DETERMINISTIC_REPO_DIR}.git`)) {
        fs.rmSync(`${DETERMINISTIC_REPO_DIR}.git`, { recursive: true, force: true });
      }
      fs.mkdirSync(DETERMINISTIC_REPO_DIR, { recursive: true });
      tmpRepoDir = DETERMINISTIC_REPO_DIR;
      createdMgmtCluster = true;
      logStep(
        `Bootstrapping fresh kind management cluster "${MGMT_CLUSTER}" via deploy/bootstrap/kind/Pulumi.yaml (repo: ${tmpRepoDir})...`,
      );
      bootstrapOutput = await runCmdTee(KEEL_BIN, [
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
      ]);
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
            'chore(e2e): seed full catalog blueprints and bundles',
          ],
          { GIT_EDITOR: 'true' },
        );
      }
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
      await expect
        .poll(() => isServerUp(BASE_URL), {
          timeout: 30_000,
          intervals: [1_000],
          message: `keel serve did not become ready at ${BASE_URL}; check ${SERVE_LOG_PATH}`,
        })
        .toBe(true);
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

    const clustersToClean = [
      WORKLOAD_CLUSTER,
      ACME_CLUSTER,
      ...(createdMgmtCluster ? [MGMT_CLUSTER] : []),
    ];
    if (process.env.CI) {
      for (const name of clustersToClean) {
        tryCmd('kind', ['delete', 'cluster', '--name', name]);
      }
      if (tmpRepoDir) {
        fs.rmSync(tmpRepoDir, { recursive: true, force: true });
        fs.rmSync(`${tmpRepoDir}.git`, { recursive: true, force: true });
      }
    } else {
      console.log(
        `tests complete - clean up using: kind delete cluster --name ${clustersToClean.join(' ')}`,
      );
    }
  });

  test('07.0 - Stage 0 hardening: state escrow, passphrase secret, Kyverno scope, break-glass plan', async () => {
    await test.step('1. Stage 0 state is escrowed to the in-cluster versioned S3 bucket', async () => {
      if (bootstrapOutput) {
        expect(bootstrapOutput).toMatch(
          new RegExp(`Stage 0 state escrowed to \\S*stage0/${MGMT_CLUSTER}`),
        );
      } else {
        logStep('Reused cluster: bootstrap output not captured; skipping escrow-summary check');
      }
      const passphrase = tryCmd('kubectl', [
        'get',
        'secret',
        'keel-stage0-pulumi-passphrase',
        '-n',
        'keel-system',
        '-o',
        'jsonpath={.metadata.name}',
      ]);
      expect(passphrase.ok, 'keel-system/keel-stage0-pulumi-passphrase must exist').toBe(true);
      expect(passphrase.stdout).toBe('keel-stage0-pulumi-passphrase');
      // Forgejo admin password is random and stored in-cluster (never a fixed default).
      const forgejoAdmin = tryCmd('kubectl', [
        'get',
        'secret',
        'keel-forgejo-admin',
        '-n',
        'keel-system',
        '-o',
        'jsonpath={.data.password}',
      ]);
      expect(forgejoAdmin.ok, 'keel-system/keel-forgejo-admin must exist').toBe(true);
      expect(Buffer.from(forgejoAdmin.stdout, 'base64').toString('utf-8').length).toBeGreaterThan(
        11,
      );
    });

    await test.step('2. Kyverno is installed only in keel-system', async () => {
      const pods = tryCmd('kubectl', [
        'get',
        'pods',
        '-A',
        '-o',
        'jsonpath={range .items[*]}{.metadata.namespace}/{.metadata.name}{"\\n"}{end}',
      ]).stdout.split('\n');
      const kyvernoNamespaces = new Set(
        pods.filter((p) => p.includes('kyverno')).map((p) => p.split('/')[0]),
      );
      expect([...kyvernoNamespaces]).toEqual(['keel-system']);
      // Core add-on HelmReleases live in flux-system (impersonating helm-controller) but must
      // install into, and store Helm release state in, keel-system.
      const releases = tryCmd('kubectl', [
        'get',
        'helmrelease',
        '-A',
        '-o',
        'jsonpath={range .items[*]}{.metadata.namespace}/{.metadata.name}/{.spec.targetNamespace}/{.spec.storageNamespace}{"\\n"}{end}',
      ]).stdout.split('\n');
      const kyvernoReleases = releases.filter((x) => /kyverno/.test(x));
      expect(kyvernoReleases, 'exactly one Kyverno HelmRelease').toHaveLength(1);
      expect(kyvernoReleases[0]).toBe('flux-system/kyverno/keel-system/keel-system');
    });

    await test.step('3. `keel bootstrap recover --dry-run` prints the break-glass plan', async () => {
      const recover = runCmdAll(KEEL_BIN, [
        'bootstrap',
        'recover',
        '--s3-endpoint',
        'http://127.0.0.1:8333',
        '--cluster-name',
        MGMT_CLUSTER,
        '--dry-run',
      ]);
      expect(recover.ok, recover.out).toBe(true);
      expect(recover.out).toContain('Break-Glass Stage 0 Recovery');
      expect(recover.out).toContain(`stage0/${MGMT_CLUSTER}/`);
      expect(recover.out).toMatch(/Plan \(dry run/);
      expect(recover.out).toContain('pulumi');
    });
  });

  test('07.1 - CLI standalone app on mgmt -> GUI bundle workload kind cluster -> GUI standalone canary app on workload cluster', async ({
    page,
  }) => {
    if (RECORD_DEMO) {
      await installDemoCursorOverlay(page);
    }

    await test.step('1. Authenticate in GUI via Rauthy OIDC', async () => {
      logStep('Opening GUI and signing in via Rauthy OIDC...');
      await page.goto('/');
      await expect(page.getByText('Keel Control Plane')).toBeVisible();
      await signInViaRauthy(page, adminUsername, adminPassword);
      // Cluster admins see the management plane strip (rendered only from a real API stack).
      await expect(page.getByTestId('management-plane-strip')).toBeVisible({ timeout: 30_000 });
      logStep('Authenticated; management plane strip is visible');
    });

    await test.step('1b. CLI: `keel auth login` via RFC 8252 loopback, driven in the browser', async () => {
      const cliHome = fs.mkdtempSync(path.join(os.tmpdir(), 'keel-e2e-cli-home-'));
      const configDir = path.join(cliHome, '.config', 'keel');
      fs.mkdirSync(configDir, { recursive: true });
      cliEnv = { HOME: cliHome, KEEL_CONFIG_DIR: configDir, KEEL_SERVER: BASE_URL };

      let loggedIn = false;
      try {
        await cliLoopbackLogin(page.context(), cliEnv, adminUsername, adminPassword);
        const status = runCmdAll(KEEL_BIN, ['auth', 'status'], cliEnv);
        expect(status.ok, status.out).toBe(true);
        expect(status.out).toContain(adminUsername);
        loggedIn = true;
        logStep('CLI authenticated via loopback login');
      } catch (err) {
        logStep(`CLI loopback login failed (${String(err)}); falling back to GUI session cookie`);
      }

      if (!loggedIn) {
        // Fallback: reuse the GUI session cookie as the CLI Bearer token.
        const cookies = await page.context().cookies(BASE_URL);
        const sessionToken = cookies.find((c) => c.name === 'keel_session')?.value || '';
        expect(sessionToken.length).toBeGreaterThan(0);
        fs.writeFileSync(
          path.join(configDir, 'credentials.json'),
          JSON.stringify({ sessions: { [BASE_URL.replace(/\/+$/, '')]: sessionToken } }, null, 2),
          { mode: 0o600 },
        );
      }
    });

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
          ['preview', 'status', '--pr', prMatch[1], '--tenant', 'default', '--server', BASE_URL],
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
    });

    await test.step('3. GUI Bundle: Submit & merge workload kind cluster + addons PR (kind-canary-platform)', async () => {
      const existingCluster = tryCmd('kubectl', [
        'get',
        'stack',
        `${WORKLOAD_CLUSTER}-cluster`,
        '-n',
        'tenant-default',
      ]);
      if (existingCluster.ok) {
        logStep(`Bundle "${WORKLOAD_CLUSTER}" already exists; skipping PR creation...`);
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
    });

    await test.step('4. Admin: Create tenant acme (API) and seed Rauthy group/user acme@keel.local', async () => {
      // `POST /api/v1/tenants` is idempotent: a re-run against a reused cluster resumes or
      // reports the provisioned tenant.
      logStep('Creating tenant "acme" via POST /api/v1/tenants (dedicated repository)...');
      const created = await page.request.post('/api/v1/tenants', {
        headers: SAME_ORIGIN,
        data: { id: 'acme' },
      });
      const createdText = await created.text();
      expect([200, 201], createdText).toContain(created.status());
      const tenantBody = JSON.parse(createdText) as CreateTenantBody;
      const repo = tenantBody.repository;
      expect(repo.layout, createdText).toBe('dedicated');
      expect(repo.owner, createdText).toBe(TENANT_REPOS_ORG);
      expect(repo.name, createdText).toMatch(/-tenant-acme$/);
      expect(repo.transport, createdText).toBe('https');
      // Never a credential in the response.
      expect(createdText).not.toMatch(/password|token"|BEGIN .*PRIVATE KEY/i);

      // The tenant repository PR (skeleton) merges first, then the platform stub PR.
      if (repo.skeletonProposal) {
        expect(repo.skeletonProposal.scope).toBe('acme');
        expect(repo.skeletonProposal.repository.fullName).toBe(`${repo.owner}/${repo.name}`);
        const merged = await page.request.post(
          `/api/v1/tenants/acme/proposals/${repo.skeletonProposal.id}/merge`,
          { headers: SAME_ORIGIN },
        );
        expect(merged.status(), await merged.text()).toBe(200);
        logStep(`Merged acme repository skeleton PR #${repo.skeletonProposal.id}`);
      }
      if (tenantBody.proposal) {
        expect(tenantBody.proposal.scope).toBe('management');
        expect(tenantBody.proposal.repository.label).toBe('platform');
        const merged = await page.request.post(
          `/api/v1/tenants/management/proposals/${tenantBody.proposal.id}/merge`,
          { headers: SAME_ORIGIN },
        );
        expect(merged.status(), await merged.text()).toBe(200);
        logStep(`Merged platform stub PR #${tenantBody.proposal.id} for tenant acme`);
      }

      logStep('Seeding Rauthy group "tenant-acme" and non-admin user "acme@keel.local"...');
      const rauthyPage = await page.context().newPage();
      let capturedCsrf = '';
      let capturedAuth = '';
      rauthyPage.on('request', (req) => {
        const hdrs = req.headers();
        if (hdrs['x-csrf-token']) capturedCsrf = hdrs['x-csrf-token'];
        if (hdrs['authorization']) capturedAuth = hdrs['authorization'];
      });
      try {
        await rauthyPage.goto('https://idp.keel.local:8888/auth/v1/admin', {
          waitUntil: 'domcontentloaded',
          timeout: 15_000,
        });
        const emailInput = rauthyPage
          .locator('input[type="email"], input[name="email"], input[autocomplete="username"]')
          .first();
        if (await emailInput.isVisible({ timeout: 3_000 }).catch(() => false)) {
          await emailInput.fill(adminUsername);
          const passwordInput = rauthyPage
            .locator('input[type="password"], input[name="password"]')
            .first();
          if (!(await passwordInput.isVisible({ timeout: 1_000 }).catch(() => false))) {
            await rauthyPage.locator('button[type="submit"]').first().click({ timeout: 10_000 });
          }
          await expect(passwordInput).toBeVisible({ timeout: 10_000 });
          await passwordInput.fill(adminPassword);
          await rauthyPage.locator('button[type="submit"]').first().click({ timeout: 10_000 });
        }
        await rauthyPage.waitForURL(/\/auth\/v1\/admin/, { timeout: 15_000 });
        logStep('Rauthy admin UI loaded; navigating to Groups...');

        const groupsNav = rauthyPage.getByRole('link', { name: /^Groups$/i }).first();
        await expect(groupsNav).toBeVisible({ timeout: 15_000 });
        await groupsNav.click({ timeout: 10_000 });

        for (const groupName of ['tenant-acme', 'tenant-acme-admin']) {
          if (
            !(await rauthyPage
              .getByText(groupName, { exact: true })
              .isVisible({ timeout: 2_000 })
              .catch(() => false))
          ) {
            if (
              await rauthyPage
                .locator('dialog[open]')
                .first()
                .isVisible({ timeout: 500 })
                .catch(() => false)
            ) {
              await rauthyPage.keyboard.press('Escape');
              await expect(rauthyPage.locator('dialog[open]')).toHaveCount(0, {
                timeout: 5_000,
              });
            }
            const addGroupBtn = rauthyPage.getByRole('button', { name: /add/i }).first();
            await expect(addGroupBtn).toBeVisible({ timeout: 10_000 });
            await addGroupBtn.click({ timeout: 10_000 });
            const dialog = rauthyPage.locator('dialog[open]').first();
            await expect(dialog).toBeVisible({ timeout: 10_000 });
            const groupInput = dialog.locator('input').first();
            await expect(groupInput).toBeVisible({ timeout: 10_000 });
            await groupInput.fill(groupName);
            await dialog
              .locator('button[type="submit"], button')
              .filter({ hasText: /save|create|add/i })
              .first()
              .click({ timeout: 10_000 });
            await expect(rauthyPage.getByText(groupName, { exact: true }).first()).toBeVisible({
              timeout: 10_000,
            });
            if (
              await rauthyPage
                .locator('dialog[open]')
                .first()
                .isVisible({ timeout: 500 })
                .catch(() => false)
            ) {
              await rauthyPage.keyboard.press('Escape');
            }
          }
        }
        logStep(
          `Rauthy group tenant-acme ready (csrf=${Boolean(capturedCsrf)}, auth=${Boolean(capturedAuth)})`,
        );

        await rauthyPage.evaluate(
          async ({ pwd, preCapturedCsrf, preCapturedAuth }) => {
            const headers: Record<string, string> = {
              'content-type': 'application/json',
            };
            if (preCapturedCsrf) headers['x-csrf-token'] = preCapturedCsrf;
            if (preCapturedAuth) headers['authorization'] = preCapturedAuth;

            const uRes = await fetch('/auth/v1/users', { headers });
            if (!uRes.ok) throw new Error(`GET /auth/v1/users failed: ${uRes.status}`);
            const users: Array<{ id: string; email: string }> = await uRes.json();
            const usersToSeed = [
              {
                email: 'acme@keel.local',
                username: 'acme',
                given: 'Acme',
                groups: ['tenant-acme'],
              },
              {
                email: 'acme-admin@keel.local',
                username: 'acme-admin',
                given: 'AcmeAdmin',
                groups: ['tenant-acme-admin'],
              },
            ];
            for (const uSpec of usersToSeed) {
              if (!users.some((u) => u.email === uSpec.email)) {
                const createUserRes = await fetch('/auth/v1/users', {
                  method: 'POST',
                  headers,
                  body: JSON.stringify({
                    email: uSpec.email,
                    preferred_username: uSpec.username,
                    given_name: uSpec.given,
                    family_name: 'User',
                    language: 'en',
                    groups: uSpec.groups,
                    roles: [],
                  }),
                });
                if (!createUserRes.ok) {
                  throw new Error(
                    `POST /auth/v1/users (${uSpec.email}) failed: ${createUserRes.status} ${await createUserRes.text()}`,
                  );
                }
                const createdUser: { id: string } = await createUserRes.json();
                const updateUserRes = await fetch(`/auth/v1/users/${createdUser.id}`, {
                  method: 'PUT',
                  headers,
                  body: JSON.stringify({
                    email: uSpec.email,
                    given_name: uSpec.given,
                    family_name: 'User',
                    password: pwd,
                    roles: [],
                    groups: uSpec.groups,
                    enabled: true,
                    email_verified: true,
                  }),
                });
                if (!updateUserRes.ok) {
                  throw new Error(
                    `PUT /auth/v1/users/${createdUser.id} failed: ${updateUserRes.status} ${await updateUserRes.text()}`,
                  );
                }
              }
            }
          },
          { pwd: acmePassword, preCapturedCsrf: capturedCsrf, preCapturedAuth: capturedAuth },
        );
      } finally {
        await rauthyPage.close();
      }
      logStep('Rauthy group tenant-acme and user acme@keel.local ready');
    });

    await test.step('5. Wait for management podinfo, tenant-acme, and workload cluster bundle', async () => {
      await Promise.all([
        waitForK8sReady('helmrelease', 'podinfo', 'tenant-default', 360_000),
        waitForNamespace('tenant-acme', 120_000),
        waitForK8sReady('stack', `${WORKLOAD_CLUSTER}-cluster`, 'tenant-default', 480_000),
      ]);
      await Promise.all([
        waitForK8sReady('stack', `${WORKLOAD_CLUSTER}-canary-config`, 'tenant-default', 300_000),
        waitForK8sReady('helmrelease', `${WORKLOAD_CLUSTER}-flagger`, 'tenant-default', 300_000),
      ]);
    });

    await test.step('5b. Tenant Stacks clone their own Git source (PKO gitSource, no fluxSource)', async () => {
      // `<name>\t<projectRepo>\t<fluxSource name>` per tenant-default Stack.
      const stackSources = () =>
        tryCmd('kubectl', [
          'get',
          'stacks.pulumi.com',
          '-n',
          'tenant-default',
          '-o',
          'jsonpath={range .items[*]}{.metadata.name}{"\\t"}{.spec.projectRepo}{"\\t"}{.spec.fluxSource.sourceRef.name}{"\\n"}{end}',
        ])
          .stdout.split('\n')
          .filter((line) => line.trim() !== '')
          .map((line) => {
            const [name, projectRepo = '', fluxSource = ''] = line.split('\t');
            return { name, projectRepo, fluxSource };
          });
      const cluster = stackSources().find((s) => s.name === `${WORKLOAD_CLUSTER}-cluster`);
      expect(cluster, JSON.stringify(stackSources())).toBeTruthy();
      expect(cluster!.projectRepo).not.toBe('');
      // PKO rejects http:// clone URLs: the in-cluster Forgejo is cloned through its HTTPS
      // ingress origin (`forge.publicUrl`), Flux keeps the http Service URL.
      expect(cluster!.projectRepo).toMatch(/^https:\/\//);
      expect(cluster!.fluxSource).toBe('');
      expect(stackSources().filter((st) => st.fluxSource !== '')).toEqual([]);

      // Workspaces trust the Keel CA through the PUBLIC `keel-ca-bundle` ConfigMap only.
      for (const ns of ['keel-system', 'tenant-default', 'keel-previews']) {
        const ca = tryCmd('kubectl', [
          'get',
          'configmap',
          'keel-ca-bundle',
          '-n',
          ns,
          '-o',
          'jsonpath={.data.ca\\.crt}',
        ]).stdout;
        expect(ca, `keel-ca-bundle in ${ns}`).toContain('BEGIN CERTIFICATE');
        expect(ca, `keel-ca-bundle in ${ns}`).not.toContain('PRIVATE KEY');
      }
      const caVolume = tryCmd('kubectl', [
        'get',
        'stacks.pulumi.com',
        `${WORKLOAD_CLUSTER}-cluster`,
        '-n',
        'tenant-default',
        '-o',
        'jsonpath={.spec.workspaceTemplate.spec.podTemplate.spec.volumes[?(@.name=="keel-ca")].configMap.name}|{.spec.workspaceTemplate.spec.podTemplate.spec.initContainers[?(@.name=="fetch")].volumeMounts[0].mountPath}',
      ]).stdout;
      expect(caVolume).toBe('keel-ca-bundle|/etc/keel-ca');
    });

    const workloadAppName = `${WORKLOAD_CLUSTER}-podinfo`;
    await test.step('6. GUI Standalone App: Deploy canary podinfo onto workload cluster', async () => {
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

    await test.step('7. Verify both clusters & apps in Topology Map', async () => {
      logStep('Verifying clusters and apps in Topology Map...');
      await page.getByRole('button', { name: /Topology Map/i }).click();
      await page.getByRole('button', { name: /Refresh Map/i }).click();
      await expect(page.getByText(`${WORKLOAD_CLUSTER}-cluster`).first()).toBeVisible({
        timeout: 20_000,
      });
      await expect(page.getByText(workloadAppName).first()).toBeVisible({ timeout: 20_000 });
      await expect(page.getByTestId('topology-stream-status')).toHaveText(/SSE Live Stream/, {
        timeout: 20_000,
      });
      await page.getByText('Live Reconciler Event Stream').scrollIntoViewIfNeeded();
      await expect(page.getByText('Live Reconciler Event Stream')).toBeInViewport();
      if (RECORD_DEMO) {
        const video = page.video();
        await page.close();
        if (video) {
          fs.mkdirSync(path.dirname(DEMO_VIDEO_PATH), { recursive: true });
          await video.saveAs(DEMO_VIDEO_PATH);
          logStep(`Saved demo video to ${DEMO_VIDEO_PATH}`);
        }
      }
      logStep('Topology Map verification complete');
    });
  });

  test('07.2 - Non-admin user (acme) can use tenant-acme and is denied access to other tenants', async ({
    browser,
  }) => {
    const context = await browser.newContext({ ignoreHTTPSErrors: true });
    const page = await context.newPage();

    try {
      await test.step('1. Authenticate as non-admin acme@keel.local via Rauthy OIDC', async () => {
        logStep('Signing in as non-admin acme@keel.local...');
        await signInViaRauthy(page, 'acme@keel.local', acmePassword);
        logStep('Authenticated as acme@keel.local');
      });

      await test.step('2. Verify non-admin is denied access to tenant-default and admin actions', async () => {
        const stacksDenied = await page.request.get('/api/v1/stacks?tenant=default');
        expect(stacksDenied.status()).toBe(403);

        const releasesDenied = await page.request.get('/api/v1/releases?tenant=default');
        expect(releasesDenied.status()).toBe(403);

        const proposalDenied = await page.request.post('/api/v1/tenants/default/proposals', {
          headers: SAME_ORIGIN,
          data: {
            catalogId: 'podinfo',
            name: 'acme-denied-podinfo',
            tenant: 'default',
            values: { replicaCount: 1 },
          },
        });
        expect(proposalDenied.status()).toBe(403);
        // RBAC denial, not a CSRF rejection.
        expect((await proposalDenied.json()).error).toMatch(/Access denied for tenant 'default'/);

        const createTenantDenied = await page.request.post('/api/v1/tenants', {
          headers: SAME_ORIGIN,
          data: { id: 'acme-unauthorized' },
        });
        expect(createTenantDenied.status()).toBe(403);
        logStep('Confirmed 403 Forbidden for tenant-default and admin tenant creation');
      });

      await test.step('3. Verify non-admin can access and deploy into tenant-acme', async () => {
        const stacksAllowed = await page.request.get('/api/v1/stacks?tenant=acme');
        expect(stacksAllowed.status()).toBe(200);

        const releasesAllowed = await page.request.get('/api/v1/releases?tenant=acme');
        expect(releasesAllowed.status()).toBe(200);

        const acmeAppName = 'acme-podinfo';
        const existingApp = tryCmd('kubectl', [
          'get',
          'helmrelease',
          acmeAppName,
          '-n',
          'tenant-acme',
        ]);
        if (!existingApp.ok) {
          logStep('Deploying podinfo into tenant-acme as acme@keel.local...');
          await page.getByRole('button', { name: /Catalog & Form Wizard/i }).click();
          const tenantSelect = page.locator('#tenant-select-catalog');
          await expect(tenantSelect.locator('option[value="acme"]')).toHaveCount(1, {
            timeout: 15_000,
          });
          await tenantSelect.selectOption('acme');

          const podinfoCard = page
            .locator('[data-testid="catalog-list"] > div')
            .filter({ has: page.getByText('podinfo', { exact: true }) })
            .first();
          await expect(podinfoCard).toBeVisible({ timeout: 15_000 });
          await podinfoCard.getByRole('button', { name: 'Create' }).click();

          await page.locator('#instance-name-input').fill(acmeAppName);
          const apiTokenInput = page.getByLabel(/API Secret Token/i);
          if (await apiTokenInput.isVisible()) {
            await apiTokenInput.fill('e2e-acme-token-1234');
          }
          await page.getByRole('button', { name: /Submit & Open Git PR Proposal/i }).click();
          await expect(page.getByText(/Git PR Proposal Successfully Opened!/i)).toBeVisible({
            timeout: 20_000,
          });

          const prsRes = await page.request.get('/api/v1/proposals');
          expect(prsRes.status()).toBe(200);
          const prsJson = (await prsRes.json()) as {
            proposals: Array<{ id: string; title: string; scope: string }>;
          };
          const acmePr = prsJson.proposals.find((p) =>
            p.title.toLowerCase().includes(acmeAppName.toLowerCase()),
          );
          expect(acmePr).toBeTruthy();

          expect(acmePr!.scope).toBe('acme');
          const nonAdminMergeRes = await page.request.post(
            `/api/v1/tenants/acme/proposals/${acmePr!.id}/merge`,
            { headers: SAME_ORIGIN },
          );
          expect(nonAdminMergeRes.status()).toBe(403);
          logStep(`Confirmed non-admin cannot merge PR #${acmePr!.id} (403 Forbidden)`);

          const tenantAdminContext = await browser.newContext({ ignoreHTTPSErrors: true });
          const tenantAdminPage = await tenantAdminContext.newPage();
          try {
            await signInViaRauthy(tenantAdminPage, 'acme-admin@keel.local', acmePassword);
            const otherTenantDenied = await tenantAdminPage.request.get(
              '/api/v1/stacks?tenant=default',
            );
            expect(otherTenantDenied.status()).toBe(403);
            const createTenantDenied2 = await tenantAdminPage.request.post('/api/v1/tenants', {
              headers: SAME_ORIGIN,
              data: { id: 'gamma' },
            });
            expect(createTenantDenied2.status()).toBe(403);

            const tenantAdminMergeRes = await tenantAdminPage.request.post(
              `/api/v1/tenants/acme/proposals/${acmePr!.id}/merge`,
              { headers: SAME_ORIGIN },
            );
            expect(tenantAdminMergeRes.status()).toBe(200);
            logStep(
              `Tenant admin acme-admin@keel.local merged scoped PR #${acmePr!.id} for tenant-acme`,
            );
          } finally {
            await tenantAdminContext.close();
          }
        }

        await waitForK8sReady('helmrelease', acmeAppName, 'tenant-acme', 360_000);
        logStep('Non-admin acme-podinfo reconciled cleanly in tenant-acme');
      });
    } finally {
      await context.close();
    }
  });

  test('07.3 - Infra proposal runs a real PKO preview Stack in keel-previews; CLI proposal list/show/close GCs it', async ({
    page,
  }) => {
    await signInViaRauthy(page, adminUsername, adminPassword);
    const instance = `e2e-preview-${Date.now().toString(36)}`;
    let prId = '';
    /** Repository-scoped preview key (`keel_pko::preview_key`): PR numbers are per repository. */
    let previewKey = '';
    let headSha = '';

    await test.step('1. Create an infra (Pulumi-backed) proposal', async () => {
      const res = await page.request.post('/api/v1/tenants/default/proposals', {
        headers: SAME_ORIGIN,
        data: {
          tenant: 'default',
          catalogId: 'kind-cluster',
          kind: 'infra',
          name: instance,
          values: { clusterName: instance, kubernetesVersion: 'v1.35.8', workerCount: 0 },
        },
      });
      expect(res.status(), await res.text()).toBe(201);
      const body = await res.json();
      prId = body.proposal.id;
      previewKey = `default-${prId}`;
      headSha = body.proposal.headSha;
      // tenant-default proposals live in its own repository (split at bootstrap).
      expect(body.proposal.scope).toBe('default');
      expect(body.proposal.repository.label).toBe('tenant-default');
      expect(body.proposal.repository.fullName).toMatch(
        new RegExp(`^${TENANT_REPOS_ORG}/.+-tenant-default$`),
      );
      expect(body.proposal.title).toMatch(new RegExp(`^feat\\(default\\): \\w+ ${instance}$`));
      // With a live cluster the PKO preview is launched asynchronously: the create response
      // carries an honest provisional (synthesized) diff while the run is pending/running, or the
      // PKO result if it already finished. It must never claim "notExecuted" here.
      expect(['pko', 'synthesized']).toContain(body.report.source);
      expect(['pending', 'running', 'succeeded']).toContain(body.report.run?.phase);
      logStep(`Infra proposal PR #${prId} created (preview phase ${body.report.run?.phase})`);
    });

    await test.step('1b. Preview report converges to the real PKO result', async () => {
      await expect
        .poll(
          async () => {
            const r = await page.request.get(`/api/v1/tenants/default/proposals/${prId}/preview`);
            if (!r.ok()) return `http ${r.status()}`;
            const b = await r.json();
            const report = b.report ?? b;
            return `${report.source}/${report.run?.phase}`;
          },
          { timeout: 300_000, intervals: [5_000] },
        )
        .toBe('pko/succeeded');
      logStep(`PR #${prId} preview executed by PKO and succeeded`);
    });

    const previewStacks = () =>
      tryCmd('kubectl', [
        'get',
        'stacks.pulumi.com',
        '-n',
        'keel-previews',
        '-l',
        `keel.io/pr-id=${previewKey}`,
        '-o',
        'jsonpath={.items[*].metadata.name}',
      ]).stdout.trim();

    await test.step('2. Ephemeral preview Stack (spec.preview: true) appears in keel-previews', async () => {
      await expect.poll(previewStacks, { timeout: 120_000, intervals: [3_000] }).not.toBe('');
      const name = previewStacks().split(/\s+/)[0];
      const isPreview = tryCmd('kubectl', [
        'get',
        'stacks.pulumi.com',
        name,
        '-n',
        'keel-previews',
        '-o',
        'jsonpath={.spec.preview}',
      ]).stdout;
      expect(isPreview).toBe('true');
      // The preview clones the PR branch itself (PKO gitSource) with a per-preview copy of the
      // tenant's read-only credential; no Flux GitRepository / source-controller artifact.
      const source = tryCmd('kubectl', [
        'get',
        'stacks.pulumi.com',
        name,
        '-n',
        'keel-previews',
        '-o',
        'jsonpath={.spec.projectRepo}|{.spec.fluxSource.sourceRef.name}|{.spec.gitAuth.basicAuth.password.secret.name}|{.spec.commit}|{.metadata.labels.keel\\.io/repo}',
      ]).stdout.split('|');
      expect(source[0], source.join('|')).toMatch(/^https:\/\//);
      // tenant-default's own repository, pinned to the PR head commit.
      expect(source[0], source.join('|')).toMatch(
        new RegExp(`/${TENANT_REPOS_ORG}/[^/]+-tenant-default\\.git$`),
      );
      expect(source[3], source.join('|')).toBe(headSha);
      expect(source[4], source.join('|')).toBe('tenant-default');
      expect(source[1], source.join('|')).toBe('');
      expect(source[2], source.join('|')).toMatch(new RegExp(`^keel-preview-${previewKey}-`));

      // Tenant previews never reference the platform source or anything else in flux-system
      // (no whole-repository fallback).
      const allPreviewStacks = JSON.parse(
        runCmd('kubectl', ['get', 'stacks.pulumi.com', '-n', 'keel-previews', '-o', 'json']),
      ) as {
        items: {
          metadata: { name: string; labels?: Record<string, string> };
          spec: Record<string, unknown>;
        }[];
      };
      const ownStacks = allPreviewStacks.items.filter(
        (s) => s.metadata.labels?.['keel.io/pr-id'] === previewKey,
      );
      expect(ownStacks.length, `preview Stacks of PR #${prId}`).toBeGreaterThan(0);
      for (const stack of ownStacks) {
        expect(stack.spec.fluxSource, stack.metadata.name).toBeUndefined();
        // Only the Git source fields must stay off flux-system: the workspace template may
        // legitimately carry DOCKER_HOST=tcp://keel-docker-proxy.flux-system... (local dev).
        const projectRepo = String(stack.spec.projectRepo ?? '');
        expect(projectRepo, `${stack.metadata.name} projectRepo`).toMatch(/^https:\/\//);
        expect(projectRepo, `${stack.metadata.name} projectRepo`).not.toContain('flux-system');
        const gitAuth = JSON.stringify(stack.spec.gitAuth ?? {});
        expect(gitAuth, `${stack.metadata.name} gitAuth`).not.toContain('flux-system');
        for (const secretName of gitAuth.match(/"name":"([^"]+)"/g) ?? []) {
          expect(secretName, `${stack.metadata.name} gitAuth secret`).toContain(
            `keel-preview-${previewKey}-`,
          );
        }
        expect(String(stack.spec.repoDir ?? ''), stack.metadata.name).toMatch(/^tenants\//);
      }
      const fluxSourcePreviews = allPreviewStacks.items
        .filter((s) => s.spec.fluxSource)
        .map((s) => s.metadata.name);
      expect(fluxSourcePreviews, 'no preview Stack uses fluxSource').toEqual([]);
      // Previews create no Flux GitRepository at all.
      const previewSources = tryCmd('kubectl', [
        'get',
        'gitrepositories.source.toolkit.fluxcd.io',
        '-n',
        'keel-previews',
        '-o',
        'json',
      ]);
      if (previewSources.ok) {
        const repos = (
          JSON.parse(previewSources.stdout) as {
            items: { metadata: { name: string }; spec: Record<string, unknown> }[];
          }
        ).items;
        expect(
          repos.map((r) => r.metadata.name),
          'no preview GitRepository',
        ).toEqual([]);
      }
    });

    await test.step('3. GUI: PKO preview badge + SSE "PKO preview" event', async () => {
      await page.goto(`/#preview/default/${prId}`);
      await expect(page.getByTestId('preview-source-badge')).toHaveAttribute('data-source', 'pko', {
        timeout: 30_000,
      });
      await expect(page.getByTestId('preview-run-status')).toHaveAttribute(
        'data-phase',
        /pending|running|succeeded|failed/,
      );
      await page.goto('/#topology');
      await expect(page.getByTestId('topology-stream-status')).toHaveText(/SSE Live Stream/, {
        timeout: 20_000,
      });
      // Phase transitions are published as `PKO preview <phase> for PR #<id>`.
      await expect(
        page.getByText(new RegExp(`PKO preview \\w+ for PR #${prId} in tenant-default\\b`)).first(),
      ).toBeVisible({
        timeout: 300_000,
      });
    });

    await test.step('4. CLI: proposal list / show / close', async () => {
      const list = runCmdAll(
        KEEL_BIN,
        ['proposal', 'list', '--tenant', 'default', '--output', 'json'],
        cliEnv,
      );
      expect(list.ok, list.out).toBe(true);
      expect(list.out).toContain(`"${prId}"`);
      const show = runCmdAll(KEEL_BIN, ['proposal', 'show', '--tenant', 'default', prId], cliEnv);
      expect(show.ok, show.out).toBe(true);
      expect(show.out).toContain(instance);
      const close = runCmdAll(KEEL_BIN, ['proposal', 'close', '--tenant', 'default', prId], cliEnv);
      expect(close.ok, close.out).toBe(true);
      const closed = runCmdAll(
        KEEL_BIN,
        ['proposal', 'list', '--tenant', 'default', '--status', 'closed', '--output', 'json'],
        cliEnv,
      );
      expect(closed.out).toContain(`"${prId}"`);
    });

    await test.step('5. Preview Stack is garbage-collected after close (60s reconciler)', async () => {
      await expect.poll(previewStacks, { timeout: 240_000, intervals: [5_000] }).toBe('');
      logStep(`Preview Stack for PR #${prId} garbage-collected`);
    });
  });

  test('07.4 - Tenant isolation: no source-controller artifacts, no ExternalArtifact, platform-owned objects', async () => {
    const tenantNs = 'tenant-acme';
    const policyExists = (name: string) =>
      tryCmd('kubectl', ['get', 'validatingpolicies.policies.kyverno.io', name]).ok;

    await test.step('1. source-controller artifacts (port 9090) are unreachable from a tenant pod', async () => {
      const policyName = 'allow-source-controller-artifacts';
      const ingress = tryCmd('kubectl', [
        'get',
        'networkpolicy',
        policyName,
        '-n',
        'flux-system',
        '-o',
        'jsonpath={.spec.ingress}',
      ]);
      expect(ingress.ok, `flux-system/${policyName} must exist`).toBe(true);
      const rules = JSON.parse(ingress.stdout || '[]') as {
        from?: Record<string, unknown>[];
        ports?: { port?: number | string }[];
      }[];
      const openToAll = rules.some(
        (r) =>
          (r.ports ?? []).some((p) => p.port === 9090) &&
          (!r.from ||
            r.from.some(
              (peer) => JSON.stringify(peer) === '{"namespaceSelector":{}}' || 'ipBlock' in peer,
            )),
      );
      expect(openToAll, 'port 9090 is reachable from flux-system only').toBe(false);

      // Same image as the tenant's own podinfo workload (already pulled; ships curl).
      const images = tryCmd('kubectl', [
        'get',
        'pods',
        '-n',
        tenantNs,
        '-o',
        'jsonpath={.items[*].spec.containers[*].image}',
      ]).stdout.split(/\s+/);
      const image = images.find((i) => i.includes('podinfo'));
      expect(image, `podinfo image in ${tenantNs}: ${images.join(',')}`).toBeTruthy();

      const scPodIp = runCmd('kubectl', [
        'get',
        'pods',
        '-n',
        'flux-system',
        '-l',
        'app=source-controller',
        '-o',
        'jsonpath={.items[0].status.podIP}',
      ]);
      expect(scPodIp).not.toBe('');
      const artifactUrl = tryCmd('kubectl', [
        'get',
        'gitrepositories.source.toolkit.fluxcd.io',
        'flux-system',
        '-n',
        'flux-system',
        '-o',
        'jsonpath={.status.artifact.url}',
      ]).stdout;

      const probe = 'keel-e2e-artifact-probe';
      // A tenant may open its own egress (it holds `admin` in its namespace); isolation must hold
      // at source-controller's ingress, so the probe gets unrestricted egress.
      const manifest = [
        'apiVersion: networking.k8s.io/v1',
        'kind: NetworkPolicy',
        'metadata:',
        `  name: ${probe}-egress`,
        `  namespace: ${tenantNs}`,
        'spec:',
        '  podSelector:',
        '    matchLabels:',
        `      keel.io/e2e-probe: ${probe}`,
        '  policyTypes: [Egress]',
        '  egress:',
        '    - {}',
        '---',
        'apiVersion: v1',
        'kind: Pod',
        'metadata:',
        `  name: ${probe}`,
        `  namespace: ${tenantNs}`,
        '  labels:',
        `    keel.io/e2e-probe: ${probe}`,
        'spec:',
        '  restartPolicy: Never',
        '  automountServiceAccountToken: false',
        '  terminationGracePeriodSeconds: 0',
        '  securityContext:',
        '    runAsNonRoot: true',
        '    runAsUser: 10001',
        '    runAsGroup: 10001',
        '    seccompProfile:',
        '      type: RuntimeDefault',
        '  containers:',
        '    - name: probe',
        `      image: ${image}`,
        '      imagePullPolicy: IfNotPresent',
        '      command: ["sleep", "600"]',
        '      resources:',
        '        requests: { cpu: 10m, memory: 16Mi }',
        '        limits: { cpu: 100m, memory: 64Mi }',
        '      securityContext:',
        '        allowPrivilegeEscalation: false',
        '        readOnlyRootFilesystem: true',
        '        runAsNonRoot: true',
        '        capabilities:',
        '          drop: [ALL]',
        '',
      ].join('\n');
      const cleanup = () => {
        tryCmd('kubectl', [
          'delete',
          'pod',
          probe,
          '-n',
          tenantNs,
          '--ignore-not-found',
          '--wait=false',
        ]);
        tryCmd('kubectl', [
          'delete',
          'networkpolicy',
          `${probe}-egress`,
          '-n',
          tenantNs,
          '--ignore-not-found',
        ]);
      };
      cleanup();
      try {
        const applied = runCmdStdin('kubectl', ['apply', '-f', '-'], manifest);
        expect(applied.ok, applied.out).toBe(true);
        const ready = runCmdAll('kubectl', [
          'wait',
          '--for=condition=Ready',
          `pod/${probe}`,
          '-n',
          tenantNs,
          '--timeout=120s',
        ]);
        expect(ready.ok, ready.out).toBe(true);
        const curl = (url: string) =>
          runCmdAll('kubectl', [
            'exec',
            '-n',
            tenantNs,
            probe,
            '--',
            'curl',
            '-sS',
            '-o',
            '/dev/null',
            '-w',
            '%{http_code}',
            '--connect-timeout',
            '5',
            '-m',
            '8',
            url,
          ]);
        // Positive control: the probe has network access (source-controller metrics are open to
        // every namespace by the flux2 chart's `allow-scraping` policy).
        const metrics = curl(`http://${scPodIp}:8080/metrics`);
        expect(metrics.ok && metrics.out.trim() === '200', `metrics control: ${metrics.out}`).toBe(
          true,
        );
        for (const url of [
          'http://source-controller.flux-system.svc.cluster.local/',
          `http://${scPodIp}:9090/`,
          ...(artifactUrl ? [artifactUrl] : []),
        ]) {
          const res = curl(url);
          expect(res.ok, `${url} must be unreachable from ${tenantNs}: ${res.out}`).toBe(false);
          logStep(`Artifact server unreachable from ${tenantNs}: ${url}`);
        }
      } finally {
        cleanup();
      }
    });

    await test.step('2. ExternalArtifact and tenant Kustomizations without the tenant SA are denied', async () => {
      expect(
        policyExists('restrict-tenant-flux-sources'),
        'restrict-tenant-flux-sources must exist',
      ).toBe(true);
      if (!tryCmd('kubectl', ['get', 'crd', 'externalartifacts.source.toolkit.fluxcd.io']).ok) {
        logStep('ExternalArtifact CRD not installed; skipping ExternalArtifact admission check');
      } else {
        const ea = runCmdStdin(
          'kubectl',
          ['apply', '--dry-run=server', '-f', '-'],
          [
            'apiVersion: source.toolkit.fluxcd.io/v1',
            'kind: ExternalArtifact',
            'metadata:',
            '  name: keel-e2e-probe',
            `  namespace: ${tenantNs}`,
            'spec:',
            '  sourceRef:',
            '    apiVersion: source.extensions.fluxcd.io/v1beta1',
            '    kind: ArtifactGenerator',
            '    name: keel-e2e-probe',
            '',
          ].join('\n'),
        );
        expect(ea.ok, ea.out).toBe(false);
        expect(ea.out).toMatch(/restrict-tenant-flux-sources|not allowed in tenant namespaces/);
        logStep('ExternalArtifact in tenant-acme denied by Kyverno');
      }
      const ks = runCmdStdin(
        'kubectl',
        ['apply', '--dry-run=server', '-f', '-'],
        [
          'apiVersion: kustomize.toolkit.fluxcd.io/v1',
          'kind: Kustomization',
          'metadata:',
          '  name: keel-e2e-probe',
          `  namespace: ${tenantNs}`,
          'spec:',
          '  interval: 10m',
          '  path: ./',
          '  prune: false',
          '  serviceAccountName: default',
          '  sourceRef:',
          '    kind: GitRepository',
          '    name: keel-gitops',
          '',
        ].join('\n'),
      );
      expect(ks.ok, ks.out).toBe(false);
      expect(ks.out).toMatch(/restrict-tenant-flux-sources|tenant-<id>-sa/);
    });

    await test.step('3. Tenant identities cannot write Flux source status (Kyverno, not RBAC)', async () => {
      expect(
        policyExists('restrict-tenant-flux-source-status'),
        'restrict-tenant-flux-source-status must exist',
      ).toBe(true);
      // Schema-valid status patch, so the request reaches validating admission.
      const patchStatus = (identity: string[]) =>
        runCmdAll('kubectl', [
          ...identity,
          'patch',
          'gitrepositories.source.toolkit.fluxcd.io',
          'keel-gitops',
          '-n',
          'tenant-default',
          '--subresource=status',
          '--type=merge',
          '-p',
          '{"status":{"observedGeneration":1}}',
          '--dry-run=server',
        ]);
      // A tenant group member that RBAC fully allows (`system:masters`): only Kyverno's
      // `restrict-tenant-flux-source-status` (tenant subject in matchConditions) can deny it.
      const forged = patchStatus([
        '--as=keel-e2e-tenant-user',
        '--as-group=tenant-acme-admins',
        '--as-group=system:masters',
      ]);
      expect(forged.ok, forged.out).toBe(false);
      expect(forged.out).toMatch(
        /restrict-tenant-flux-source-status|may not write the status of Flux source objects/,
      );
      // The same request without the tenant group is not a tenant subject and is admitted.
      const admin = patchStatus(['--as=keel-e2e-admin', '--as-group=system:masters']);
      expect(admin.ok, `non-tenant status write (dry run) must pass: ${admin.out}`).toBe(true);
      // The tenant's own ServiceAccount: denied too (by Kyverno whenever RBAC would allow it).
      const tenantSa = 'system:serviceaccount:tenant-default:tenant-default-sa';
      const canI = tryCmd('kubectl', [
        'auth',
        'can-i',
        'patch',
        'gitrepositories.source.toolkit.fluxcd.io',
        '--subresource=status',
        '-n',
        'tenant-default',
        `--as=${tenantSa}`,
      ]).stdout.trim();
      const saWrite = patchStatus([`--as=${tenantSa}`]);
      expect(saWrite.ok, saWrite.out).toBe(false);
      if (canI === 'yes') {
        expect(saWrite.out).toMatch(
          /restrict-tenant-flux-source-status|may not write the status of Flux source objects/,
        );
      }
      logStep(
        `Tenant status writes on tenant-default/keel-gitops denied by Kyverno (SA RBAC: ${canI})`,
      );
    });

    await test.step('4. Tenant identities cannot change platform-owned objects', async () => {
      expect(
        policyExists('protect-platform-owned-tenant-objects'),
        'protect-platform-owned-tenant-objects must exist',
      ).toBe(true);
      const owned = tryCmd('kubectl', [
        'get',
        'networkpolicy',
        'default-deny-all',
        '-n',
        tenantNs,
        '-o',
        'jsonpath={.metadata.labels.keel\\.io/platform-owned}',
      ]).stdout;
      expect(owned, `${tenantNs}/default-deny-all is platform-owned`).toBe('true');
      const edit = runCmdAll('kubectl', [
        `--as=system:serviceaccount:${tenantNs}:${tenantNs}-sa`,
        'label',
        'networkpolicy',
        'default-deny-all',
        '-n',
        tenantNs,
        'keel.io/e2e-probe=tamper',
        '--dry-run=server',
      ]);
      expect(edit.ok, edit.out).toBe(false);
      expect(edit.out).toMatch(/protect-platform-owned-tenant-objects|managed by the platform/);
      logStep('Tenant SA denied from editing platform-owned default-deny-all');

      // `flux reconcile source git keel-gitops` (only `reconcile.fluxcd.io/requestedAt` changes)
      // stays admitted for identities RBAC allows; any other change to the source is denied.
      const exemption = tryCmd('kubectl', [
        'get',
        'validatingpolicies.policies.kyverno.io',
        'protect-platform-owned-tenant-objects',
        '-o',
        'jsonpath={.spec.validations[0].expression}',
      ]).stdout;
      const sourceOwned = tryCmd('kubectl', [
        'get',
        'gitrepositories.source.toolkit.fluxcd.io',
        'keel-gitops',
        '-n',
        'tenant-default',
        '-o',
        'jsonpath={.metadata.labels.keel\\.io/platform-owned}',
      ]).stdout;
      expect(exemption, 'keel-gitops reconcile-request exemption').toContain(
        'reconcile.fluxcd.io/requestedAt',
      );
      expect(sourceOwned, 'tenant-default/keel-gitops is platform-owned').toBe('true');
      const sourceSa = 'system:serviceaccount:tenant-default:tenant-default-sa';
      const canPatch = tryCmd('kubectl', [
        'auth',
        'can-i',
        'patch',
        'gitrepositories.source.toolkit.fluxcd.io',
        '-n',
        'tenant-default',
        `--as=${sourceSa}`,
      ]).stdout.trim();
      if (canPatch !== 'yes') {
        logStep(`${sourceSa} cannot patch GitRepositories (RBAC); reconcile-request probe skipped`);
        return;
      }
      const reconcile = runCmdAll('kubectl', [
        `--as=${sourceSa}`,
        'annotate',
        '--overwrite',
        'gitrepositories.source.toolkit.fluxcd.io',
        'keel-gitops',
        '-n',
        'tenant-default',
        `reconcile.fluxcd.io/requestedAt=e2e-${Date.now()}`,
        '--dry-run=server',
      ]);
      expect(reconcile.ok, `reconcile request on keel-gitops must pass: ${reconcile.out}`).toBe(
        true,
      );
      const repoint = runCmdAll('kubectl', [
        `--as=${sourceSa}`,
        'annotate',
        '--overwrite',
        'gitrepositories.source.toolkit.fluxcd.io',
        'keel-gitops',
        '-n',
        'tenant-default',
        'keel.io/e2e-probe=tamper',
        '--dry-run=server',
      ]);
      expect(repoint.ok, repoint.out).toBe(false);
      expect(repoint.out).toMatch(/protect-platform-owned-tenant-objects|managed by the platform/);
      logStep('keel-gitops: reconcile request admitted, other annotation changes denied');
    });
  });

  test('07.5 - Tenant PKO hardening: no Workspace/Update/Stack status writes, no keel.io/protect, HelmRelease SA', async () => {
    const tenantNs = 'tenant-acme';
    const policyExists = (name: string) =>
      tryCmd('kubectl', ['get', 'validatingpolicies.policies.kyverno.io', name]).ok;
    // A tenant group member that RBAC fully allows (`system:masters`): only Kyverno's
    // `restrict-tenant-pulumi-objects` (tenant subject in matchConditions) can deny it. The same
    // request without the tenant group is not a tenant subject and must be admitted (dry run).
    const forgedTenant = [
      '--as=keel-e2e-tenant-user',
      '--as-group=tenant-acme-admins',
      '--as-group=system:masters',
    ];
    const admin = ['--as=keel-e2e-admin', '--as-group=system:masters'];
    const tenantSa = `system:serviceaccount:${tenantNs}:${tenantNs}-sa`;
    const dryRunApply = (identity: string[], manifest: string[]) =>
      runCmdStdin(
        'kubectl',
        [...identity, 'apply', '--dry-run=server', '-f', '-'],
        [...manifest, ''].join('\n'),
      );
    const pulumiDenied =
      /restrict-tenant-pulumi-objects|Tenant identities may not|Tenant Stacks may not/;

    await test.step('1. PKO aggregated roles are installed and give the tenant SA Stacks', async () => {
      const aggregated = runCmd('kubectl', [
        'get',
        'clusterroles',
        '-l',
        'rbac.authorization.k8s.io/aggregate-to-edit=true',
        '-o',
        'jsonpath={.items[*].metadata.name}',
      ]).split(/\s+/);
      expect(
        aggregated.some((n) => n.includes('pulumi-kubernetes-operator') && n.endsWith('-edit')),
        `PKO edit aggregation ClusterRole: ${aggregated.join(',')}`,
      ).toBe(true);
      const canI = tryCmd('kubectl', [
        'auth',
        'can-i',
        'create',
        'stacks.pulumi.com',
        '-n',
        tenantNs,
        `--as=${tenantSa}`,
      ]).stdout.trim();
      expect(canI, `${tenantSa} creates Stacks through the aggregated roles`).toBe('yes');
    });

    expect(
      policyExists('restrict-tenant-pulumi-objects'),
      'restrict-tenant-pulumi-objects must exist',
    ).toBe(true);

    await test.step('2. Tenant identities cannot write auto.pulumi.com Workspaces or Updates', async () => {
      for (const kind of ['Workspace', 'Update']) {
        const manifest = [
          'apiVersion: auto.pulumi.com/v1alpha1',
          `kind: ${kind}`,
          'metadata:',
          `  name: keel-e2e-probe-${kind.toLowerCase()}`,
          `  namespace: ${tenantNs}`,
          'spec: {}',
        ];
        const forged = dryRunApply(forgedTenant, manifest);
        expect(forged.ok, forged.out).toBe(false);
        expect(forged.out).toMatch(pulumiDenied);
        const sa = dryRunApply([`--as=${tenantSa}`], manifest);
        expect(sa.ok, sa.out).toBe(false);
        expect(sa.out).toMatch(pulumiDenied);
        const control = dryRunApply(admin, manifest);
        expect(control.ok, `non-tenant ${kind} (dry run) must pass: ${control.out}`).toBe(true);
        logStep(`${kind} in ${tenantNs}: tenant identities denied, admin admitted`);
      }
    });

    await test.step('3. Tenant identities cannot write Stack status', async () => {
      const stacks = runCmd('kubectl', [
        'get',
        'stacks.pulumi.com',
        '-A',
        '-o',
        'jsonpath={range .items[*]}{.metadata.namespace}/{.metadata.name}{"\\n"}{end}',
      ])
        .split('\n')
        .filter((l) => l.startsWith('tenant-'));
      if (stacks.length === 0) {
        logStep('No tenant Stack exists; Stack status probe skipped');
        return;
      }
      const [ns, name] = stacks[0].split('/');
      const patchStatus = (identity: string[]) =>
        runCmdAll('kubectl', [
          ...identity,
          'patch',
          'stacks.pulumi.com',
          name,
          '-n',
          ns,
          '--subresource=status',
          '--type=merge',
          '-p',
          '{"status":{"observedGeneration":1}}',
          '--dry-run=server',
        ]);
      const forged = patchStatus(forgedTenant);
      expect(forged.ok, forged.out).toBe(false);
      expect(forged.out).toMatch(pulumiDenied);
      const control = patchStatus(admin);
      expect(control.ok, `non-tenant Stack status write (dry run) must pass: ${control.out}`).toBe(
        true,
      );
      logStep(`${ns}/${name}: tenant Stack status writes denied`);
    });

    await test.step('4. Tenant Stacks cannot carry keel.io/protect, protect config or the management Stack name', async () => {
      const stack = (name: string, labels: string[], spec: string[] = []) => [
        'apiVersion: pulumi.com/v1',
        'kind: Stack',
        'metadata:',
        `  name: ${name}`,
        `  namespace: ${tenantNs}`,
        '  labels:',
        '    keel.io/provider: kind',
        ...labels,
        'spec:',
        '  stack: keel-e2e-probe',
        ...spec,
      ];
      const protectedStack = stack('keel-e2e-probe-protect', ['    keel.io/protect: "true"']);
      const forged = dryRunApply(forgedTenant, protectedStack);
      expect(forged.ok, forged.out).toBe(false);
      expect(forged.out).toMatch(pulumiDenied);
      const sa = dryRunApply([`--as=${tenantSa}`], protectedStack);
      expect(sa.ok, sa.out).toBe(false);
      expect(sa.out).toMatch(pulumiDenied);
      const reserved = dryRunApply(forgedTenant, stack('management-cluster-core', []));
      expect(reserved.ok, reserved.out).toBe(false);
      expect(reserved.out).toMatch(/restrict-tenant-pulumi-objects|reserved/);
      // `spec.config.protect` / `spec.config["keel:protect"]` count as protection for Keel's
      // force-delete and preview GC, so tenants may not set them either.
      for (const key of ['protect', '"keel:protect"']) {
        const res = dryRunApply(
          [`--as=${tenantSa}`],
          stack('keel-e2e-probe-protect-config', [], ['  config:', `    ${key}: "true"`]),
        );
        expect(res.ok, `tenant Stack with spec.config.${key} (dry run): ${res.out}`).toBe(false);
        expect(res.out).toMatch(pulumiDenied);
      }
      // Positive control: the same tenant Stack without the label is admitted.
      const plain = dryRunApply([`--as=${tenantSa}`], stack('keel-e2e-probe-plain', []));
      expect(plain.ok, `unlabelled tenant Stack (dry run) must pass: ${plain.out}`).toBe(true);
      logStep(
        'Tenant Stacks: keel.io/protect, protect config and management-cluster-core denied, plain Stack admitted',
      );
    });

    await test.step('5. Tenant HelmReleases must impersonate tenant-<id>-sa', async () => {
      const exprs = tryCmd('kubectl', [
        'get',
        'validatingpolicies.policies.kyverno.io',
        'restrict-tenant-flux-sources',
        '-o',
        'jsonpath={.spec.validations[*].expression}',
      ]).stdout;
      expect(exprs, 'restrict-tenant-flux-sources carries the HelmRelease SA rule').toContain(
        'HelmRelease',
      );
      // Every HelmRelease Keel committed for a tenant already satisfies the rule.
      const releases = runCmd('kubectl', [
        'get',
        'helmreleases.helm.toolkit.fluxcd.io',
        '-A',
        '-o',
        'jsonpath={range .items[*]}{.metadata.namespace} {.spec.serviceAccountName}{"\\n"}{end}',
      ])
        .split('\n')
        .filter((l) => l.startsWith('tenant-'));
      for (const line of releases) {
        const [ns, sa] = line.split(' ');
        expect(sa, `HelmRelease in ${ns} impersonates ${ns}-sa`).toBe(`${ns}-sa`);
      }
      const hr = (serviceAccount: string | null) => [
        'apiVersion: helm.toolkit.fluxcd.io/v2',
        'kind: HelmRelease',
        'metadata:',
        '  name: keel-e2e-probe',
        `  namespace: ${tenantNs}`,
        'spec:',
        '  interval: 10m',
        ...(serviceAccount ? [`  serviceAccountName: ${serviceAccount}`] : []),
        '  chart:',
        '    spec:',
        '      chart: podinfo',
        '      sourceRef:',
        '        kind: HelmRepository',
        '        name: keel-e2e-probe',
      ];
      for (const sa of [null, 'default']) {
        const res = dryRunApply(admin, hr(sa));
        expect(res.ok, res.out).toBe(false);
        expect(res.out).toMatch(/restrict-tenant-flux-sources|tenant-<id>-sa/);
      }
      const ok = dryRunApply(admin, hr(`${tenantNs}-sa`));
      expect(ok.ok, `HelmRelease with the tenant SA (dry run) must pass: ${ok.out}`).toBe(true);
      logStep(`${tenantNs} HelmReleases: missing/foreign SA denied, ${tenantNs}-sa admitted`);
    });
  });

  test('07.6 - Per-tenant repositories: acme and tenant-default each run from their own keel-tenants repository', async ({
    page,
  }) => {
    await signInViaRauthy(page, adminUsername, adminPassword);
    const caFile = keelCaFile();
    const workRoot = fs.mkdtempSync(path.join(os.tmpdir(), 'keel-e2e-tenant-repos-'));
    const admin = forgeAdminAuth();
    expect(admin.password.length, 'keel-system/forge-token').toBeGreaterThan(0);
    const platformPath = new URL(
      runCmd('kubectl', [
        'get',
        'gitrepositories.source.toolkit.fluxcd.io',
        'flux-system',
        '-n',
        'flux-system',
        '-o',
        'jsonpath={.spec.url}',
      ]),
    ).pathname;
    const monorepoUrl = `${FORGEJO_ORIGIN}${platformPath}`;
    const monorepoName = path.posix.basename(platformPath).replace(/\.git$/, '');
    const tenantRepoUrl = (tenant: string) =>
      `${FORGEJO_ORIGIN}/${TENANT_REPOS_ORG}/${monorepoName}-tenant-${tenant}.git`;
    const acmeUrl = tenantRepoUrl('acme');
    const lsRemote = (auth: GitBasicAuth, url: string) =>
      gitAs(auth, ['ls-remote', '--heads', url], caFile);
    const clone = (auth: GitBasicAuth, url: string, name: string): string => {
      const dir = path.join(workRoot, name);
      fs.rmSync(dir, { recursive: true, force: true });
      const res = gitAs(auth, ['clone', '--quiet', url, dir], caFile);
      expect(res.ok, `clone ${url}: ${res.out}`).toBe(true);
      return dir;
    };
    /** `true` when `needle` appears in any commit of the clone at `dir` (output never logged). */
    const historyContains = (dir: string, needle: string) =>
      runCmdAll('git', ['-C', dir, 'log', '--all', '--format=%H', `-S${needle}`]).out.trim() !== '';
    const kubectlJson = <T>(args: string[]): T =>
      JSON.parse(runCmd('kubectl', [...args, '-o', 'json'])) as T;
    type K8sObject = {
      metadata: { name: string; namespace?: string; labels?: Record<string, string> };
      spec: Record<string, unknown>;
    };
    const echoToken = `e2e-${randomUUID()}`;
    const echoText = 'hello-from-the-acme-repository';

    try {
      await test.step('1. Each tenant syncs its own repository with a cluster-only, repository-scoped credential', async () => {
        for (const tenant of ['default', 'acme']) {
          const ns = `tenant-${tenant}`;
          const source = kubectlJson<K8sObject>([
            'get',
            'gitrepositories.source.toolkit.fluxcd.io',
            'keel-gitops',
            '-n',
            ns,
          ]);
          expect(source.spec.url, `${ns}/keel-gitops url`).toBe(tenantRepoUrl(tenant));
          expect(source.spec.interval, `${ns}/keel-gitops interval`).toBe('1m');
          expect(source.spec.secretRef, `${ns}/keel-gitops secretRef`).toEqual({
            name: 'keel-gitops-credentials',
          });
          expect(source.metadata.labels?.['keel.io/platform-owned'], ns).toBeDefined();
          await waitForK8sReady('gitrepository', 'keel-gitops', ns, 180_000);

          const root = kubectlJson<K8sObject>([
            'get',
            'kustomizations.kustomize.toolkit.fluxcd.io',
            'keel-tenant-root',
            '-n',
            ns,
          ]);
          expect(root.spec.path, `${ns}/keel-tenant-root path`).toBe(`./tenants/${tenant}`);
          expect(root.spec.serviceAccountName, `${ns}/keel-tenant-root SA`).toBe(`${ns}-sa`);
          expect(root.spec.sourceRef, `${ns}/keel-tenant-root sourceRef`).toMatchObject({
            kind: 'GitRepository',
            name: 'keel-gitops',
          });
          expect(JSON.stringify(root.spec.decryption ?? {}), ns).toContain('sops-age');
          // The stub itself is applied by the platform (`flux-system/keel-tenants`).
          expect(root.metadata.labels?.['kustomize.toolkit.fluxcd.io/name'], ns).toBe(
            'keel-tenants',
          );
          await waitForK8sReady('kustomization', 'keel-tenant-root', ns, 300_000);

          const credential = kubectlJson<{
            metadata: { labels?: Record<string, string> };
            data?: Record<string, string>;
          }>(['get', 'secret', 'keel-gitops-credentials', '-n', ns]);
          expect(Object.keys(credential.data ?? {}).sort(), `${ns} credential keys`).toEqual(
            expect.arrayContaining(['password', 'username']),
          );
          expect(credential.metadata.labels?.['keel.io/secret-kind'], ns).toBe(
            'tenant-gitops-credentials',
          );
          const auth = tenantRepoAuth(tenant);
          // A bot-owned, repository-scoped token: never the platform repository owner's.
          expect(auth.username, `${ns} credential user`).not.toBe(admin.username);
          expect(auth.password === admin.password, `${ns} credential is not the forge token`).toBe(
            false,
          );
          expect(tryCmd('kubectl', ['get', 'secret', 'sops-age', '-n', ns]).ok, ns).toBe(true);
        }
        // tenant-default no longer receives the platform repository's read token.
        expect(
          tryCmd('kubectl', ['get', 'secret', 'git-credentials', '-n', 'tenant-default']).ok,
          'tenant-default/git-credentials (shared platform token) must not exist',
        ).toBe(false);
        logStep('tenant-default and tenant-acme sync their own repositories');
      });

      await test.step("2. acme's credential reads the acme repository only, and only reads", async () => {
        const acme = tenantRepoAuth('acme');
        const tenantDefault = tenantRepoAuth('default');
        const own = lsRemote(acme, acmeUrl);
        expect(own.ok, `acme credential must read ${acmeUrl}: ${own.out}`).toBe(true);
        expect(own.out).toContain('refs/heads/main');
        for (const url of [monorepoUrl, tenantRepoUrl('default')]) {
          const res = lsRemote(acme, url);
          expect(res.ok, `acme credential must not read ${url}`).toBe(false);
          expect(res.out, url).toMatch(/\b403\b/);
          logStep(`acme credential denied (403) on ${url}`);
        }
        const cross = lsRemote(tenantDefault, acmeUrl);
        expect(cross.ok, 'tenant-default credential must not read the acme repository').toBe(false);
        expect(cross.out).toMatch(/\b403\b/);
        // Read-only: the receive-pack handshake is refused.
        const acmeClone = clone(acme, acmeUrl, 'acme-as-acme');
        const push = gitAs(
          acme,
          ['-C', acmeClone, 'push', '--dry-run', 'origin', 'HEAD:refs/heads/keel-e2e-probe'],
          caFile,
        );
        expect(push.ok, 'acme credential must not push').toBe(false);
        expect(push.out).toMatch(/\b403\b/);
      });

      await test.step('3. acme manifest app with a secret value runs from the acme repository', async () => {
        const appName = 'acme-echo';
        if (!tryCmd('kubectl', ['get', 'kustomization', appName, '-n', 'tenant-acme']).ok) {
          const res = await page.request.post('/api/v1/tenants/acme/proposals', {
            headers: SAME_ORIGIN,
            data: {
              tenant: 'acme',
              catalogId: 'http-echo-manifest',
              name: appName,
              values: { echoText, replicas: 1, apiToken: echoToken },
            },
          });
          const text = await res.text();
          expect(text.includes(echoToken), 'proposal response must not echo the secret').toBe(
            false,
          );
          expect(res.status(), text).toBe(201);
          const proposal = (JSON.parse(text) as { proposal: ScopedProposalBody }).proposal;
          expect(proposal.scope).toBe('acme');
          expect(proposal.repository.label).toBe('tenant-acme');
          expect(proposal.repository.fullName).toBe(
            `${TENANT_REPOS_ORG}/${monorepoName}-tenant-acme`,
          );
          const merged = await page.request.post(
            `/api/v1/tenants/acme/proposals/${proposal.id}/merge`,
            { headers: SAME_ORIGIN },
          );
          expect(merged.status(), await merged.text()).toBe(200);
          logStep(`Merged acme manifest app PR #${proposal.id} in the acme repository`);
        }
        await waitForK8sReady('kustomization', `${appName}-config`, 'tenant-acme', 300_000);
        await waitForK8sReady('kustomization', appName, 'tenant-acme', 300_000);
        for (const name of [`${appName}-config`, appName]) {
          const sourceRef = runCmd('kubectl', [
            'get',
            'kustomization',
            name,
            '-n',
            'tenant-acme',
            '-o',
            'jsonpath={.spec.sourceRef.name}',
          ]);
          expect(sourceRef, `${name} source`).toBe('keel-gitops');
        }
        const rollout = runCmdAll('kubectl', [
          'rollout',
          'status',
          'deployment/http-echo',
          '-n',
          'tenant-acme',
          '--timeout=240s',
        ]);
        expect(rollout.ok, rollout.out).toBe(true);
        const args = runCmd('kubectl', [
          'get',
          'deployment',
          'http-echo',
          '-n',
          'tenant-acme',
          '-o',
          'jsonpath={.spec.template.spec.containers[0].args}',
        ]);
        expect(args).toContain(`-text=${echoText}`);
        // The SOPS-decrypted secret value was substituted into the workload's Secret (compared,
        // never printed).
        const substituted = secretValue('tenant-acme', 'http-echo-api-token', 'token');
        expect(substituted === echoToken, 'substituted apiToken reaches the workload').toBe(true);
        const envRef = runCmd('kubectl', [
          'get',
          'deployment',
          'http-echo',
          '-n',
          'tenant-acme',
          '-o',
          'jsonpath={.spec.template.spec.containers[0].env[?(@.name=="ECHO_API_TOKEN")].valueFrom.secretKeyRef.name}',
        ]);
        expect(envRef).toBe('http-echo-api-token');
        // Never logged by keel serve.
        expect(
          fs.readFileSync(SERVE_LOG_PATH, 'utf-8').includes(echoToken),
          'keel serve log must not contain the secret',
        ).toBe(false);
      });

      await test.step('4. The monorepo holds only platform content and per-tenant stubs', async () => {
        const mono = clone(admin, monorepoUrl, 'platform');
        const tenantDirs = fs
          .readdirSync(path.join(mono, 'tenants'), { withFileTypes: true })
          .filter((e) => e.isDirectory())
          .map((e) => e.name)
          .sort();
        expect(tenantDirs).toEqual(expect.arrayContaining(['acme', 'default']));
        for (const id of tenantDirs) {
          const files = listFiles(mono, `tenants/${id}`).map((f) =>
            f.slice(`tenants/${id}/`.length),
          );
          expect(files, `monorepo tenants/${id}/ holds only the platform stub`).toEqual(
            TENANT_STUB_FILES,
          );
        }
        const rootResources = fs.readFileSync(
          path.join(mono, 'tenants/kustomization.yaml'),
          'utf-8',
        );
        for (const line of rootResources.split('\n').filter((l) => /^\s*-\s/.test(l))) {
          expect(line, 'tenants/kustomization.yaml lists stub files only').toMatch(
            /^\s*-\s+[a-z0-9-]+\/(git-repository|rbac|sync)\.yaml\s*$/,
          );
        }

        const contentOf = (dir: string) =>
          listFiles(dir)
            .filter((f) => f.endsWith('.yaml'))
            .map((f) => fs.readFileSync(path.join(dir, f), 'utf-8'))
            .join('\n---\n');
        const tenantClones: Record<string, string> = {};
        for (const tenant of ['default', 'acme']) {
          const dir = clone(admin, tenantRepoUrl(tenant), `tenant-${tenant}`);
          tenantClones[tenant] = dir;
          const files = listFiles(dir);
          const outside = files.filter(
            (f) =>
              !f.startsWith(`tenants/${tenant}/`) && !/^(README\.md|LICENSE|\.gitignore)$/i.test(f),
          );
          expect(outside, `tenant-${tenant} repository holds only tenants/${tenant}/`).toEqual([]);
          expect(files).toContain(`tenants/${tenant}/kustomization.yaml`);
        }
        // tenant-default's 07.1/07.2 flows were committed to its own repository...
        const defaultContent = contentOf(tenantClones.default);
        for (const name of [
          'podinfo',
          `${WORKLOAD_CLUSTER}-cluster`,
          `${WORKLOAD_CLUSTER}-podinfo`,
        ]) {
          expect(defaultContent, `tenant-default repository defines ${name}`).toContain(
            `name: ${name}`,
          );
        }
        // ...and acme's to its own.
        const acmeContent = contentOf(tenantClones.acme);
        for (const name of ['acme-podinfo', 'acme-echo']) {
          expect(acmeContent, `acme repository defines ${name}`).toContain(`name: ${name}`);
        }
        expect(acmeContent, 'acme secrets are SOPS-encrypted').toContain('ENC[');
        // No tenant credential or secret value is in any repository's history.
        const needles = [
          echoToken,
          tenantRepoAuth('acme').password,
          tenantRepoAuth('default').password,
        ];
        for (const [label, dir] of [
          ['platform', mono],
          ['tenant-default', tenantClones.default],
          ['tenant-acme', tenantClones.acme],
        ] as const) {
          for (const [i, needle] of needles.entries()) {
            expect(historyContains(dir, needle), `${label} history holds secret #${i}`).toBe(false);
          }
        }
        logStep('Monorepo: stubs only; tenant repositories: own content only; no secrets in Git');
      });

      await test.step('5. `keel tenant repair-credentials acme` rotates the credential', async () => {
        const before = tenantRepoAuth('acme');
        const repair = runCmdAll(
          KEEL_BIN,
          ['tenant', 'repair-credentials', 'acme', '--server', BASE_URL],
          cliEnv,
        );
        expect(repair.ok, repair.out).toBe(true);
        expect(repair.out.includes(before.password), 'repair output holds no credential').toBe(
          false,
        );
        await expect
          .poll(() => tenantRepoAuth('acme').password !== before.password, {
            timeout: 60_000,
            intervals: [2_000],
          })
          .toBe(true);
        const after = tenantRepoAuth('acme');
        expect(repair.out.includes(after.password), 'repair output holds no credential').toBe(
          false,
        );
        const old = lsRemote(before, acmeUrl);
        expect(old.ok, 'the rotated-out acme token is revoked').toBe(false);
        const fresh = lsRemote(after, acmeUrl);
        expect(fresh.ok, `the new acme token reads ${acmeUrl}: ${fresh.out}`).toBe(true);
        const denied = lsRemote(after, monorepoUrl);
        expect(denied.ok, 'the new acme token cannot read the monorepo').toBe(false);
        expect(denied.out).toMatch(/\b403\b/);
        logStep(
          'acme credential rotated: old token revoked, new token scoped to the acme repository',
        );
      });

      await test.step('6. acme PKO blueprint: preview pinned to the acme PR commit, then Ready from the acme repository', async () => {
        const existing = tryCmd('kubectl', [
          'get',
          'stacks.pulumi.com',
          '-n',
          'tenant-acme',
          '-o',
          'jsonpath={.items[*].metadata.name}',
        ])
          .stdout.split(/\s+/)
          .find((n) => n.startsWith(ACME_CLUSTER));
        if (!existing) {
          const res = await page.request.post('/api/v1/tenants/acme/proposals', {
            headers: SAME_ORIGIN,
            data: {
              tenant: 'acme',
              catalogId: 'kind-cluster',
              kind: 'infra',
              name: ACME_CLUSTER,
              values: { clusterName: ACME_CLUSTER, kubernetesVersion: 'v1.35.8', workerCount: 0 },
            },
          });
          const text = await res.text();
          expect(res.status(), text).toBe(201);
          const proposal = (JSON.parse(text) as { proposal: ScopedProposalBody }).proposal;
          expect(proposal.scope).toBe('acme');
          expect(proposal.repository.label).toBe('tenant-acme');
          const key = `acme-${proposal.id}`;

          // The preview is launched after the rotation: it copies the NEW credential.
          await expect
            .poll(
              async () => {
                const r = await page.request.get(
                  `/api/v1/tenants/acme/proposals/${proposal.id}/preview`,
                );
                if (!r.ok()) return `http ${r.status()}`;
                const b = await r.json();
                const report = b.report ?? b;
                return `${report.source}/${report.run?.phase}`;
              },
              { timeout: 420_000, intervals: [5_000] },
            )
            .toBe('pko/succeeded');
          const previews = kubectlJson<{ items: K8sObject[] }>([
            'get',
            'stacks.pulumi.com',
            '-n',
            'keel-previews',
            '-l',
            `keel.io/pr-id=${key}`,
          ]).items;
          expect(previews.length, `preview Stacks of ${key}`).toBeGreaterThan(0);
          for (const stack of previews) {
            const spec = stack.spec;
            expect(spec.preview, stack.metadata.name).toBe(true);
            expect(spec.projectRepo, `${stack.metadata.name} clones the acme repository`).toBe(
              acmeUrl,
            );
            expect(spec.commit, `${stack.metadata.name} is pinned to the PR head`).toBe(
              proposal.headSha,
            );
            expect(stack.metadata.labels?.['keel.io/repo'], stack.metadata.name).toBe(
              'tenant-acme',
            );
            expect(String(spec.repoDir ?? ''), stack.metadata.name).toMatch(/^tenants\/acme\//);
            expect(spec.fluxSource, stack.metadata.name).toBeUndefined();
            // acme previews never reference the monorepo, another tenant or flux-system.
            const serialized = JSON.stringify(spec);
            expect(serialized).not.toContain(`/${monorepoName}.git`);
            expect(serialized).not.toContain('-tenant-default');
            const gitAuth = JSON.stringify(spec.gitAuth ?? {});
            expect(gitAuth).not.toContain('flux-system');
            const secretNames = gitAuth.match(/"name":"([^"]+)"/g) ?? [];
            expect(secretNames.length, `${stack.metadata.name} gitAuth`).toBeGreaterThan(0);
            for (const secretName of secretNames) {
              expect(secretName).toContain(`keel-preview-${key}-`);
            }
          }
          logStep(`acme preview ${key} cloned ${acmeUrl} at ${proposal.headSha}`);

          const merged = await page.request.post(
            `/api/v1/tenants/acme/proposals/${proposal.id}/merge`,
            { headers: SAME_ORIGIN },
          );
          expect(merged.status(), await merged.text()).toBe(200);
        }
        let stackName = existing ?? '';
        await expect
          .poll(
            () =>
              (stackName =
                tryCmd('kubectl', [
                  'get',
                  'stacks.pulumi.com',
                  '-n',
                  'tenant-acme',
                  '-o',
                  'jsonpath={.items[*].metadata.name}',
                ])
                  .stdout.split(/\s+/)
                  .find((n) => n.startsWith(ACME_CLUSTER)) ?? ''),
            { timeout: 240_000, intervals: [5_000] },
          )
          .not.toBe('');
        await waitForK8sReady('stack', stackName, 'tenant-acme', 900_000);
        const stack = kubectlJson<K8sObject>([
          'get',
          'stacks.pulumi.com',
          stackName,
          '-n',
          'tenant-acme',
        ]);
        expect(stack.spec.projectRepo).toBe(acmeUrl);
        expect(stack.spec.fluxSource).toBeUndefined();
        expect(JSON.stringify(stack.spec.gitAuth ?? {})).toContain('keel-gitops-credentials');
        logStep(`acme Stack ${stackName} Ready from ${acmeUrl}`);
      });

      await test.step('7. tenant-default and acme workloads are applied from their own repository only', async () => {
        for (const tenant of ['default', 'acme']) {
          const ns = `tenant-${tenant}`;
          const sources = kubectlJson<{ items: K8sObject[] }>([
            'get',
            'gitrepositories.source.toolkit.fluxcd.io',
            '-n',
            ns,
          ]).items.map((s) => s.metadata.name);
          expect(sources, `${ns} Git sources`).toEqual(['keel-gitops']);
          const kustomizations = kubectlJson<{ items: K8sObject[] }>([
            'get',
            'kustomizations.kustomize.toolkit.fluxcd.io',
            '-n',
            ns,
          ]).items;
          for (const ks of kustomizations) {
            const ref = ks.spec.sourceRef as { kind: string; name: string; namespace?: string };
            expect(ref.kind, `${ns}/${ks.metadata.name}`).toBe('GitRepository');
            expect(ref.name, `${ns}/${ks.metadata.name}`).toBe('keel-gitops');
            expect([undefined, ns], `${ns}/${ks.metadata.name}`).toContain(ref.namespace);
            expect(ks.spec.serviceAccountName, `${ns}/${ks.metadata.name}`).toBe(`${ns}-sa`);
          }
          const workloads = [
            ...kubectlJson<{ items: K8sObject[] }>([
              'get',
              'helmreleases.helm.toolkit.fluxcd.io',
              '-n',
              ns,
            ]).items,
            ...kubectlJson<{ items: K8sObject[] }>(['get', 'stacks.pulumi.com', '-n', ns]).items,
          ];
          expect(workloads.length, `${ns} workloads`).toBeGreaterThan(0);
          for (const obj of workloads) {
            // Applied by one of the tenant's own Kustomizations (impersonating tenant-<id>-sa).
            expect(
              obj.metadata.labels?.['kustomize.toolkit.fluxcd.io/namespace'],
              `${ns}/${obj.metadata.name} applied by a ${ns} Kustomization`,
            ).toBe(ns);
            if ('projectRepo' in obj.spec || 'fluxSource' in obj.spec) {
              expect(obj.spec.projectRepo, `${ns}/${obj.metadata.name}`).toBe(
                tenantRepoUrl(tenant),
              );
              expect(obj.spec.fluxSource, `${ns}/${obj.metadata.name}`).toBeUndefined();
            }
          }
        }
        const defaultReleases = runCmd('kubectl', [
          'get',
          'helmreleases.helm.toolkit.fluxcd.io',
          '-n',
          'tenant-default',
          '-o',
          'jsonpath={.items[*].metadata.name}',
        ]).split(/\s+/);
        expect(defaultReleases).toEqual(
          expect.arrayContaining(['podinfo', `${WORKLOAD_CLUSTER}-podinfo`]),
        );
        await waitForK8sReady('helmrelease', 'acme-podinfo', 'tenant-acme', 120_000);
        logStep('Every tenant workload is applied from its own repository');
      });
    } finally {
      fs.rmSync(workRoot, { recursive: true, force: true });
    }
  });
});
