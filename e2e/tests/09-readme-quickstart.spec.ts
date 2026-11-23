/**
 * 09 - README Quick Start, executed literally.
 *
 * Static part (always runs, also in the mocked suite): the marked Quick Start blocks in README.md
 * parse, cover every step this spec knows, pin the workspace version, and every `keel`
 * subcommand / flag / `KEEL_*` variable they use exists in the built binary's `--help`.
 *
 * Live part (`pnpm --dir e2e test:live:quickstart`, KEEL_E2E_QUICKSTART_LIVE=1): runs the blocks in
 * README order with a fresh HOME, TMPDIR and kubeconfig and a minimal environment, the way a new
 * user would in one terminal (plus a second one for `keel serve`). Only the download is replaced:
 * a `curl` stand-in serves a tarball + SHA256SUMS built from the local binary, so `sha256sum -c`,
 * `tar` and `keel --version` still run as written. It then checks what the README promises: login,
 * a Ready workload cluster, the `podinfo` canary Ready on the management cluster with the catalog
 * defaults, and a clean teardown. It uses the README's cluster names (`keel-mgmt`,
 * `workload-dev`) and port 8080, so it must not run next to `07-kind-live-e2e.spec.ts`.
 */
import { expect, test, chromium, type Browser } from '@playwright/test';
import { execFileSync, spawn, spawnSync, type ChildProcess } from 'node:child_process';
import dns from 'node:dns';
import fs from 'node:fs';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';

import {
  QUICKSTART_MODES,
  helpFlags,
  helpSubcommands,
  hostsEntries,
  keelInvocations,
  parseQuickstart,
  pinnedReleaseVersion,
  readReadme,
  redactSecrets,
  shellCommands,
  type QuickstartBlock,
} from './support/quickstart.js';

const KEEL_BIN = process.env.KEEL_BIN || path.resolve(process.cwd(), '../target/debug/keel');
const QUICKSTART_LIVE = process.env.KEEL_E2E_QUICKSTART_LIVE === '1';
const BASE_URL = 'http://127.0.0.1:8080';
const MGMT_CLUSTER = 'keel-mgmt';
const WORKLOAD_CLUSTER = 'workload-dev';
/** Step ids this spec knows how to check; the static test keeps README and spec in sync. */
const REQUIRED_STEPS = [
  'install',
  'hosts',
  'bootstrap',
  'serve',
  'login',
  'workload',
  'podinfo',
  'podinfo-check',
  'cleanup',
] as const;
const KNOWN_STEPS = new Set<string>([...REQUIRED_STEPS, 'cosign']);
/** Host names `keel serve` and git resolve themselves (cannot be emulated for them). */
const SERVER_SIDE_HOSTS = ['idp.keel.local', 'forgejo.keel.local'];

function workspaceVersion(): string {
  const cargo = fs.readFileSync(path.resolve(process.cwd(), '../Cargo.toml'), 'utf8');
  const m = /\[workspace\.package\][^[]*?\nversion\s*=\s*"([^"]+)"/.exec(cargo);
  if (!m) throw new Error('cannot read [workspace.package] version from Cargo.toml');
  return m[1];
}

const helpCache = new Map<string, string>();
function keelHelp(cmdPath: string[]): string {
  const key = cmdPath.join(' ');
  if (!helpCache.has(key)) {
    helpCache.set(
      key,
      execFileSync(KEEL_BIN, [...cmdPath, '--help'], {
        encoding: 'utf8',
        env: { ...process.env, NO_COLOR: '1' },
      }),
    );
  }
  return helpCache.get(key)!;
}

