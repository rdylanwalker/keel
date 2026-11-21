import { expect, test } from '@playwright/test';

import { USERS } from './support/harness.js';

test.describe('06 - Full Localhost GUI GitOps Workflow', () => {
  test('06.1 - OIDC PKCE login -> GUI auth -> create tenant -> create kind cluster -> install app -> verify status', async ({
    page,
    browserName,
  }) => {
    test.skip(
      browserName !== 'chromium',
      'Full mutating GitOps workflow runs once per server instance',
    );
    // The server ships a strict CSP; any violation (inline script, cross-origin load) fails the run.
    const cspViolations: string[] = [];
    page.on('console', (msg) => {
      if (msg.type() === 'error' && /Content Security Policy/i.test(msg.text())) {
        cspViolations.push(msg.text());
      }
    });

    await page.addInitScript(() => {
      const w = window as unknown as { __cspViolations: string[] };
      w.__cspViolations = [];
      document.addEventListener('securitypolicyviolation', (e) => {
        w.__cspViolations.push(`${e.violatedDirective} ${e.blockedURI}`);
      });
    });
    const domCspViolations = () =>
      page.evaluate(
        () => (window as unknown as { __cspViolations?: string[] }).__cspViolations ?? [],
      );

    /** Waits for the PR-opened notice; on failure surfaces CSP violations (e.g. eval-based validators). */
    const expectProposalOpened = async () => {
      try {
        await expect(page.getByText(/Git PR Proposal Successfully Opened!/i)).toBeVisible({
          timeout: 10000,
        });
      } catch (err) {
        const violations = [...(await domCspViolations()), ...cspViolations];
        if (violations.length > 0) {
          throw new Error(`CSP violations blocked the form: ${violations.join('; ')}`, {
            cause: err,
          });
        }
        throw err;
      }
    };

    // 1. Open GUI in unauthenticated mode and verify OIDC sign-in gate hides cluster data
    await page.goto('/');
    await expect(page.getByText('Keel Control Plane')).toBeVisible();
    await expect(page.getByText('Authentication Required')).toBeVisible();
    await expect(page.getByText('management-cluster-core')).toHaveCount(0);

    // 2. Click Sign in with OIDC -> complete username/password login on Dex mock -> return to /
    await page
      .getByRole('button', { name: /Sign in with OIDC/i })
      .first()
      .click();
    await expect(page.getByText('Dex Login')).toBeVisible();
    await page.getByLabel(/Username/i).fill(USERS.admin.username);
    await page.getByLabel(/Password/i).fill('e2e-password');
    await page.getByRole('button', { name: /Sign In/i }).click();

    // Header should update and live management cluster should load
    await expect(page.getByText('Keel E2E Admin')).toBeVisible({ timeout: 10000 });
    await expect(page.getByText(/API error \(401\)/i)).toHaveCount(0);
    await expect(page.getByText('management-cluster-core', { exact: true })).toBeVisible({
      timeout: 10000,
    });

    // 3. Create a new tenant via GUI (New Tenant) and merge its PR
    await page.getByRole('button', { name: /Catalog & Form Wizard/i }).click();
    await expect(page.getByText('Self-Service Catalog & Dynamic Form Wizard')).toBeVisible();

    await page.getByRole('button', { name: /New Tenant/i }).click();
    await page.getByLabel(/Tenant ID/i).fill('e2e-team');
    await page.getByRole('button', { name: /Create Tenant PR/i }).click();
    await expectProposalOpened();

    await page.getByRole('button', { name: /PR & Preview Inspector/i }).click();
    await expect(page.getByTestId('pr-search-input')).toBeVisible();
    await page.getByTestId('pr-search-input').fill('tenant e2e-team');
    await expect(page.getByRole('heading', { name: /create tenant e2e-team/i })).toBeVisible({
      timeout: 10000,
    });
    await page.getByRole('button', { name: /Merge PR/i }).click();
    await expect(page.getByText(/merged cleanly/i)).toBeVisible({ timeout: 10000 });

    // 4. Create a new kind cluster via GitOps for tenant `e2e-team`
    await page.getByRole('button', { name: /Catalog & Form Wizard/i }).click();
    const tenantSelect = page.locator('#tenant-select-catalog');
    await expect(tenantSelect.locator('option[value="e2e-team"]')).toHaveCount(1, {
      timeout: 10000,
    });
    await tenantSelect.selectOption('e2e-team');

    await page
      .getByTestId('catalog-list')
      .locator('.p-5', { hasText: 'Local kind Cluster' })
      .getByRole('button', { name: 'Create' })
      .click();
    await page.getByRole('button', { name: /Submit & Open Git PR Proposal/i }).click();
    await expectProposalOpened();

    // Inspect PKO v2 preview diff & merge the kind-cluster PR
    await page.getByRole('button', { name: /PR & Preview Inspector/i }).click();
    await expect(
      page.getByRole('heading', { name: /feat\(e2e-team\): \w+ kind-cluster/i }),
    ).toBeVisible({
      timeout: 10000,
    });
    await expect(page.getByText(/Resource Diff Tree/i)).toBeVisible({ timeout: 10000 });
    await page.getByRole('button', { name: /Merge PR/i }).click();
    await expect(page.getByText(/merged cleanly/i)).toBeVisible({ timeout: 10000 });

    // 5. Install application (`podinfo`) into `e2e-team`
    await page.getByRole('button', { name: /Catalog & Form Wizard/i }).click();
    await page.locator('#tenant-select-catalog').selectOption('e2e-team');
    await page
      .getByTestId('catalog-list')
      .locator('.p-5', { hasText: 'Podinfo App' })
      .getByRole('button', { name: 'Create' })
      .click();

    const backendUrlInput = page.getByLabel(/Backend Service URL/i);
    if (await backendUrlInput.isVisible()) {
      await backendUrlInput.fill('http://e2e-api.local');
    }

    await page.getByRole('button', { name: /Submit & Open Git PR Proposal/i }).click();
    await expectProposalOpened();

    await page.getByRole('button', { name: /PR & Preview Inspector/i }).click();
    await expect(
      page.getByRole('heading', { name: /feat\(e2e-team\): deploy podinfo/i }),
    ).toBeVisible({
      timeout: 10000,
    });
    await page.getByRole('button', { name: /Merge PR/i }).click();
    await expect(page.getByText(/merged cleanly/i)).toBeVisible({ timeout: 10000 });

    // 6. Verify status & reconciliation in Topology Map
    await page.getByRole('button', { name: /Topology Map/i }).click();
    await page.getByRole('button', { name: /Refresh Map/i }).click();
    await expect(page.getByText('Cluster & Workload Topology')).toBeVisible();
    await expect(page.getByTestId('topology-clusters-section')).toBeVisible();
    await expect(page.getByTestId('topology-vms-section')).toBeVisible();
    await expect(page.getByTestId('topology-services-section')).toBeVisible();
    await expect(page.getByText('Management Control Plane')).toBeVisible();
    await expect(page.getByText('management-cluster-core', { exact: true })).toBeVisible();
    await expect(page.getByText('e2e-team').first()).toBeVisible({ timeout: 10000 });
    await expect(page.getByText(/Live Reconciler Event Stream/i)).toBeVisible();
    await expect(page.getByText(/podinfo/i).first()).toBeVisible({ timeout: 10000 });

    // 6b. Verify SSH button is disabled for kind / non-VM stacks and missing from cluster stacks
    await expect(page.getByTestId('stack-ssh-btn-management-cluster-core')).toHaveCount(0);

    // If an SSH-capable VM stack is present, verify its modal & opt-in browser terminal
    const enabledSshButton = page
      .locator('[data-testid^="stack-ssh-btn-"]:not([disabled])')
      .first();
    if ((await enabledSshButton.count()) > 0) {
      await enabledSshButton.click();
      await expect(page.getByTestId('ssh-raw-command')).toBeVisible();
      await expect(page.getByTestId('ssh-cli-print-command')).toBeVisible();
      await expect(page.getByTestId('ssh-cli-exec-command')).toBeVisible();
      // Interactive terminal must NOT be active by default before user clicks Launch
      await expect(page.getByTestId('browser-ssh-terminal')).toHaveCount(0);
      await page.getByTestId('launch-browser-terminal-btn').click();
      // wterm (DOM-rendered) initializes asynchronously: wait for rendered text, not just the node.
      await expect(page.getByTestId('browser-ssh-terminal')).toBeVisible({ timeout: 10000 });
      await expect(page.getByTestId('browser-ssh-terminal')).not.toBeEmpty({ timeout: 10000 });
      await page.getByTestId('disconnect-browser-terminal-btn').click();
      await page.getByTestId('vm-ssh-modal-close-btn').click();
    }

    // 6c. Author & deploy a Raw Kubernetes Manifest / Template catalog entry, test protection toggle & delete
    await page.getByRole('button', { name: /Catalog & Form Wizard/i }).click();
    await page.getByRole('button', { name: /Add Catalog Item/i }).click();
    await page.locator('#app-id').fill('e2e-raw-manifest');
    await page.locator('#app-name').fill('E2E Raw Manifest App');
    await page.locator('#app-runtime').selectOption('manifest');
    await expect(page.getByTestId('catalog-manifests-yaml-input')).toBeVisible();

    // Toggle protection on
    await page.getByTestId('catalog-protected-checkbox').check();

    await page.getByTestId('extract-manifest-vars-btn').first().click();
    await page.getByRole('button', { name: /Submit App PR Proposal/i }).click();
    await expectProposalOpened();

    // Merge the app creation PR
    await page.getByRole('button', { name: /PR & Preview Inspector/i }).click();
    await expect(page.getByRole('heading', { name: /e2e-raw-manifest/i })).toBeVisible({
      timeout: 10000,
    });
    await page.getByTestId('pr-search-input').fill('e2e-raw-manifest');
    await expect(page.getByTestId('pr-list').getByText(/e2e-raw-manifest/i)).toBeVisible();
    await page.getByRole('button', { name: /Merge PR/i }).click();
    await expect(page.getByText(/merged cleanly/i)).toBeVisible({ timeout: 10000 });

    // Now go back to Catalog, verify it's protected, unprotect it, and delete it!
    await page.getByRole('button', { name: /Catalog & Form Wizard/i }).click();
    await expect(page.getByTestId('catalog-protected-badge-e2e-raw-manifest')).toBeVisible({
      timeout: 10000,
    });
    await expect(page.getByTestId('delete-catalog-item-e2e-raw-manifest')).toBeDisabled();

    // Edit item to unprotect it
    await page
      .getByTestId('catalog-list')
      .locator('.p-5', { hasText: 'E2E Raw Manifest App' })
      .getByRole('button', { name: 'Edit' })
      .click();
    await page.getByTestId('catalog-protected-checkbox').uncheck();
    await page.getByRole('button', { name: /Submit Update PR/i }).click();
    await expectProposalOpened();

    // Merge the unprotect PR
    await page.getByRole('button', { name: /PR & Preview Inspector/i }).click();
    await expect(page.getByRole('heading', { name: /update app e2e-raw-manifest/i })).toBeVisible({
      timeout: 10000,
    });
    await page.getByTestId('pr-search-input').fill('update app e2e-raw-manifest');
    await page
      .getByRole('button', { name: /Merge PR/i })
      .first()
      .click();
    await expect(page.getByText(/merged cleanly/i)).toBeVisible({ timeout: 10000 });

    // Finally delete it
    await page.getByRole('button', { name: /Catalog & Form Wizard/i }).click();
    await expect(page.getByTestId('delete-catalog-item-e2e-raw-manifest')).toBeEnabled({
      timeout: 10000,
    });
    await page.getByTestId('delete-catalog-item-e2e-raw-manifest').click();
    // Deletion is confirmed in a Radix alertdialog before any PR is opened.
    await expect(page.getByRole('alertdialog')).toBeVisible();
    await page.getByTestId('confirm-delete-catalog-item-e2e-raw-manifest').click();
    await expectProposalOpened();

    // 7. Control Plane & Providers keeps cluster health + providers; add-ons live in the topology.
    await page.getByRole('button', { name: /Control Plane & Providers/i }).click();
    await expect(page.getByText('Management Control Plane Cluster')).toBeVisible();
    await expect(page.getByText('Enabled Provider Mesh')).toBeVisible();
    await expect(page.getByText('Management Plane Base Addons')).toHaveCount(0);
    await expect(page.getByTestId('management-addon-row-rauthy')).toHaveCount(0);
    await page.getByRole('button', { name: /Manage add-ons in Topology Map/i }).click();

    // 8. Management add-ons are protected rows of Topology Map -> Deployed Application Releases.
    const releasesSection = page.getByTestId('topology-releases-section');
    await expect(releasesSection.getByTestId('management-addons-group')).toHaveCount(0);
    await expect(releasesSection.getByRole('table')).toHaveCount(1);
    const rauthy = releasesSection.getByTestId('management-addon-row-rauthy');
    await expect(rauthy).toBeVisible();
    // Management rows are listed before tenant releases.
    await expect(releasesSection.locator('tbody tr').first()).toHaveAttribute(
      'data-testid',
      /^management-addon-row-/,
    );
    // Rauthy is an image-based Flux Kustomization add-on: its version is the pinned image tag.
    await expect(rauthy).toContainText('0.37.2');
    await expect(rauthy).toContainText('Kustomization');
    await expect(rauthy).toContainText('flux-system');
    await expect(
      rauthy.getByRole('link', { name: 'https://idp.keel.local:8888/auth/v1/admin' }),
    ).toBeVisible();
    await expect(rauthy.getByText('Protected')).toBeVisible();
    // Protected add-on rows offer no tenant edit/delete actions.
    await expect(
      releasesSection
        .locator('[data-testid^="management-addon-row-"]')
        .getByRole('button', { name: /Delete|Edit/ }),
    ).toHaveCount(0);
    await expect(
      releasesSection.getByRole('button', { name: /Delete release (rauthy|s3-state-backend)/ }),
    ).toHaveCount(0);
    await expect(rauthy.getByTestId('management-addon-diagnostics-rauthy')).toBeEnabled();
    await expect(rauthy.getByTestId('management-addon-reconcile-rauthy')).toBeEnabled();
    await expect(rauthy.getByTestId('management-addon-rollback-rauthy')).toBeEnabled();

    // Filtering the table by "management" keeps only the add-on rows.
    const filter = releasesSection.getByRole('textbox');
    await filter.fill('management');
    await expect(releasesSection.locator('tbody tr').first()).toHaveAttribute(
      'data-testid',
      /^management-addon-row-/,
    );
    await expect(
      releasesSection.locator('tbody tr:not([data-testid^="management-addon-row-"])'),
    ).toHaveCount(0);
    await filter.fill('');

    await releasesSection.getByTestId('management-addon-upgrade-s3-state-backend').click();
    await expect(page.getByRole('button', { name: /Sync Upstream Schema/i })).toBeVisible();
    await page.getByRole('button', { name: /Submit & Open Git PR Proposal/i }).click();
    await expect(
      releasesSection.getByText(/Opened Upgrade PR #\S+ .* for management add-on s3-state-backend/),
    ).toBeVisible({ timeout: 10000 });

    // Roll rauthy back to its previous Git revision (seeded 0.37.1 -> 0.37.2) through a reviewed PR.
    await releasesSection.getByTestId('management-addon-rollback-rauthy').click();
    const rollbackDialog = page.getByRole('alertdialog', { name: /Roll back rauthy/i });
    await expect(rollbackDialog).toBeVisible();
    await rollbackDialog.getByRole('button', { name: 'Open Rollback PR' }).click();
    await expect(
      releasesSection.getByText(/Opened Rollback PR #\S+ .* for management add-on rauthy/),
    ).toBeVisible({ timeout: 10000 });

    expect(cspViolations, cspViolations.join('\n')).toEqual([]);
    expect(await domCspViolations()).toEqual([]);
  });
});
