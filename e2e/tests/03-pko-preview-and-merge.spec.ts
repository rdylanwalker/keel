import { expect, test } from '@playwright/test';

import { USERS, apiAs, createProposal, git, openTab, uiLogin, uniq } from './support/harness.js';

test.describe('03 - PKO Ephemeral Previews & PR Merge Reconciliation', () => {
  test('03.1 - Proposal preview/merge/reject/reopen endpoints require authentication', async ({
    request,
  }) => {
    expect((await request.get('/api/v1/proposals')).status()).toBe(401);
    expect((await request.get('/api/v1/proposals/1/preview')).status()).toBe(401);
    for (const action of ['merge', 'reject', 'reopen']) {
      expect((await request.post(`/api/v1/proposals/1/${action}`)).status(), action).toBe(401);
    }
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

    const res = await api.get(`/api/v1/proposals/${created.proposal.id}/preview`);
    expect(res.status()).toBe(200);
    const preview = await res.json();
    expect(preview).toMatchObject({
      prId: created.proposal.id,
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
    const merged = await api.post(`/api/v1/proposals/${b.proposal.id}/merge`);
    expect(merged.status(), await merged.text()).toBe(200);
    expect(git(['ls-tree', '-r', '--name-only', 'main'])).toContain(
      `tenants/acme/apps/${bName}/values.yaml`,
    );

    // A was branched before B merged; a two-dot diff would show B's files as deletions.
    const aPreview = await (await api.get(`/api/v1/proposals/${a.proposal.id}/preview`)).json();
    expect(aPreview.gitDiff).toContain(`tenants/acme/apps/${a.proposal.title.split(' ').pop()}`);
    expect(aPreview.gitDiff).not.toContain(bName);

    // A merged PR keeps showing only its own changes.
    const bPreview = await (await api.get(`/api/v1/proposals/${b.proposal.id}/preview`)).json();
    expect(bPreview.state).toBe('merged');
    expect(bPreview.gitDiff).toContain(bName);
    expect(bPreview.gitDiff).not.toContain(a.proposal.title.split(' ').pop());
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
    await page.goto(`/#preview/${id}`);
    await expect(page.getByRole('heading', { name: 'PR & Preview Inspector' })).toBeVisible();
    await expect(page).toHaveURL(new RegExp(`#preview/${id}$`));
    await expect(
      page.getByRole('heading', { name: `PR #${id}: ${created.proposal.title}` }),
    ).toBeVisible();

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
    expect((await (await prs.get(`/api/v1/proposals/${id}/preview`)).json()).state).toBe('open');

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
      .poll(async () => (await (await prs.get(`/api/v1/proposals/${id}/preview`)).json()).state)
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
    await page.goto('/#preview/987654');
    await expect(page.getByTestId('pr-deeplink-missing')).toContainText('PR #987654');
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
    await api.dispose();
    await uiLogin(page, USERS.admin);
    await openTab(page, 'PR & Preview Inspector');
    await page.getByTestId('pr-search-input').fill(name);
    await expect(page.getByTestId('pr-list').getByText(`feat(acme): deploy ${name}`)).toBeVisible();
    await page
      .getByRole('group', { name: 'Filter pull requests by status' })
      .getByRole('button', { name: 'merged' })
      .click();
    await expect(page.getByTestId('pr-list').getByText(`feat(acme): deploy ${name}`)).toHaveCount(
      0,
    );
  });
});
