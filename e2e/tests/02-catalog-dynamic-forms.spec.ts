import { expect, test, type Page } from '@playwright/test';

import {
  USERS,
  apiAs,
  branchFiles,
  createProposal,
  git,
  openTab,
  showFile,
  uiLogin,
  uniq,
} from './support/harness.js';

/** Returns true when `needle` occurs anywhere in the tree of `ref` (git grep, fixed string). */
function treeContains(ref: string, needle: string): boolean {
  try {
    git(['grep', '-F', '-q', needle, ref]);
    return true;
  } catch {
    return false;
  }
}

async function openPodinfoDeployForm(page: Page) {
  await openTab(page, 'Catalog & Form Wizard');
  await expect(
    page.getByRole('heading', { name: /Self-Service Catalog & Dynamic Form Wizard/ }),
  ).toBeVisible();
  const card = page.getByTestId('catalog-list').locator('.p-5', { hasText: 'Podinfo App' });
  await card.getByRole('button', { name: 'Create' }).click();
  const modal = page.getByTestId('deploy-instance-modal');
  await expect(modal).toBeVisible();
  return modal;
}

test.describe('02 - Catalog Dynamic Forms & PR Proposal Generation', () => {
  test('02.1 - Unauthenticated catalog and proposal APIs are rejected', async ({ request }) => {
    expect((await request.get('/api/v1/catalog')).status()).toBe(401);
    const res = await request.post('/api/v1/proposals', {
      data: { catalogId: 'podinfo', tenant: 'acme', values: { replicas: 3 } },
    });
    expect(res.status()).toBe(401);
  });

  test('02.2 - Catalog API exposes nested JSONSchema with writeOnly secret fields', async () => {
    const api = await apiAs(USERS.admin);
    const res = await api.get('/api/v1/catalog');
    expect(res.status()).toBe(200);
    const body = await res.json();
    const apps = body.applications as Array<{
      metadata: { id: string };
      spec: {
        upstream: { repoURL: string; chart: string };
        schema: { properties: Record<string, Record<string, unknown>>; required?: string[] };
      };
    }>;
    const podinfo = apps.find((a) => a.metadata.id === 'podinfo');
    expect(podinfo).toBeDefined();
    const schema = podinfo!.spec.schema;
    expect(schema.required).toContain('replicas');
    expect(schema.properties.adminPassword).toMatchObject({ writeOnly: true, format: 'password' });
    expect(schema.properties.ingress).toMatchObject({ type: 'object' });
    expect(podinfo!.spec.upstream).toMatchObject({ chart: 'podinfo' });
    await api.dispose();
  });

  test('02.3 - Server-side JSON Schema validation rejects invalid values with field details', async () => {
    const api = await apiAs(USERS.admin);
    const res = await api.post('/api/v1/proposals', {
      data: { tenant: 'acme', catalogId: 'podinfo', name: uniq('bad'), values: { replicas: 0 } },
    });
    expect(res.status()).toBe(422);
    const err = await res.json();
    expect(err.details?.[0]).toMatchObject({ fieldPath: '/replicas' });
    // Friendly error envelope, never the legacy "API error (...)" prefix.
    expect(String(err.error)).not.toMatch(/^API error/);
    await api.dispose();
  });

  test('02.4 - writeOnly secrets are SOPS-encrypted in the PR branch; upstream chart unmodified', async () => {
    const api = await apiAs(USERS.admin);
    const name = uniq('sec');
    const secret = `S3cr3t-${uniq('pw')}`;
    const created = await createProposal(api, {
      tenant: 'acme',
      catalogId: 'podinfo',
      name,
      values: {
        replicas: 3,
        adminPassword: secret,
        ingress: { enabled: true, host: 'podinfo.e2e' },
      },
    });
    expect(created.proposal.title).toBe(`feat(acme): deploy ${name}`);
    expect(created.proposal.sourceBranch).toBe(created.branch);

    const files = branchFiles(created.branch);
    const dir = `tenants/acme/apps/${name}`;
    expect(files).toContain(`${dir}/secrets.enc.yaml`);
    expect(files).toContain(`${dir}/values.yaml`);

    // Plaintext secret appears nowhere in the branch tree or its commit message.
    expect(treeContains(created.branch, secret)).toBe(false);
    expect(git(['log', '-1', '--format=%B', created.branch])).not.toContain(secret);

    const enc = showFile(created.branch, `${dir}/secrets.enc.yaml`);
    expect(enc).toMatch(/adminPassword: ENC\[AES256_GCM,/);
    expect(enc).toContain('sops:');
    expect(enc).toMatch(/recipient: age1/);

    const values = showFile(created.branch, `${dir}/values.yaml`);
    expect(values).toContain('replicas: 3');
    expect(values).toContain('host: podinfo.e2e');
    expect(values).not.toContain('adminPassword');

    // Upstream chart is consumed as-is: HelmRepository -> upstream URL, chart name + pinned version.
    expect(showFile(created.branch, `${dir}/helm-repository.yaml`)).toContain(
      'https://stefanprodan.github.io/podinfo',
    );
    const hr = showFile(created.branch, `${dir}/helmrelease.yaml`);
    expect(hr).toContain('chart: podinfo');
    expect(hr).toContain('version: "6.5.0"');
    expect(hr).not.toContain(secret);

    // Nothing was committed to main (PR-only workflow).
    expect(treeContains('main', name)).toBe(false);
    await api.dispose();
  });

  test('02.5 - GUI form: required markers, client-side validation, masked secret preview, PR', async ({
    page,
  }) => {
    await uiLogin(page, USERS.admin);
    const modal = await openPodinfoDeployForm(page);

    // Required fields carry a visual " *" marker; nested object fields are rendered.
    await expect(modal.locator('label', { hasText: 'Replica Count' })).toHaveText(
      /Replica Count\s*\*$/,
    );
    await expect(modal.getByLabel('Ingress Hostname')).toBeVisible();
    await expect(modal.getByLabel('Admin Secret Password')).toHaveAttribute('type', 'password');

    const name = uniq('gui');
    await modal.locator('#instance-name-input').fill(name);
    await modal.locator('#deploy-modal-tenant-select').selectOption('acme');

    // Client-side validation blocks submission (no POST is sent).
    let posted = 0;
    page.on('request', (r) => {
      if (r.method() === 'POST' && r.url().endsWith('/api/v1/proposals')) posted++;
    });
    await modal.getByLabel('Replica Count').fill('0');
    await modal.getByLabel('Ingress Hostname').fill('Bad Host!');
    await modal.getByRole('button', { name: /Submit & Open Git PR Proposal/i }).click();
    await expect(modal.getByText('Please fix the following:')).toBeVisible();
    await expect(modal.locator('.error-detail').first()).toBeVisible();
    expect(posted).toBe(0);

    await modal.getByLabel('Replica Count').fill('4');
    await modal.getByLabel('Ingress Hostname').fill('gui.e2e.local');
    const secret = `GuiS3cret-${uniq('pw')}`;
    await modal.getByLabel('Admin Secret Password').fill(secret);

    // Live values.yaml preview masks writeOnly values and never shows the plaintext.
    const preview = modal.getByLabel('values.yaml preview');
    await expect(preview).toContainText('replicas: 4');
    await expect(preview).toContainText('host: gui.e2e.local');
    await expect(preview).toContainText('********');
    await expect(preview).not.toContainText(secret);

    const createResponse = page.waitForResponse(
      (r) => r.request().method() === 'POST' && r.url().endsWith('/api/v1/proposals'),
    );
    await modal.getByRole('button', { name: /Submit & Open Git PR Proposal/i }).click();
    const res = await createResponse;
    expect(res.status()).toBe(201);
    const body = await res.json();
    await expect(page.getByText(/Git PR Proposal Successfully Opened!/i)).toBeVisible();
    await expect(page.getByText(`feat(acme): deploy ${name}`).first()).toBeVisible();

    expect(treeContains(body.branch, secret)).toBe(false);
    expect(showFile(body.branch, `tenants/acme/apps/${name}/values.yaml`)).toContain('replicas: 4');
  });

  test('02.6 - Deploy dialog closes with Escape without opening a PR', async ({ page }) => {
    await uiLogin(page, USERS.admin);
    const modal = await openPodinfoDeployForm(page);
    await page.keyboard.press('Escape');
    await expect(modal).toHaveCount(0);
    await expect(page.getByText(/Git PR Proposal Successfully Opened!/i)).toHaveCount(0);
  });
});
