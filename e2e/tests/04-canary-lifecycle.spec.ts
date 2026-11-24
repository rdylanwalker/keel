import { expect, test } from '@playwright/test';

import {
  USERS,
  apiAs,
  branchFiles,
  cliLogin,
  collectSse,
  createProposal,
  openTab,
  showFile,
  uiLogin,
  uniq,
} from './support/harness.js';

/**
 * Mocked-harness scope: no cluster is connected, so Flagger analysis/rollback cannot run here.
 * These specs cover the GitOps side (Canary manifests in the PR) and the SSE transport. The live
 * suite (07-kind-live-e2e) only goes as far as a deployed Canary reaching `Initialized` (and the
 * workload cluster's canary Kustomization becoming Ready); no spec drives a canary analysis,
 * promotion or rollback, so that lifecycle is untested.
 */
test.describe('04 - Flagger Canary Lifecycle & Live SSE Event Stream', () => {
  test('04.1 - Releases and SSE stream require auth; /metrics is public', async ({ request }) => {
    expect((await request.get('/api/v1/releases')).status()).toBe(401);
    expect((await request.get('/api/v1/events/stream')).status()).toBe(401);
    const metrics = await request.get('/metrics');
    expect(metrics.status()).toBe(200);
    expect(await metrics.text()).toContain('# HELP');
  });

  test('04.2 - canary=true writes a Flagger Canary + Envoy NetworkPolicy into the PR', async () => {
    const api = await apiAs(USERS.admin);
    const name = uniq('can');
    const created = await createProposal(api, {
      tenant: 'acme',
      catalogId: 'podinfo',
      name,
      values: { replicas: 2 },
      canary: true,
    });
    const dir = `tenants/acme/apps/${name}`;
    const files = branchFiles(created.branch);
    expect(files).toEqual(
      expect.arrayContaining([
        `${dir}/canary.yaml`,
        `${dir}/networkpolicy-canary.yaml`,
        `${dir}/helmrelease-canary.yaml`,
      ]),
    );
    expect(files).not.toContain(`${dir}/helmrelease.yaml`);

    const canary = showFile(created.branch, `${dir}/canary.yaml`);
    expect(canary).toContain('apiVersion: flagger.app/v1beta1');
    expect(canary).toContain('kind: Canary');
    expect(canary).toContain(`name: ${name}`);
    expect(canary).toContain('namespace: tenant-acme');
    expect(canary).toMatch(/provider: "?gatewayapi/);
    expect(canary).toContain('envoy-gateway-request-success-rate');
    expect(canary).toMatch(/threshold: \d+/);
    // The catalog default `ingressHost` becomes the Canary host: never a catch-all route.
    expect(canary).toContain('podinfo.keel.local');

    const np = showFile(created.branch, `${dir}/networkpolicy-canary.yaml`);
    expect(np).toContain('kind: NetworkPolicy');
    expect(np).toContain('keel-system');

    const kustomization = showFile(created.branch, 'tenants/kustomization.yaml');
    expect(kustomization).toContain(`acme/apps/${name}/canary.yaml`);
    await api.dispose();
  });

  test('04.3 - canary=false keeps a plain HelmRelease; non-Flagger apps reject canary', async () => {
    const api = await apiAs(USERS.admin);
    const name = uniq('plain');
    const created = await createProposal(api, {
      tenant: 'acme',
      catalogId: 'podinfo',
      name,
      values: { replicas: 1 },
    });
    const files = branchFiles(created.branch);
    expect(files).toContain(`tenants/acme/apps/${name}/helmrelease.yaml`);
    expect(files.some((f) => f.includes('canary'))).toBe(false);

    const rejected = await api.post('/api/v1/tenants/acme/proposals', {
      data: {
        tenant: 'acme',
        catalogId: 'static-site',
        name: uniq('site'),
        values: {},
        canary: true,
      },
    });
    expect(rejected.status()).toBe(400);
    expect((await rejected.json()).error).toMatch(/does not support Flagger canary/);

    // A canary host may not be a platform host (Kyverno would deny Flagger's route).
    const platformHost = await api.post('/api/v1/tenants/acme/proposals', {
      data: {
        tenant: 'acme',
        catalogId: 'podinfo',
        name: uniq('idp'),
        values: { replicas: 1, ingressHost: 'idp.keel.local' },
        canary: true,
      },
    });
    expect(platformHost.status()).toBe(400);
    expect((await platformHost.json()).error).toMatch(/reserved platform host/);
    await api.dispose();
  });

  test('04.4 - GUI canary toggle (Radix checkbox) produces a Canary manifest in the PR', async ({
    page,
  }) => {
    await uiLogin(page, USERS.admin);
    await openTab(page, 'Catalog & Form Wizard');
    await page
      .getByTestId('catalog-list')
      .locator('.p-5', { hasText: 'Podinfo App' })
      .getByRole('button', { name: 'Create' })
      .click();
    const modal = page.getByTestId('deploy-instance-modal');
    const name = uniq('cangui');
    await modal.locator('#instance-name-input').fill(name);
    await modal.locator('#deploy-modal-tenant-select').selectOption('acme');

    const toggle = modal.getByRole('checkbox', { name: 'Enable Flagger Canary' });
    await expect(toggle).toHaveAttribute('data-state', 'unchecked');
    await toggle.click();
    await expect(toggle).toHaveAttribute('data-state', 'checked');

    const createResponse = page.waitForResponse(
      (r) =>
        r.request().method() === 'POST' && /\/api\/v1\/tenants\/[^/]+\/proposals$/.test(r.url()),
    );
    await modal.getByRole('button', { name: /Submit & Open Git PR Proposal/i }).click();
    const res = await createResponse;
    expect(res.status()).toBe(201);
    expect(JSON.parse(res.request().postData() ?? '{}')).toMatchObject({ canary: true });
    const body = await res.json();
    expect(branchFiles(body.branch)).toContain(`tenants/acme/apps/${name}/canary.yaml`);
  });

  test('04.5 - SSE stream delivers reconciler events (API)', async () => {
    const admin = await cliLogin(USERS.admin);
    const api = await apiAs(USERS.admin);
    const tenantId = uniq('sse');
    const frames = await collectSse(
      { Authorization: `Bearer ${admin.sessionToken}` },
      4000,
      async () => {
        const res = await api.post('/api/v1/tenants', { data: { id: tenantId } });
        expect(res.status(), await res.text()).toBe(201);
      },
    );
    const events = frames.map((f) => JSON.parse(f));
    const ev = events.find((e) => String(e.message ?? '').includes(`'${tenantId}'`));
    expect(ev, JSON.stringify(events)).toBeDefined();
    expect(ev.namespace).toBe(`tenant-${tenantId}`);
    expect(ev.kind).toBe('Kustomization');
    await api.dispose();
  });

  test('04.6 - GUI shows SSE connection state and streams live events', async ({ page }) => {
    await uiLogin(page, USERS.admin);
    await openTab(page, 'Topology Map');
    const status = page.getByTestId('topology-stream-status');
    await expect(status).toHaveText(/SSE Live Stream/, { timeout: 15000 });
    await expect(page.getByText('Live Reconciler Event Stream')).toBeVisible();
    await expect(page.getByText('Drifted / Error / Deleting / Orphaned')).toBeVisible();

    const api = await apiAs(USERS.admin);
    const tenantId = uniq('ssegui');
    const res = await api.post('/api/v1/tenants', { data: { id: tenantId } });
    expect(res.status()).toBe(201);
    await expect(page.getByText(new RegExp(`Proposed tenant '${tenantId}' creation`))).toBeVisible({
      timeout: 15000,
    });
    await api.dispose();
  });
});
