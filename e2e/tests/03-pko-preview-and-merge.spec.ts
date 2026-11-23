import { expect, test } from '@playwright/test';

import {
  USERS,
  apiAs,
  createProposal,
  git,
  openTab,
  proposalPath,
  uiLogin,
  uniq,
} from './support/harness.js';

test.describe('03 - PKO Ephemeral Previews & PR Merge Reconciliation', () => {
  test('03.1 - Proposal preview/merge/reject/reopen endpoints require authentication', async ({
    request,
  }) => {
    expect((await request.get('/api/v1/proposals')).status()).toBe(401);
    expect((await request.get('/api/v1/tenants/acme/proposals')).status()).toBe(401);
    expect((await request.post('/api/v1/tenants/acme/proposals')).status()).toBe(401);
    for (const scope of ['acme', 'management']) {
      expect((await request.get(`/api/v1/tenants/${scope}/proposals/1/preview`)).status()).toBe(
        401,
      );
      for (const action of ['merge', 'reject', 'reopen']) {
        expect(
          (await request.post(`/api/v1/tenants/${scope}/proposals/1/${action}`)).status(),
          `${scope} ${action}`,
        ).toBe(401);
      }
    }
    // The pre-split unscoped proposal routes are gone (alpha: no compatibility aliases).
    expect([404, 405]).toContain((await request.get('/api/v1/proposals/1/preview')).status());
  });

  test('03.2 - Infra proposal reports an honest synthesized preview when no cluster is connected', async () => {
    const api = await apiAs(USERS.admin);
    const name = uniq('kc');
    const created = await createProposal(api, {
      tenant: 'acme',
      catalogId: 'kind-cluster',
      name,
      values: { nodeCount: 1 },
    });
    expect(created.proposal.title).toMatch(new RegExp(`^feat\\(acme\\): \\w+ ${name}$`));
    // Mocked harness has no live cluster: the server must not pretend a PKO preview ran.
    expect(created.report.source).toBe('synthesized');
    expect(created.report.run?.phase).toBe('notExecuted');
    // Local Git provider (dev mode): the tenant's PRs live in the platform repository, routed
    // through the tenant scope.
    expect(created.proposal.scope).toBe('acme');
    expect(created.proposal.repository.label).toBe('platform');

    const res = await api.get(proposalPath(created.proposal, 'preview'));
    expect(res.status()).toBe(200);
    const preview = await res.json();
    expect(preview).toMatchObject({
      prId: created.proposal.id,
      scope: 'acme',
      state: 'open',
      sourceBranch: created.branch,
      targetBranch: 'main',
    });
    expect(preview.report.source).toBe('synthesized');
    expect(preview.report.run.phase).toBe('notExecuted');
    expect(preview.report.run.message).toMatch(/not executed/i);
    expect(preview.report.summary.create).toBeGreaterThanOrEqual(1);
    expect(preview.gitDiff).toContain(`tenants/acme/infra/${name}/stack.yaml`);
    // The forge comment is labelled as a simulated diff, never as a real pulumi preview.
    expect(preview.comments[0]?.body ?? '').toContain('Simulated diff');
    await api.dispose();
  });

  test('03.3 - PR diffs are three-dot (merge-base) diffs, unaffected by later commits on main', async () => {
    const api = await apiAs(USERS.admin);
    const a = await createProposal(api, {
      tenant: 'acme',
      catalogId: 'podinfo',
      name: uniq('diffa'),
      values: { replicas: 1 },
    });
    const bName = uniq('diffb');
    const b = await createProposal(api, {
      tenant: 'acme',
      catalogId: 'podinfo',
      name: bName,
      values: { replicas: 2 },
    });
    const merged = await api.post(proposalPath(b.proposal, 'merge'));
    expect(merged.status(), await merged.text()).toBe(200);
    expect(git(['ls-tree', '-r', '--name-only', 'main'])).toContain(
      `tenants/acme/apps/${bName}/values.yaml`,
    );

    // A was branched before B merged; a two-dot diff would show B's files as deletions.
    const aPreview = await (await api.get(proposalPath(a.proposal, 'preview'))).json();
    expect(aPreview.gitDiff).toContain(`tenants/acme/apps/${a.proposal.title.split(' ').pop()}`);
    expect(aPreview.gitDiff).not.toContain(bName);

    // A merged PR keeps showing only its own changes.
    const bPreview = await (await api.get(proposalPath(b.proposal, 'preview'))).json();
    expect(bPreview.state).toBe('merged');
    expect(bPreview.gitDiff).toContain(bName);
    expect(bPreview.gitDiff).not.toContain(a.proposal.title.split(' ').pop());
    await api.dispose();
  });

  test('03.3b - A proposal that fell behind main is rebuilt and re-previewed before it merges', async () => {
    const api = await apiAs(USERS.admin);
    const names = [uniq('stalea'), uniq('staleb')];
    const created = [];
    for (const name of names) {
      created.push(
        await createProposal(api, {
          tenant: 'acme',
          catalogId: 'podinfo',
          name,
          values: { replicas: 1 },
        }),
      );
    }
    const [first, second] = created;
    const merged = await api.post(proposalPath(first.proposal, 'merge'));
    expect(merged.status(), await merged.text()).toBe(200);

    // Both proposals rewrote the tenant index from the same `main`: the second is rebuilt on the
    // new `main` and must be reviewed again instead of merging.
    const stale = await api.post(proposalPath(second.proposal, 'merge'));
    expect(stale.status()).toBe(409);
    expect((await stale.json()).error).toMatch(/Keel rebuilt branch/);
    const rebuiltHead = git(['rev-parse', second.branch]).trim();
    expect(git(['rev-parse', `${rebuiltHead}^`]).trim()).toBe(git(['rev-parse', 'main']).trim());

    const preview = await (await api.get(proposalPath(second.proposal, 'preview'))).json();
    expect(preview.gitDiff).toContain(`tenants/acme/apps/${names[1]}`);
    // The first proposal's entries are unchanged context in the re-merged index.
    expect(preview.gitDiff).not.toMatch(new RegExp(`^[-+].*${names[0]}`, 'm'));

    const again = await api.post(proposalPath(second.proposal, 'merge'));
    expect(again.status(), await again.text()).toBe(200);
    const tree = git(['ls-tree', '-r', '--name-only', 'main']);
    for (const name of names) expect(tree).toContain(`tenants/acme/apps/${name}/values.yaml`);
    await api.dispose();
  });

  test('03.4 - GUI inspector: synthesized preview badge, diff tree, confirmed reject, reopen, merge', async ({
    page,
  }) => {
    const api = await apiAs(USERS.admin);
    const name = uniq('kcgui');
    const created = await createProposal(api, {
      tenant: 'acme',
      catalogId: 'kind-cluster',
      name,
      values: { nodeCount: 2 },
    });
    const id = created.proposal.id;
    await api.dispose();

    await uiLogin(page, USERS.admin);
    await page.goto(`/#preview/acme/${id}`);
    await expect(page.getByRole('heading', { name: 'PR & Preview Inspector' })).toBeVisible();
    await expect(page).toHaveURL(new RegExp(`#preview/acme/${id}$`));
    await expect(
      page.getByRole('heading', { name: `PR #${id}: ${created.proposal.title}` }),
    ).toBeVisible();
    // Each PR names its tenant scope and repository (Local Git provider: the platform repository).
    const meta = page.locator('[data-slot="page-header-meta"]').last();
    await expect(meta).toContainText('Tenant: acme');
    await expect(meta).toContainText('Repository: platform repository');

    // Honest preview provenance in mocked mode.
    await expect(page.getByTestId('preview-source-badge')).toHaveAttribute(
      'data-source',
      'synthesized',
    );
    await expect(page.getByTestId('preview-run-status')).toHaveAttribute(
      'data-phase',
      'notExecuted',
    );
    await expect(page.getByTestId('simulated-preview-badge')).toBeVisible();
    await expect(page.getByTestId('git-diff-viewer')).toContainText(`tenants/acme/infra/${name}`);

    // Reject requires an explicit confirmation in an alertdialog; cancelling keeps the PR open.
    await page.getByRole('button', { name: 'Reject PR' }).click();
    const dialog = page.getByRole('alertdialog');
    await expect(dialog).toBeVisible();
    await expect(dialog).toContainText(`Reject PR #${id}?`);
    await dialog.getByRole('button', { name: 'Cancel' }).click();
    await expect(dialog).toHaveCount(0);
    const prs = await apiAs(USERS.admin);
    expect((await (await prs.get(proposalPath(created.proposal, 'preview'))).json()).state).toBe(
      'open',
    );

    // Escape also dismisses the confirmation without acting.
    await page.getByRole('button', { name: 'Reject PR' }).click();
    await expect(page.getByRole('alertdialog')).toBeVisible();
    await page.keyboard.press('Escape');
    await expect(page.getByRole('alertdialog')).toHaveCount(0);

    await page.getByRole('button', { name: 'Reject PR' }).click();
    await page.getByRole('alertdialog').getByRole('button', { name: 'Reject PR' }).click();
    await expect(page.getByText(/rejected|closed/i).first()).toBeVisible();
    await expect(page.getByRole('button', { name: 'Reopen PR' })).toBeVisible();
    await expect
      .poll(
        async () => (await (await prs.get(proposalPath(created.proposal, 'preview'))).json()).state,
      )
      .toBe('closed');

    await page.getByRole('button', { name: 'Reopen PR' }).click();
    await expect(page.getByRole('button', { name: 'Merge PR' })).toBeEnabled();

    await page.getByRole('button', { name: 'Merge PR' }).click();
    await expect(
      page.getByText(new RegExp(`PR #${id}.*merged|merged.*#${id}`, 'i')).first(),
    ).toBeVisible();
    await expect(page.getByRole('button', { name: 'Merged', exact: true }).last()).toBeDisabled();
    await expect
      .poll(() => git(['ls-tree', '-r', '--name-only', 'main']))
      .toContain(`tenants/acme/infra/${name}/stack.yaml`);
    await prs.dispose();
  });

  test('03.5 - Deep link to an unknown PR shows a not-found notice', async ({ page }) => {
    await uiLogin(page, USERS.admin);
    await page.goto('/#preview/acme/987654');
    await expect(page.getByTestId('pr-deeplink-missing')).toContainText('PR #987654 (acme)');
  });

  test('03.6 - PR list status filter and search', async ({ page }) => {
    const api = await apiAs(USERS.admin);
    const name = uniq('search');
    await createProposal(api, {
      tenant: 'acme',
      catalogId: 'podinfo',
      name,
      values: { replicas: 1 },
    });
    // A platform-repository PR (tenant stub, scope `management`) alongside the tenant PR.
    const platformPr = await api.post('/api/v1/tenants', { data: { id: uniq('scope') } });
    expect(platformPr.status(), await platformPr.text()).toBe(201);
    expect((await platformPr.json()).proposal.scope).toBe('management');
    await api.dispose();
    await uiLogin(page, USERS.admin);
    await openTab(page, 'PR & Preview Inspector');
    await page.getByTestId('pr-search-input').fill(name);
    await expect(page.getByTestId('pr-list').getByText(`feat(acme): deploy ${name}`)).toBeVisible();
    // Admins see every repository's PRs; the repository filter narrows them per scope.
    const scopeFilter = page.getByTestId('pr-scope-filter');
    await scopeFilter.selectOption('management');
    await expect(page.getByTestId('pr-list').getByText(`feat(acme): deploy ${name}`)).toHaveCount(
      0,
    );
    await scopeFilter.selectOption('acme');
    await expect(page.getByTestId('pr-list').getByText(`feat(acme): deploy ${name}`)).toBeVisible();
    await scopeFilter.selectOption('all');
    await page
      .getByRole('group', { name: 'Filter pull requests by status' })
      .getByRole('button', { name: 'merged' })
      .click();
    await expect(page.getByTestId('pr-list').getByText(`feat(acme): deploy ${name}`)).toHaveCount(
      0,
    );
  });
});