test.describe('09 - README Quick Start (static)', () => {
  test('09.s1 - the marked Quick Start blocks parse, cover every step and pin the workspace version', () => {
    const readme = readReadme();
    const blocks = parseQuickstart(readme);
    const ids = blocks.map((b) => b.id);
    for (const id of REQUIRED_STEPS) expect(ids, `README quickstart step '${id}'`).toContain(id);
    for (const id of ids) {
      expect(KNOWN_STEPS.has(id), `step '${id}' has no handler in this spec`).toBe(true);
    }
    for (const b of blocks) expect(QUICKSTART_MODES).toContain(b.mode);
    // Order: install first, cleanup last, serve before login before any API command.
    expect(ids[0]).toBe('install');
    expect(blocks.at(-1)?.mode).toBe('teardown');
    expect(ids.indexOf('serve')).toBeLessThan(ids.indexOf('login'));
    expect(ids.indexOf('login')).toBeLessThan(ids.indexOf('workload'));
    expect(blocks.filter((b) => b.mode === 'serve')).toHaveLength(1);
    expect(blocks.filter((b) => b.mode === 'login')).toHaveLength(1);
    // Every block splits into commands without unterminated quotes.
    for (const b of blocks) expect(shellCommands(b.script).length).toBeGreaterThan(0);
    // Hosts block lists the names the rest of the Quick Start uses.
    const hosts = hostsEntries(blocks.find((b) => b.id === 'hosts')!.script);
    expect(hosts.ip).toBe('127.0.0.1');
    for (const name of [...SERVER_SIDE_HOSTS, 'podinfo.keel.local']) {
      expect(hosts.names).toContain(name);
    }
    // The pinned release is the version this tree builds (semantic-release bumps both).
    const version = pinnedReleaseVersion(readme);
    expect(version).toBe(workspaceVersion());
    expect(execFileSync(KEEL_BIN, ['--version'], { encoding: 'utf8' })).toContain(version);
  });

  test('09.s2 - every keel subcommand, flag and KEEL_* variable in the Quick Start exists in --help', () => {
    const blocks = parseQuickstart(readReadme());
    const invocations = keelInvocations(blocks);
    expect(invocations.length).toBeGreaterThan(5);
    const problems: string[] = [];
    const helpTexts: string[] = [];
    for (const inv of invocations) {
      const cmdPath: string[] = [];
      let i = 0;
      while (i < inv.argv.length && !inv.argv[i].startsWith('-')) {
        const subs = helpSubcommands(keelHelp(cmdPath));
        if (!subs.has(inv.argv[i])) break;
        cmdPath.push(inv.argv[i]);
        i++;
      }
      const help = keelHelp(cmdPath);
      helpTexts.push(help);
      const infoOnly = inv.argv.slice(i).some((w) => ['--help', '--version'].includes(w));
      if (helpSubcommands(help).size > 0 && !infoOnly) {
        problems.push(
          `[${inv.blockId}] \`${inv.source}\`: '${inv.argv[i] ?? ''}' is not a subcommand of \`keel ${cmdPath.join(' ')}\``,
        );
        continue;
      }
      const flags = helpFlags(help);
      for (const word of inv.argv.slice(i)) {
        if (!word.startsWith('--')) continue;
        const flag = word.split('=')[0];
        if (!flags.has(flag)) {
          problems.push(
            `[${inv.blockId}] \`${inv.source}\`: \`keel ${cmdPath.join(' ')}\` has no ${flag}`,
          );
        }
      }
    }
    for (const b of blocks) {
      for (const m of b.script.matchAll(/\bexport\s+(KEEL_[A-Z0-9_]+)=/g)) {
        if (!helpTexts.some((h) => h.includes(`env: ${m[1]}=`))) {
          problems.push(`[${b.id}] ${m[1]} is not read by any keel command it is used with`);
        }
      }
    }
    expect(problems, problems.join('\n')).toEqual([]);
  });

  test('09.s3 - the generated admin password never reaches the console or the kept logs', () => {
    const summary =
      'Admin Username:  admin@keel.local\nAdmin Password:  AoaMvyXeURwr8ep746eP\nPortal URL: x\n';
    const redacted = redactSecrets(summary);
    expect(redacted).not.toContain('AoaMvyXeURwr8ep746eP');
    expect(redacted).toContain('Admin Password:  [REDACTED]\n');
    expect(redacted).toContain('Admin Username:  admin@keel.local');
    expect(redactSecrets('Admin Password:  [PROVIDED VIA CLI]\n')).toContain('[PROVIDED VIA CLI]');
  });
});

// --------------------------------------------------------------------------------------------
// Live run
// --------------------------------------------------------------------------------------------

interface Session {
  root: string;
  home: string;
  stateDir: string;
  logDir: string;
  shimDir: string;
  releaseDir: string;
  kubeconfig: string;
  env: NodeJS.ProcessEnv;
  browserUrls: string;
}

interface RunningBlock {
  child: ChildProcess;
  output: () => string;
  done: Promise<number | null>;
}

function logStep(msg: string): void {
  console.log(`[quickstart ${new Date().toISOString().slice(11, 19)}] ${msg}`);
}

function commandExists(bin: string): boolean {
  return spawnSync('sh', ['-c', 'command -v "$1"', 'sh', bin], { stdio: 'ignore' }).status === 0;
}

function portFree(port: number): Promise<boolean> {
  return new Promise((resolve) => {
    const srv = net.createServer();
    srv.once('error', () => resolve(false));
    srv.listen(port, '127.0.0.1', () => srv.close(() => resolve(true)));
  });
}

function kindClusters(env?: NodeJS.ProcessEnv): string[] {
  const r = spawnSync('kind', ['get', 'clusters'], { encoding: 'utf8', env: env ?? process.env });
  return (r.stdout || '').split(/\r?\n/).filter(Boolean);
}

