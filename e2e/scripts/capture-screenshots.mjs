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
  {
    id: 'topology-events',
    label: 'Topology Map',
    filename: 'topology-events.png',
    scrollToBottom: true,
  },
  {
    // Unified releases table (tenant releases + protected management add-on rows for admins).
    id: 'topology-releases',
    label: 'Topology Map',
    filename: 'topology-releases.png',
    scrollToTestId: 'topology-releases-section',
  },
  { id: 'catalog', label: 'Catalog & Form Wizard', filename: 'catalog.png' },
  { id: 'preview', label: 'PR & Preview Inspector', filename: 'preview.png' },
  { id: 'providers', label: 'Control Plane & Providers', filename: 'providers.png' },
];

function readTargetFile(name) {
  const file = path.join(REPO_ROOT, 'target', name);
  return fs.existsSync(file) ? fs.readFileSync(file, 'utf-8').trim() : '';
}

/** Admin login: `KEEL_ADMIN_EMAIL` env, else `target/keel-admin-username` (written by the justfile). */
function resolveAdminEmail() {
  return (
    process.env.KEEL_ADMIN_EMAIL || readTargetFile('keel-admin-username') || 'admin@keel.local'
  );
}

/** Admin password: `KEEL_ADMIN_PASSWORD` env, else `target/keel-admin-password` (justfile). */
function resolveAdminPassword() {
  return process.env.KEEL_ADMIN_PASSWORD || readTargetFile('keel-admin-password');
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
      'Unable to resolve the admin password. Set KEEL_ADMIN_PASSWORD, or bootstrap via the justfile so target/keel-admin-password exists.',
    );
  }

  console.log(`Signing in via OIDC (Rauthy) as ${adminEmail}...`);
  await oidcBtn.click();

  const loginInput = page
    .locator(
      'input[type="email"], input[name="email"], input[name="login"], input#login, input[autocomplete="username"]',
    )
    .first();
  await loginInput.waitFor({ state: 'visible', timeout: 20_000 });
  await loginInput.fill(adminEmail);

  const passwordInput = page
    .locator('input[type="password"], input[name="password"], input#password')
    .first();
  if (!(await passwordInput.isVisible().catch(() => false))) {
    await page.locator('button[type="submit"]').first().click();
  }
  await passwordInput.waitFor({ state: 'visible', timeout: 10_000 });
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

      if (entry.scrollToTestId) {
        // Align the element with the top of the viewport, just below the sticky chrome (demo
        // banner + header), which would otherwise cover the section title. scrollIntoViewIfNeeded
        // only nudges it to the nearest edge, which would cut off most of a tall section.
        const target = page.getByTestId(entry.scrollToTestId);
        await target.waitFor({ state: 'visible', timeout: 10_000 });
        await target.evaluate((el) => {
          const doc = el.ownerDocument;
          const win = doc.defaultView;
          const header = doc.querySelector('header');
          const stickyBottom = header ? Math.max(0, header.getBoundingClientRect().bottom) : 0;
          win.scrollTo(0, el.getBoundingClientRect().top + win.scrollY - stickyBottom - 12);
        });
        await page.waitForTimeout(400);
      } else if (entry.scrollToBottom) {
        await page.evaluate('window.scrollTo(0, document.body.scrollHeight)');
        await page.waitForTimeout(400);
      } else {
        await page.evaluate('window.scrollTo(0, 0)');
      }

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
