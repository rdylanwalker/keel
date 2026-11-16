import { Buffer } from 'node:buffer';
import { execFileSync } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { chromium } from '@playwright/test';

const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);
const REPO_ROOT = path.resolve(__dirname, '../..');

const BASE_URL = (process.env.KEEL_BASE_URL || 'http://127.0.0.1:8080').replace(/\/+$/, '');
const DEMO_MODE = process.env.KEEL_DEMO_MODE !== '0';
const OUTPUT_DIR = path.resolve(
  REPO_ROOT,
  process.env.KEEL_SCREENSHOTS_DIR || 'docs/assets/screenshots',
);

const PAGES = [
  { id: 'topology', label: 'Topology Map', filename: 'topology.png' },
  { id: 'catalog', label: 'Catalog & Form Wizard', filename: 'catalog.png' },
  { id: 'preview', label: 'PR & Preview Inspector', filename: 'preview.png' },
  { id: 'providers', label: 'Control Plane & Providers', filename: 'providers.png' },
];

function tryKubectl(args) {
  try {
    return execFileSync('kubectl', args, {
      encoding: 'utf-8',
      stdio: ['ignore', 'pipe', 'ignore'],
    }).trim();
  } catch {
    return '';
  }
}

function resolveAdminEmail() {
  if (process.env.KEEL_ADMIN_EMAIL) {
    return process.env.KEEL_ADMIN_EMAIL;
  }
  const b64 = tryKubectl([
    'get',
    'secret',
    '-n',
    'keel-system',
    'dex-config',
    '-o',
    'jsonpath={.data.config\\.yaml}',
  ]);
  if (b64) {
    const decoded = Buffer.from(b64, 'base64').toString('utf-8');
    const match = decoded.match(/email:\s*(\S+)/);
    if (match?.[1]) {
      return match[1];
    }
  }
  return 'admin@keel.local';
}

function resolveAdminPassword() {
  if (process.env.KEEL_ADMIN_PASSWORD) {
    return process.env.KEEL_ADMIN_PASSWORD;
  }
  const passwordFile = path.join(REPO_ROOT, 'target/keel-dex-password');
  if (fs.existsSync(passwordFile)) {
    const fromFile = fs.readFileSync(passwordFile, 'utf-8').trim();
    if (fromFile) {
      return fromFile;
    }
  }
  const b64 = tryKubectl([
    'get',
    'secret',
    '-n',
    'keel-system',
    'dex-admin',
    '-o',
    'jsonpath={.data.password}',
  ]);
  if (b64) {
    const decoded = Buffer.from(b64, 'base64').toString('utf-8').trim();
    if (decoded) {
      return decoded;
    }
  }
  return '';
}

async function login(page) {
  const entryUrl = DEMO_MODE ? `${BASE_URL}/?demo=true` : `${BASE_URL}/`;
  console.log(`Opening ${entryUrl}...`);
  await page.goto(entryUrl, { waitUntil: 'networkidle' });

  const logoutBtn = page.getByRole('button', { name: 'Logout' });
  if (await logoutBtn.isVisible().catch(() => false)) {
    console.log('Already authenticated.');
    return;
  }

  if (DEMO_MODE) {
    const demoBtn = page.getByRole('button', { name: /Demo Login/i }).first();
    await demoBtn.waitFor({ state: 'visible', timeout: 15_000 });
    await demoBtn.click();
    await logoutBtn.waitFor({ state: 'visible', timeout: 15_000 });
    console.log('Authenticated in demo mode.');
    return;
  }

  const oidcBtn = page.getByRole('button', { name: /Sign in with OIDC/i }).first();
  await oidcBtn.waitFor({ state: 'visible', timeout: 15_000 });

  const adminEmail = resolveAdminEmail();
  const adminPassword = resolveAdminPassword();
  if (!adminPassword) {
    throw new Error(
      'Unable to resolve Dex admin password. Set KEEL_ADMIN_PASSWORD, or run `just dex-reset-password` first.',
    );
  }

  console.log(`Signing in via Dex OIDC as ${adminEmail}...`);
  await oidcBtn.click();

  const loginInput = page.locator('input[name="login"], input#login').first();
  await loginInput.waitFor({ state: 'visible', timeout: 20_000 });
  await loginInput.fill(adminEmail);

  const passwordInput = page.locator('input[name="password"], input#password').first();
  await passwordInput.fill(adminPassword);

  const submitBtn = page.locator('button[type="submit"], #submit-login').first();
  await submitBtn.click();

  await logoutBtn.waitFor({ state: 'visible', timeout: 20_000 });
  console.log('Authenticated via OIDC.');
}

async function main() {
  fs.mkdirSync(OUTPUT_DIR, { recursive: true });

  const browser = await chromium.launch({ headless: true });
  try {
    const context = await browser.newContext({
      viewport: { width: 1440, height: 900 },
    });
    const page = await context.newPage();

    await login(page);

    for (const entry of PAGES) {
      const navButton = page.getByRole('button', { name: entry.label });
      await navButton.waitFor({ state: 'visible', timeout: 10_000 });
      await navButton.click();

      // Wait for lazy-loaded view and any initial API requests to settle.
      await page.waitForLoadState('networkidle').catch(() => {});
      await page.waitForTimeout(800);

      const destPath = path.join(OUTPUT_DIR, entry.filename);
      await page.screenshot({ path: destPath, fullPage: false });
      console.log(`Saved ${path.relative(REPO_ROOT, destPath)}`);
    }
  } finally {
    await browser.close();
  }
}

main().catch((err) => {
  console.error(err instanceof Error ? err.message : err);
  process.exit(1);
});