/** Minimal environment of a fresh login shell: no KUBECONFIG, KEEL_*, XDG_* or PULUMI_* leaks. */
function freshEnv(home: string, tmp: string, browserShim: string): NodeJS.ProcessEnv {
  const env: NodeJS.ProcessEnv = {
    HOME: home,
    TMPDIR: tmp,
    PATH: `${path.join(home, '.local/bin')}:${process.env.PATH ?? '/usr/bin:/bin'}`,
    USER: process.env.USER ?? 'keel',
    LOGNAME: process.env.LOGNAME ?? process.env.USER ?? 'keel',
    SHELL: '/bin/bash',
    LANG: process.env.LANG ?? 'C.UTF-8',
    TERM: 'dumb',
    NO_COLOR: '1',
    // The user's browser: records the URL `keel auth login` opens; the test browser visits it.
    BROWSER: browserShim,
  };
  // Docker client setup (contexts, credential helpers) is machine setup, not Keel state.
  const dockerConfig = process.env.DOCKER_CONFIG || path.join(os.homedir(), '.docker');
  if (fs.existsSync(dockerConfig)) env.DOCKER_CONFIG = dockerConfig;
  if (process.env.DOCKER_HOST) env.DOCKER_HOST = process.env.DOCKER_HOST;
  return env;
}

function createSession(): Session {
  const root = fs.mkdtempSync(
    path.join(process.env.KEEL_E2E_QUICKSTART_DIR || os.tmpdir(), 'keel-quickstart-'),
  );
  const home = path.join(root, 'home');
  const tmp = path.join(root, 'tmp');
  const stateDir = path.join(root, 'state');
  const logDir = path.join(root, 'logs');
  const shimDir = path.join(root, 'shim');
  const releaseDir = path.join(root, 'release');
  const binDir = path.join(root, 'bin');
  for (const d of [home, tmp, stateDir, logDir, shimDir, releaseDir, binDir]) {
    fs.mkdirSync(d, { recursive: true });
  }
  const browserUrls = path.join(stateDir, 'browser-urls');
  const browserShim = path.join(binDir, 'record-browser-url');
  fs.writeFileSync(browserShim, `#!/bin/sh\nprintf '%s\\n' "$1" >> '${browserUrls}'\n`, {
    mode: 0o755,
  });
  fs.writeFileSync(path.join(stateDir, 'pwd'), home);
  return {
    root,
    home,
    stateDir,
    logDir,
    shimDir,
    releaseDir,
    kubeconfig: path.join(home, '.kube', 'config'),
    env: freshEnv(home, tmp, browserShim),
    browserUrls,
  };
}

/**
 * Wraps a README block so consecutive blocks behave like one terminal: the working directory and
 * exported variables carry over (`persist`), and `set -e -o pipefail` fails on the first error.
 */
function wrapScript(s: Session, body: string, opts: { persist: boolean; pathPrefix?: string }) {
  const state = (f: string) => `'${path.join(s.stateDir, f)}'`;
  const lines = [
    'set -eo pipefail',
    `cd "$(cat ${state('pwd')})"`,
    `if [ -f ${state('env')} ]; then . ${state('env')}; fi`,
  ];
  if (opts.pathPrefix) lines.push(`PATH='${opts.pathPrefix}':"$PATH"`);
  lines.push('# ---- README block ----', body, '# ---- end of README block ----');
  if (opts.persist) {
    if (opts.pathPrefix) lines.push(`PATH="\${PATH#'${opts.pathPrefix}':}"`);
    lines.push(`pwd > ${state('pwd')}`, `export -p > ${state('env')}`);
  }
  return lines.join('\n') + '\n';
}

function startShell(
  s: Session,
  name: string,
  body: string,
  opts: { persist: boolean; pathPrefix?: string; quiet?: boolean },
): RunningBlock {
  const scriptFile = path.join(s.stateDir, `${name}.sh`);
  fs.writeFileSync(scriptFile, wrapScript(s, body, opts));
  const logFile = fs.createWriteStream(path.join(s.logDir, `${name}.log`), { flags: 'a' });
  const child = spawn('bash', ['--noprofile', '--norc', scriptFile], {
    env: s.env,
    cwd: s.home,
    stdio: ['ignore', 'pipe', 'pipe'],
    detached: true,
  });
  let out = '';
  // Line-buffered so a secret split across two chunks is still redacted before it is logged.
  let pending = '';
  const emit = (text: string) => {
    const safe = redactSecrets(text);
    logFile.write(safe);
    if (!opts.quiet) process.stdout.write(safe);
  };
  const onData = (d: Buffer) => {
    const text = d.toString();
    out += text;
    pending += text;
    const cut = pending.lastIndexOf('\n') + 1;
    if (cut > 0) {
      emit(pending.slice(0, cut));
      pending = pending.slice(cut);
    }
  };
  child.stdout!.on('data', onData);
  child.stderr!.on('data', onData);
  const done = new Promise<number | null>((resolve) => {
    child.on('close', (code) => {
      if (pending) emit(pending);
      pending = '';
      logFile.end();
      resolve(code);
    });
  });
  return { child, output: () => out, done };
}

