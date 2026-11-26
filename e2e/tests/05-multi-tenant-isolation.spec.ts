import { expect, test, type APIRequestContext } from '@playwright/test';

import {
  USERS,
  apiAs,
  cliLogin,
  collectSse,
  createProposal,
  openTab,
  proposalPath,
  tenantProposalsPath,
  uiLogin,
  uniq,
} from './support/harness.js';

/**
 * Tenant-A = `acme` (users `acme-dev` member, `acme-admin` tenant admin); Tenant-B = a fresh
 * tenant created and merged by the cluster admin. Kyverno admission is exercised by the live
 * suite (07); the mocked harness verifies the API and GUI layers.
 */
test.describe('05 - Multi-Tenant Namespace & Secret Isolation Guards', () => {
  test.describe.configure({ mode: 'serial' });

  let admin: APIRequestContext;
  let acmeDev: APIRequestContext;
  let acmeAdmin: APIRequestContext;
  let tenantB: string;
  let tenantBProposalId: string;
  let acmeProposalId: string;

  test.beforeAll(async () => {
    admin = await apiAs(USERS.admin);
    acmeDev = await apiAs(USERS.acmeDev);
    acmeAdmin = await apiAs(USERS.acmeAdmin);

    tenantB = uniq('tb');
    const created = await admin.post('/api/v1/tenants', { data: { id: tenantB } });
    expect(created.status(), await created.text()).toBe(201);
    const body = await created.json();
    expect(body.proposal.title).toBe(`feat(${tenantB}): create tenant ${tenantB}`);
    // The tenant stub PR lives in the platform repository (scope `management`). The mocked
    // harness uses the Local Git provider: tenants share the platform repository (dev only).
    expect(body.proposal.scope).toBe('management');
    expect(body.repository.layout).toBe('monorepo');
    const merged = await admin.post(proposalPath(body.proposal, 'merge'));
    expect(merged.status(), await merged.text()).toBe(200);

    const bApp = await createProposal(admin, {
      tenant: tenantB,
      catalogId: 'podinfo',
      name: uniq('bapp'),
      values: { replicas: 1 },
    });
    tenantBProposalId = bApp.proposal.id;

    const aApp = await createProposal(acmeDev, {
      tenant: 'acme',
      catalogId: 'podinfo',
      name: uniq('aapp'),
      values: { replicas: 1 },
    });
    acmeProposalId = aApp.proposal.id;
  });

  test.afterAll(async () => {
    await Promise.all([admin?.dispose(), acmeDev?.dispose(), acmeAdmin?.dispose()]);
  });

  test('05.1 - Unauthenticated tenant-scoped reads are rejected', async ({ request }) => {
    for (const p of ['/api/v1/stacks?tenant=acme', '/api/v1/releases?tenant=acme']) {
      expect((await request.get(p)).status(), p).toBe(401);
    }
  });

  test('05.2 - Tenant member lists contain only their own tenant', async () => {
    const tenants = (await (await acmeDev.get('/api/v1/tenants')).json()).tenants;
    expect(tenants).toEqual([{ id: 'acme', namespace: 'tenant-acme' }]);
    const adminTenants = (await (await admin.get('/api/v1/tenants')).json()).tenants;
    expect(adminTenants.map((t: { id: string }) => t.id)).toContain(tenantB);

    const stacks = (await (await acmeDev.get('/api/v1/stacks')).json()).stacks as Array<{
      tenant: string;
      namespace: string;
    }>;
    expect(stacks.length).toBeGreaterThan(0);
    for (const s of stacks) {
      expect(s.tenant).toBe('acme');
      expect(s.namespace).toBe('tenant-acme');
    }
    // The management stack (tenant "management") is visible to cluster admins only.
    const adminStacks = (await (await admin.get('/api/v1/stacks')).json()).stacks;
    expect(adminStacks).toEqual(
      expect.arrayContaining([
        expect.objectContaining({ name: 'management-cluster-core', tenant: 'management' }),
      ]),
    );

    const releases = (await (await acmeDev.get('/api/v1/releases')).json()).releases as Array<{
      namespace: string;
    }>;
    for (const r of releases) expect(r.namespace).toBe('tenant-acme');

    for (const listing of [
      await acmeDev.get('/api/v1/proposals?status=all'),
      await acmeDev.get(`${tenantProposalsPath('acme')}?status=all`),
    ]) {
      const proposals = (await listing.json()).proposals as Array<{
        id: string;
        tenant: string;
        scope: string;
      }>;
      expect(proposals.some((p) => p.id === acmeProposalId)).toBe(true);
      expect(proposals.some((p) => p.id === tenantBProposalId)).toBe(false);
      for (const p of proposals) {
        expect(p.tenant).toBe('acme');
        expect(p.scope).toBe('acme');
      }
    }
    // Admins see every repository, the platform scope included.
    const all = (await (await admin.get('/api/v1/proposals?status=all')).json())
      .proposals as Array<{
      scope: string;
    }>;
    expect(all.some((p) => p.scope === 'management')).toBe(true);
    expect(all.some((p) => p.scope === tenantB)).toBe(true);
  });

  test('05.3 - Cross-tenant reads and writes are rejected with 403', async () => {
    const forbidden: Array<[string, Promise<{ status(): number }>]> = [
      ['stacks', acmeDev.get(`/api/v1/stacks?tenant=${tenantB}`)],
      ['releases', acmeDev.get(`/api/v1/releases?tenant=${tenantB}`)],
      ['proposals', acmeDev.get(tenantProposalsPath(tenantB))],
      ['platform proposals', acmeAdmin.get(tenantProposalsPath('management'))],
      [
        'create proposal',
        acmeDev.post(tenantProposalsPath(tenantB), {
          data: { tenant: tenantB, catalogId: 'podinfo', name: uniq('x'), values: { replicas: 1 } },
        }),
      ],
      ['ssh-info', acmeDev.get(`/api/v1/stacks/tenant-${tenantB}/any/ssh-info`)],
      [
        'merge B PR as acme admin',
        acmeAdmin.post(proposalPath({ scope: tenantB, id: tenantBProposalId }, 'merge')),
      ],
      [
        'reject B PR as acme admin',
        acmeAdmin.post(proposalPath({ scope: tenantB, id: tenantBProposalId }, 'reject')),
      ],
      [
        'create tenant as tenant admin',
        acmeAdmin.post('/api/v1/tenants', { data: { id: uniq('g') } }),
      ],
    ];
    for (const [name, p] of forbidden) {
      expect((await p).status(), name).toBe(403);
    }
    const preview = await acmeDev.get(
      proposalPath({ scope: tenantB, id: tenantBProposalId }, 'preview'),
    );
    expect(preview.status()).toBe(403);
    // Addressing tenant B's PR through the caller's own scope never reaches it.
    const viaOwnScope = await acmeDev.get(
      proposalPath({ scope: 'acme', id: tenantBProposalId }, 'preview'),
    );
    expect([403, 404]).toContain(viaOwnScope.status());
    const mergeViaOwnScope = await acmeAdmin.post(
      proposalPath({ scope: 'acme', id: tenantBProposalId }, 'merge'),
    );
    expect([403, 404]).toContain(mergeViaOwnScope.status());
    const err = await (await acmeDev.get(`/api/v1/stacks?tenant=${tenantB}`)).json();
    expect(err.error).toBe(`Access denied for tenant '${tenantB}'`);
  });

  test('05.4 - Privileged actions need tenant-admin; management stack needs cluster admin', async () => {
    for (const action of ['force-delete', 'unlock', 'restart']) {
      const member = await acmeDev.post(`/api/v1/stacks/tenant-acme/acme-net/${action}`);
      expect(member.status(), `member ${action}`).toBe(403);
      // Tenant admin passes RBAC (the mocked harness has no live cluster to act on).
      const tAdmin = await acmeAdmin.post(`/api/v1/stacks/tenant-acme/acme-net/${action}`);
      expect(tAdmin.status(), `tenant-admin ${action}`).not.toBe(403);
      const mgmt = await acmeAdmin.post(
        `/api/v1/stacks/flux-system/management-cluster-core/${action}`,
      );
      expect(mgmt.status(), `tenant-admin mgmt ${action}`).toBe(403);
    }
    const protectedDelete = await admin.post(
      '/api/v1/stacks/flux-system/management-cluster-core/force-delete',
    );
    expect(protectedDelete.status()).toBe(409);

    expect((await admin.get('/api/v1/stacks/tenant-acme/nope/ssh-info')).status()).toBe(404);
    expect((await admin.get('/api/v1/stacks/tenant-acme/acme-net/ssh-info')).status()).toBe(422);

    const schema = await acmeDev.post('/api/v1/catalog/fetch-schema', {
      data: { repoUrl: 'https://example.invalid/charts', chart: 'x', version: '1.0.0' },
    });
    expect(schema.status()).toBe(403);

    // Members can propose but not merge; their tenant admin can merge.
    const acmePr = { scope: 'acme', id: acmeProposalId };
    expect((await acmeDev.post(proposalPath(acmePr, 'merge'))).status()).toBe(403);
    const merged = await acmeAdmin.post(proposalPath(acmePr, 'merge'));
    expect(merged.status(), await merged.text()).toBe(200);
  });

  test('05.5 - SSE stream never leaks other tenants’ events', async () => {
    const adminSession = await cliLogin(USERS.admin);
    const devSession = await cliLogin(USERS.acmeDev);
    const otherTenant = uniq('tc');
    const marker = `'${otherTenant}'`;
    const [adminFrames, devFrames, devScopedFrames] = await Promise.all([
      collectSse({ Authorization: `Bearer ${adminSession.sessionToken}` }, 4000),
      collectSse({ Authorization: `Bearer ${devSession.sessionToken}` }, 4000),
      // Explicit namespace subscription to another tenant yields an empty stream.
      collectSse({ Authorization: `Bearer ${devSession.sessionToken}` }, 4000, undefined, {
        namespace: `tenant-${otherTenant}`,
      }),
      (async () => {
        await new Promise((r) => setTimeout(r, 1000));
        const res = await admin.post('/api/v1/tenants', { data: { id: otherTenant } });
        expect(res.status()).toBe(201);
      })(),
    ]);
    expect(adminFrames.some((f) => f.includes(marker))).toBe(true);
    expect(devFrames.some((f) => f.includes(marker))).toBe(false);
    expect(devScopedFrames).toEqual([]);
    for (const f of devFrames) expect(JSON.parse(f).namespace).toBe('tenant-acme');
  });

  test('05.6 - GUI hides admin controls and other tenants from a tenant member', async ({
    page,
  }) => {
    await uiLogin(page, USERS.acmeDev);
    await expect(page.getByText(USERS.acmeDev.name)).toBeVisible();
    await expect(page.locator('header').getByText('User', { exact: true })).toBeVisible();
    await expect(page.locator('header').getByText('Admin', { exact: true })).toHaveCount(0);
    // Single tenant: read-only tenant label instead of a switcher.
    await expect(page.locator('#header-tenant-select')).toHaveCount(0);
    await expect(page.locator('header')).toContainText('acme');

    await openTab(page, 'Catalog & Form Wizard');
    await expect(page.getByTestId('catalog-list')).toContainText('Podinfo App');
    await expect(page.getByRole('button', { name: /New Tenant/i })).toHaveCount(0);
    await expect(page.getByRole('button', { name: /Add Catalog Item/i })).toHaveCount(0);
    await expect(page.getByRole('button', { name: /Remove tenant/i })).toHaveCount(0);
    await expect(
      page.getByTestId('catalog-list').getByRole('button', { name: 'Edit' }),
    ).toHaveCount(0);
    const tenantOptions = page.locator('#tenant-select-catalog option');
    await expect(tenantOptions).not.toContainText([tenantB]);

    // Own PR is visible but cannot be merged/rejected; tenant B's PR is absent.
    const own = await createProposal(acmeDev, {
      tenant: 'acme',
      catalogId: 'podinfo',
      name: uniq('own'),
      values: { replicas: 1 },
    });
    await page.goto(`/#preview/acme/${own.proposal.id}`);
    await expect(
      page.getByRole('heading', { name: `PR #${own.proposal.id}: ${own.proposal.title}` }),
    ).toBeVisible();
    await expect(page.getByRole('button', { name: 'Merge PR' })).toBeDisabled();
    await expect(page.getByRole('button', { name: 'Reject PR' })).toBeDisabled();
    await expect(page.getByTestId('pr-actions-permission-hint')).toContainText('tenant admin');
    await expect(page.getByTestId('pr-list')).not.toContainText(tenantB);

    await page.goto(`/#preview/${tenantB}/${tenantBProposalId}`);
    await expect(page.getByTestId('pr-deeplink-missing')).toBeVisible();

    await openTab(page, 'Topology Map');
    await expect(page.getByTestId('management-plane-strip')).toHaveCount(0);
    await expect(page.locator('[data-testid^="management-addon-row-"]')).toHaveCount(0);
    await expect(page.locator('main')).not.toContainText(tenantB);
  });

  test('05.7 - GUI grants a tenant admin merge rights and catalog authoring in their tenant', async ({
    page,
  }) => {
    const pr = await createProposal(acmeDev, {
      tenant: 'acme',
      catalogId: 'podinfo',
      name: uniq('tadm'),
      values: { replicas: 1 },
    });
    await uiLogin(page, USERS.acmeAdmin);
    await openTab(page, 'Catalog & Form Wizard');
    await expect(page.getByRole('button', { name: /Add Catalog Item/i })).toBeVisible();
    await expect(page.getByRole('button', { name: /New Tenant/i })).toHaveCount(0);

    await page.goto(`/#preview/acme/${pr.proposal.id}`);
    await expect(page.getByRole('button', { name: 'Merge PR' })).toBeEnabled();
    await page.getByRole('button', { name: 'Merge PR' }).click();
    await expect(page.getByRole('button', { name: 'Merged', exact: true })).toBeVisible();
  });

  test('05.8 - Cluster admin sees the management plane strip and all tenants', async ({ page }) => {
    await uiLogin(page, USERS.admin);
    await expect(page.locator('header').getByText('Admin', { exact: true })).toBeVisible();
    await expect(page.getByTestId('management-plane-strip')).toBeVisible();
    await expect(
      page
        .getByTestId('topology-releases-section')
        .getByTestId('management-addon-row-s3-state-backend'),
    ).toBeVisible();
    await expect(page.locator('#header-tenant-select option', { hasText: tenantB })).toHaveCount(1);
  });

  test('05.9 - Browser SSH terminals are limited to 4 concurrent sessions per user', async ({
    page,
  }) => {
    const info = await acmeDev.get('/api/v1/stacks/tenant-acme/acme-vm/ssh-info');
    expect(info.status(), await info.text()).toBe(200);
    await uiLogin(page, USERS.acmeDev);

    const result = await page.evaluate(async () => {
      const scheme = location.protocol === 'https:' ? 'wss' : 'ws';
      const url = `${scheme}://${location.host}/api/v1/stacks/tenant-acme/acme-vm/ssh/ws`;
      const open = (): Promise<WebSocket> =>
        new Promise((resolve, reject) => {
          const ws = new WebSocket(url);
          ws.onopen = () => resolve(ws);
          ws.onerror = () => reject(new Error('WebSocket failed to open'));
        });
      const nextFrame = (ws: WebSocket): Promise<string> =>
        new Promise((resolve) => {
          ws.onmessage = (e) => resolve(String(e.data));
          ws.onclose = () => resolve('closed');
        });

      // Four open terminals (waiting for their `connect` frame) hold the user's slots.
      const held: WebSocket[] = [];
      for (let i = 0; i < 4; i++) held.push(await open());
      const fifth = await open();
      const refused = await nextFrame(fifth);

      // Closing one terminal frees its slot for a new session.
      held.shift()?.close();
      let accepted = '';
      for (let attempt = 0; attempt < 50; attempt++) {
        const ws = await open();
        const frame = nextFrame(ws);
        ws.send(JSON.stringify({ type: 'connect' }));
        accepted = await frame;
        held.push(ws);
        if (!accepted.includes('Too many open SSH sessions')) break;
        await new Promise((r) => setTimeout(r, 100));
      }
      for (const ws of held) ws.close();
      return { refused, accepted };
    });

    const refused = JSON.parse(result.refused);
    expect(refused.type).toBe('error');
    expect(refused.message).toContain('Too many open SSH sessions');
    expect(refused.message).toContain('at most 4');
    // The freed slot admits a real session; it stops at the stack's unresolved target, which
    // only tenant admins may supply.
    expect(result.accepted).not.toContain('Too many open SSH sessions');
    expect(result.accepted).toContain('only tenant administrators may supply a target host');
  });
});