async function runShell(
  s: Session,
  name: string,
  body: string,
  opts: { persist: boolean; pathPrefix?: string; quiet?: boolean; timeoutMs?: number },
): Promise<string> {
  const run = startShell(s, name, body, opts);
  const timeoutMs = opts.timeoutMs ?? 20 * 60_000;
  const code = await Promise.race([
    run.done,
    new Promise<'timeout'>((r) => setTimeout(() => r('timeout'), timeoutMs)),
  ]);
  if (code === 'timeout') {
    killGroup(run.child, 'SIGKILL');
    throw new Error(`${name} timed out after ${timeoutMs / 1000}s:\n${run.output().slice(-4000)}`);
  }
  if (code !== 0) {
    throw new Error(`${name} exited with ${code}:\n${run.output().slice(-4000)}`);
  }
  return run.output();
}

function killGroup(child: ChildProcess | null, signal: NodeJS.Signals): void {
  if (!child?.pid) return;
  try {
    process.kill(-child.pid, signal);
  } catch {
    // already gone
  }
}

/** `curl` stand-in for the install block: serves the release assets from the local tarball. */
function writeInstallShim(s: Session, version: string): void {
  const prefix = `https://github.com/rdylanwalker/keel/releases/download/v${version}/`;
  fs.writeFileSync(
    path.join(s.shimDir, 'curl'),
    `#!/bin/sh
# Stand-in for the release download in the README install step (local build, same file names).
for arg in "$@"; do
  case "$arg" in
    http://*|https://*)
      case "$arg" in
        '${prefix}'*) ;;
        *) echo "quickstart curl shim: unexpected URL $arg (expected ${prefix}...)" >&2; exit 2 ;;
      esac
      file="\${arg##*/}"
      if [ ! -f '${s.releaseDir}'/"$file" ]; then
        echo "curl: (22) The requested URL returned error: 404 ($file)" >&2; exit 22
      fi
      cp '${s.releaseDir}'/"$file" "./$file" ;;
  esac
done
`,
    { mode: 0o755 },
  );
  const stage = fs.mkdtempSync(path.join(s.root, 'stage-'));
  fs.copyFileSync(KEEL_BIN, path.join(stage, 'keel'));
  fs.chmodSync(path.join(stage, 'keel'), 0o755);
  execFileSync('tar', [
    '-czf',
    path.join(s.releaseDir, 'keel-linux-amd64.tar.gz'),
    '-C',
    stage,
    'keel',
  ]);
  fs.rmSync(stage, { recursive: true, force: true });
  const sums = execFileSync('sha256sum', ['keel-linux-amd64.tar.gz'], {
    cwd: s.releaseDir,
    encoding: 'utf8',
  });
  fs.writeFileSync(path.join(s.releaseDir, 'SHA256SUMS'), sums);
}

async function resolvesTo(name: string, ip: string): Promise<boolean> {
  try {
    const addrs = await dns.promises.lookup(name, { all: true });
    return addrs.some((a) => a.address === ip);
  } catch {
    return false;
  }
}

function kubectl(s: Session, args: string[], kubeconfig = s.kubeconfig): string {
  return execFileSync('kubectl', ['--kubeconfig', kubeconfig, ...args], {
    encoding: 'utf8',
    env: s.env,
    stdio: ['ignore', 'pipe', 'pipe'],
  }).trim();
}

function tryKubectl(s: Session, args: string[], kubeconfig = s.kubeconfig): string {
  try {
    return kubectl(s, args, kubeconfig);
  } catch {
    return '';
  }
}

function nodesReady(s: Session, kubeconfig: string): boolean {
  const out = tryKubectl(
    s,
    [
      'get',
      'nodes',
      '-o',
      'jsonpath={range .items[*]}{.status.conditions[?(@.type=="Ready")].status}{"\\n"}{end}',
    ],
    kubeconfig,
  );
  const lines = out.split('\n').filter(Boolean);
  return lines.length > 0 && lines.every((l) => l === 'True');
}

async function loginInBrowser(
  browser: Browser,
  url: string,
  email: string,
  password: string,
): Promise<void> {
  const context = await browser.newContext({ ignoreHTTPSErrors: true });
  const page = await context.newPage();
  const loopbackCallback = /^http:\/\/(127\.0\.0\.1|\[::1\]):\d+\/callback/;
  try {
    await page.goto(url, { waitUntil: 'domcontentloaded' }).catch(() => undefined);
    if (!loopbackCallback.test(page.url())) {
      const emailInput = page
        .locator('input[type="email"], input[name="email"], input[autocomplete="username"]')
        .first();
      await emailInput.fill(email, { timeout: 30_000 });
      const passwordInput = page.locator('input[type="password"], input[name="password"]').first();
      if (!(await passwordInput.isVisible({ timeout: 2_000 }).catch(() => false))) {
        await page.locator('button[type="submit"]').first().click({ timeout: 10_000 });
      }
      await passwordInput.fill(password, { timeout: 15_000 });
      await page.locator('button[type="submit"], #submit-login').first().click({ timeout: 10_000 });
      await page.waitForURL(loopbackCallback, { timeout: 30_000 });
    }
    await expect(page.locator('body')).toContainText('login complete', { timeout: 10_000 });
  } finally {
    await context.close();
  }
}

test.describe('09 - README Quick Start (live, executed literally)', () => {
  test.describe.configure({ mode: 'serial' });
  test.skip(
    !QUICKSTART_LIVE,
    'Run `pnpm --dir e2e test:live:quickstart` (KEEL_E2E_QUICKSTART_LIVE=1) to execute the README Quick Start',
  );

  let blocks: QuickstartBlock[] = [];
  try {
    blocks = parseQuickstart(readReadme());
  } catch {
    // Reported by the static tests above.
  }

  let s: Session;
  let serve: RunningBlock | null = null;
  let browser: Browser | null = null;
  /** Host names not in /etc/hosts that the test browser and curl map to the hosts block's IP. */
  let emulatedHosts: string[] = [];
  let hostsIp = '127.0.0.1';
  let adminUsername = '';
  let adminPassword = '';
  let tornDown = false;
  const timings: { step: string; seconds: number }[] = [];

  const time = async <T>(step: string, fn: () => Promise<T>): Promise<T> => {
    const t0 = Date.now();
    try {
      return await fn();
    } finally {
      const seconds = Math.round((Date.now() - t0) / 100) / 10;
      timings.push({ step, seconds });
      logStep(`${step}: ${seconds}s`);
    }
  };

  test.beforeAll(async () => {
    const missing = [
      'docker',
      'kind',
      'pulumi',
      'helm',
      'kubectl',
      'git',
      'sops',
      'age-keygen',
    ].filter((b) => !commandExists(b));
    expect(missing, 'README Requirements: tools on PATH').toEqual([]);
    expect(fs.existsSync(KEEL_BIN), `build the binary first (just build): ${KEEL_BIN}`).toBe(true);
    for (const port of [8080, 8888]) {
      expect(await portFree(port), `README Requirements: host port ${port} must be free`).toBe(
        true,
      );
    }
    // Created first so every `kind` call below runs with the fresh HOME and no KUBECONFIG:
    // `kind delete cluster` edits the kubeconfig and must never touch the runner's own.
    s = createSession();
    const stale = kindClusters(s.env).filter((c) => c === MGMT_CLUSTER || c === WORKLOAD_CLUSTER);
    if (stale.length) {
      expect(
        process.env.KEEL_E2E_REUSE_CLUSTER,
        `kind clusters ${stale.join(', ')} exist; the Quick Start needs a fresh start (set KEEL_E2E_REUSE_CLUSTER=0 to delete them)`,
      ).toBe('0');
      for (const c of stale) {
        logStep(`Deleting existing kind cluster ${c} (KEEL_E2E_REUSE_CLUSTER=0)...`);
        execFileSync('kind', ['delete', 'cluster', '--name', c], { stdio: 'inherit', env: s.env });
      }
    }
    logStep(`Fresh HOME ${s.home}; logs in ${s.logDir}`);
  });

  test.afterAll(async () => {
    await browser?.close().catch(() => undefined);
    if (!tornDown && s) {
      killGroup(serve?.child ?? null, 'SIGINT');
      if (process.env.KEEL_E2E_QUICKSTART_KEEP === '1') {
        console.log(
          `Quick Start state kept (KEEL_E2E_QUICKSTART_KEEP=1): HOME=${s.home}; clean up with: kind delete cluster --name ${WORKLOAD_CLUSTER} && kind delete cluster --name ${MGMT_CLUSTER}`,
        );
      } else {
        for (const c of [WORKLOAD_CLUSTER, MGMT_CLUSTER]) {
          spawnSync('kind', ['delete', 'cluster', '--name', c], { stdio: 'inherit', env: s.env });
        }
      }
    }
    if (timings.length) {
      console.log('\nREADME Quick Start timings:');
      for (const t of timings) console.log(`  ${t.step.padEnd(34)} ${t.seconds.toFixed(1)}s`);
      if (s)
        fs.writeFileSync(path.join(s.logDir, 'timings.json'), JSON.stringify(timings, null, 2));
    }
    if (s && tornDown) {
      // The fresh HOME (Pulumi plugins, Helm cache, ~700 MB) and the release tarball are only
      // needed to debug a failure; keep just the logs.
      for (const d of [s.home, s.releaseDir, s.env.TMPDIR!]) {
        fs.rmSync(d, { recursive: true, force: true });
      }
    }
    if (s) console.log(`Quick Start logs: ${s.logDir}`);
  });

  /** Starts the README `keel serve` block (the "second terminal") and waits until it serves. */
  const startServe = async (b: QuickstartBlock, name = b.id): Promise<void> => {
    const srv = startShell(s, name, b.script, { persist: false, quiet: true });
    serve = srv;
    await expect
      .poll(
        async () => {
          if (srv.child.exitCode !== null) {
            throw new Error(`keel serve exited early:\n${srv.output().slice(-4000)}`);
          }
          try {
            const r = await fetch(`${BASE_URL}/api/v1/auth/status`);
            return r.ok ? ((await r.json()) as { oidcEnabled?: boolean }).oidcEnabled : false;
          } catch {
            return false;
          }
        },
        { timeout: 120_000, intervals: [1_000], message: 'keel serve with OIDC on :8080' },
      )
      .toBe(true);
    // The Envoy Gateway port-forward behind https://*.keel.local:8888.
    await expect.poll(() => portFree(8888), { timeout: 60_000 }).toBe(false);
  };

  /**
   * Signals only the `keel serve` process, not its process group. A terminal's Ctrl+C reaches
   * the whole foreground group, kubectl included, so a group signal would kill the port-forwards
   * even if keel leaked them. Signalling keel alone proves that keel itself stops its
   * `kubectl port-forward` children, frees the ports and removes the working copy's credential.
   */
  const stopServeWith = async (signal: NodeJS.Signals): Promise<void> => {
    const srv = serve!;
    const pgid = srv.child.pid!;
    const pids = (args: string[]) =>
      (spawnSync('pgrep', args, { encoding: 'utf8' }).stdout || '')
        .split('\n')
        .filter(Boolean)
        .map(Number);
    const [keelPid] = pids(['-g', String(pgid), '-x', 'keel']);
    expect(keelPid, `keel serve process in group ${pgid}`).toBeGreaterThan(0);
    const forwards = pids(['-P', String(keelPid), '-f', 'kubectl.*port-forward']);
    expect(
      forwards.length,
      'keel serve runs the Forgejo and Envoy Gateway port-forwards',
    ).toBeGreaterThanOrEqual(2);
    const workdir = path.join(s.home, 'keel-try/keel-gitops');
    const origin = execFileSync('git', ['-C', workdir, 'remote', 'get-url', 'origin'], {
      encoding: 'utf8',
      env: s.env,
    }).trim();
    const forgejoPort = Number(/^http:\/\/127\.0\.0\.1:(\d+)\//.exec(origin)?.[1]);
    expect(forgejoPort, `origin points at the Forgejo port-forward: ${origin}`).toBeGreaterThan(0);

    process.kill(keelPid, signal);
    const code = await Promise.race([
      srv.done,
      new Promise<'timeout'>((r) => setTimeout(() => r('timeout'), 60_000)),
    ]);
    expect(code, `keel serve did not stop on ${signal}:\n${srv.output().slice(-2000)}`).not.toBe(
      'timeout',
    );
    expect(srv.output(), 'keel serve logs the signal it shut down on').toContain(
      `Received ${signal}; shutting down`,
    );
    // No orphaned `kubectl port-forward` keeps a port: nothing left in the group, ports free.
    await expect.poll(() => pids(['-g', String(pgid)]), { timeout: 20_000 }).toEqual([]);
    for (const pid of forwards) {
      expect(spawnSync('kill', ['-0', String(pid)]).status, `port-forward ${pid} is gone`).not.toBe(
        0,
      );
    }
    for (const port of [8080, 8888, forgejoPort]) {
      await expect.poll(() => portFree(port), { timeout: 20_000 }).toBe(true);
    }
    expect(fs.existsSync(path.join(workdir, '.git/keel-credentials'))).toBe(false);
    serve = null;
  };

  const handlers: Record<string, (b: QuickstartBlock) => Promise<void>> = {
    install: async (b) => {
      const version = pinnedReleaseVersion(readReadme());
      writeInstallShim(s, version);
      const out = await time('1. install', () =>
        runShell(s, b.id, b.script, { persist: true, pathPrefix: s.shimDir }),
      );
      expect(out).toMatch(/keel-linux-amd64\.tar\.gz: OK/);
      expect(out).toContain(version);
      const which = await runShell(s, 'which-keel', 'command -v keel', {
        persist: false,
        quiet: true,
      });
      expect(which.trim()).toBe(path.join(s.home, '.local/bin/keel'));
    },

    cosign: async () => {
      logStep('Skipping the optional cosign verification (needs a published release bundle)');
    },

    hosts: async (b) => {
      const { ip, names } = hostsEntries(b.script);
      hostsIp = ip;
      const missing: string[] = [];
      for (const n of names) if (!(await resolvesTo(n, ip))) missing.push(n);
      if (missing.length === 0) {
        logStep(`hosts: ${names.join(' ')} already resolve to ${ip}; not appending again`);
        return;
      }
      if (spawnSync('sudo', ['-n', 'true'], { stdio: 'ignore' }).status === 0) {
        await time('2. hosts (sudo)', () => runShell(s, b.id, b.script, { persist: true }));
        for (const n of names) expect(await resolvesTo(n, ip), `${n} after step 2`).toBe(true);
        return;
      }
      const serverSide = missing.filter((n) => SERVER_SIDE_HOSTS.includes(n));
      expect(
        serverSide,
        `keel serve and git resolve these themselves; add '${ip} ${serverSide.join(' ')}' to /etc/hosts or allow passwordless sudo`,
      ).toEqual([]);
      // Browser-only names: the test browser maps them, and curl gets a ~/.curlrc stand-in.
      emulatedHosts = missing;
      fs.writeFileSync(
        path.join(s.home, '.curlrc'),
        missing.map((n) => `resolve = "${n}:8888:${ip}"\n`).join(''),
      );
      logStep(`hosts: no sudo; emulating ${missing.join(' ')} for the browser and curl only`);
    },

    bootstrap: async (b) => {
      const out = await time('3. bootstrap init', () =>
        runShell(s, b.id, b.script, { persist: true, timeoutMs: 25 * 60_000 }),
      );
      adminUsername = /Admin Username:\s*(\S+)/.exec(out)?.[1] ?? '';
      adminPassword = /Admin Password:\s*(\S+)/.exec(out)?.[1] ?? '';
      expect(adminUsername, 'bootstrap summary prints the admin username').not.toBe('');
      expect(adminPassword, 'bootstrap summary prints the admin password').not.toMatch(
        /^$|^\[PROVIDED/,
      );
      // The console copy is redacted already; also mask any later echo in the public CI log.
      if (process.env.GITHUB_ACTIONS === 'true') console.log(`::add-mask::${adminPassword}`);
      expect(kindClusters(s.env)).toContain(MGMT_CLUSTER);
      expect(nodesReady(s, s.kubeconfig), 'management cluster nodes Ready').toBe(true);
      expect(fs.existsSync(path.join(s.home, 'keel-try/keel-gitops/.git'))).toBe(true);
    },

    serve: async (b) => {
      await time('4. keel serve ready', () => startServe(b));
    },

    login: async (b) => {
      const hostRules = [...new Set([...SERVER_SIDE_HOSTS, ...emulatedHosts])]
        .map((n) => `MAP ${n} ${hostsIp}`)
        .join(', ');
      browser = await chromium.launch({ args: [`--host-resolver-rules=${hostRules}`] });
      await time('5. keel auth login', async () => {
        const run = startShell(s, b.id, b.script, { persist: true });
        try {
          await expect
            .poll(() => fs.existsSync(s.browserUrls), {
              timeout: 30_000,
              message: `keel auth login opened no browser:\n${run.output()}`,
            })
            .toBe(true);
          const url = fs.readFileSync(s.browserUrls, 'utf8').trim().split('\n').at(-1)!;
          expect(url).toMatch(/^http:\/\/127\.0\.0\.1:8080\/api\/v1\/auth\/cli\/login\?/);
          expect(run.output()).toContain(url);
          await loginInBrowser(browser!, url, adminUsername, adminPassword);
          const code = await Promise.race([
            run.done,
            new Promise<'timeout'>((r) => setTimeout(() => r('timeout'), 60_000)),
          ]);
          expect(code, run.output()).toBe(0);
        } finally {
          killGroup(run.child, 'SIGTERM');
        }
      });
      const status = await runShell(s, 'auth-status', 'keel auth status', { persist: false });
      expect(status).toContain(adminUsername);
    },

    workload: async (b) => {
      const out = await time('6. provision + preview + merge', () =>
        runShell(s, b.id, b.script, { persist: true, timeoutMs: 30 * 60_000 }),
      );
      expect(out, 'the README hard-codes PR 1').toMatch(/Opened Proposal PR #1:/);
      expect(out, 'preview status --watch ends with the real PKO result').toMatch(
        /PKO Preview:\s+Succeeded/,
      );
      expect(out, 'the preview plans the kind cluster').toMatch(
        /\+ Create\s+kind:index\/cluster:Cluster/,
      );
      await time('6. merge -> workload-dev nodes Ready', async () => {
        await expect
          .poll(() => kindClusters(s.env), { timeout: 15 * 60_000, intervals: [5_000] })
          .toContain(WORKLOAD_CLUSTER);
        const wlKubeconfig = path.join(s.root, 'workload-dev.kubeconfig');
        fs.writeFileSync(
          wlKubeconfig,
          execFileSync('kind', ['get', 'kubeconfig', '--name', WORKLOAD_CLUSTER], {
            encoding: 'utf8',
            env: s.env,
          }),
        );
        await expect
          .poll(() => nodesReady(s, wlKubeconfig), { timeout: 5 * 60_000, intervals: [5_000] })
          .toBe(true);
        const nodes = tryKubectl(s, ['get', 'nodes', '-o', 'name'], wlKubeconfig);
        expect(nodes.split('\n').filter(Boolean), 'control plane + workerCount 1').toHaveLength(2);
      });
    },

    podinfo: async (b) => {
      const out = await time('7. podinfo install + merge', () =>
        runShell(s, b.id, b.script, { persist: true, timeoutMs: 10 * 60_000 }),
      );
      expect(out, 'the README hard-codes PR 2').toMatch(/Opened Proposal PR #2:/);
    },

    'podinfo-check': async (b) => {
      let attempts = 0;
      let last = '';
      await time('7. merge -> podinfo served', async () => {
        await expect
          .poll(
            async () => {
              attempts++;
              try {
                last = await runShell(s, `${b.id}-${attempts}`, b.script, {
                  persist: false,
                  quiet: true,
                  timeoutMs: 60_000,
                });
                return true;
              } catch (e) {
                last = String(e);
                return false;
              }
            },
            {
              timeout: 15 * 60_000,
              intervals: [10_000],
              message: 'podinfo never became reachable (see the last attempt below)',
            },
          )
          .toBe(true)
          .catch((e: unknown) => {
            throw new Error(`${String(e)}\nlast attempt:\n${last}`);
          });
      });
      process.stdout.write(last);
      expect(last, 'curl prints podinfo /version').toMatch(/"version"/);
      const releases = JSON.parse(
        await runShell(s, 'app-list-json', 'keel app list --tenant default --output json', {
          persist: false,
          quiet: true,
        }),
      ) as Record<string, unknown>[];
      const podinfo = releases.find((r) => r.name === 'podinfo');
      expect(podinfo, JSON.stringify(releases)).toBeTruthy();
      expect(JSON.stringify(podinfo)).toMatch(/"(status|phase)":"Ready"|"ready":true/);
      // A canary, with the catalog defaults (restricted securityContext) applied server-side.
      await expect
        .poll(
          () =>
            tryKubectl(s, [
              'get',
              'canaries.flagger.app',
              'podinfo',
              '-n',
              'tenant-default',
              '-o',
              'jsonpath={.status.phase}',
            ]),
          { timeout: 5 * 60_000, intervals: [5_000] },
        )
        .toMatch(/^(Initialized|Succeeded)$/);
      const securityContext = tryKubectl(s, [
        'get',
        'deployment',
        'podinfo-primary',
        '-n',
        'tenant-default',
        '-o',
        'jsonpath={.spec.template.spec.containers[0].securityContext}',
      ]);
      expect(securityContext).toContain('"runAsNonRoot":true');
      // The tenant identity Flux applies the tenant with may manage the Canary through the
      // shipped aggregated role (v0.12.0 needed a hand-applied workaround role for this).
      expect(
        tryKubectl(s, ['get', 'clusterrole', 'keel-tenant-flagger-canaries', '-o', 'name']),
      ).toBe('clusterrole.rbac.authorization.k8s.io/keel-tenant-flagger-canaries');
      const tenantSa = kubectl(s, [
        'get',
        'kustomizations.kustomize.toolkit.fluxcd.io',
        '-n',
        'tenant-default',
        '-o',
        'jsonpath={.items[*].spec.serviceAccountName}',
      ])
        .split(/\s+/)
        .find(Boolean);
      expect(tenantSa, 'tenant Kustomizations impersonate a tenant ServiceAccount').toBeTruthy();
      expect(
        tryKubectl(s, [
          'auth',
          'can-i',
          'patch',
          'canaries.flagger.app',
          '-n',
          'tenant-default',
          `--as=system:serviceaccount:tenant-default:${tenantSa}`,
        ]),
      ).toBe('yes');
    },

    cleanup: async (b) => {
      // "Stop keel serve with Ctrl+C first." (SIGINT), then the same for SIGTERM (`kill`, a
      // closed terminal, a service manager), which docs/getting-started/bootstrap.md documents.
      expect(serve, 'keel serve is still running from step 4').not.toBeNull();
      await time('cleanup: Ctrl+C (SIGINT) keel serve', () => stopServeWith('SIGINT'));
      const serveBlock = blocks.find((x) => x.id === 'serve')!;
      await time('cleanup: restart + SIGTERM keel serve', async () => {
        await startServe(serveBlock, 'serve-sigterm');
        await stopServeWith('SIGTERM');
      });
      await time('cleanup: README commands', () =>
        runShell(s, b.id, b.script, { persist: true, timeoutMs: 10 * 60_000 }),
      );
      tornDown = true;
      const left = kindClusters(s.env);
      expect(left).not.toContain(WORKLOAD_CLUSTER);
      expect(left).not.toContain(MGMT_CLUSTER);
      expect(fs.existsSync(path.join(s.home, 'keel-try'))).toBe(false);
      expect(fs.existsSync(path.join(s.home, '.config/keel/stage0', MGMT_CLUSTER))).toBe(false);
    },
  };

  for (const [n, b] of blocks.entries()) {
    test(`09.${n + 1} - README step '${b.id}' (${b.mode})`, async () => {
      test.setTimeout(40 * 60_000);
      const handler = handlers[b.id];
      expect(handler, `no handler for README step '${b.id}'`).toBeTruthy();
      logStep(`README step '${b.id}' (README.md:${b.line})`);
      await handler(b);
    });
  }
});
