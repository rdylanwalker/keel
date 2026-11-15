import { expect, test } from '@playwright/test';

test.describe('06 - Full Localhost GUI GitOps Workflow', () => {
  test('06.1 - OIDC PKCE login -> GUI auth -> create tenant -> create kind cluster -> install app -> verify status', async ({
    page,
    browserName,
  }) => {
    test.skip(
      browserName !== 'chromium',
      'Full mutating GitOps workflow runs once per server instance',
    );
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
    await page.getByLabel(/Username/i).fill('admin@keel.local');
    await page.getByLabel(/Password/i).fill('e2e-password');
    await page.getByRole('button', { name: /Sign In/i }).click();

    // Header should update and live management cluster should load
    await expect(page.getByText('OIDC Auth')).toBeVisible({ timeout: 10000 });
    await expect(page.getByText(/API error \(401\)/i)).toHaveCount(0);
    await expect(page.getByText('management-cluster-core', { exact: true })).toBeVisible({
      timeout: 10000,
    });

    // 3. Create a new tenant via GUI (+ New Tenant) and merge its PR
    await page.getByRole('button', { name: /Catalog & Form Wizard/i }).click();
    await expect(page.getByText('Self-Service Catalog & Dynamic Form Wizard')).toBeVisible();

    await page.getByRole('button', { name: /\+ New Tenant/i }).click();
    await page.getByLabel(/Tenant ID/i).fill('e2e-team');
    await page.getByRole('button', { name: /Create Tenant PR/i }).click();
    await expect(page.getByText(/Git PR Proposal Successfully Opened!/i)).toBeVisible({
      timeout: 10000,
    });

    // Merge the create-tenant PR in PR & Preview Inspector
    await page.getByRole('button', { name: /PR & Preview Inspector/i }).click();
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

    await page.getByTestId('catalog-list').getByRole('button', { name: 'Create' }).first().click();
    await page.getByRole('button', { name: /Submit & Open Git PR Proposal/i }).click();
    await expect(page.getByText(/Git PR Proposal Successfully Opened!/i)).toBeVisible({
      timeout: 10000,
    });

    // Inspect PKO v2 preview diff & merge the kind-cluster PR
    await page.getByRole('button', { name: /PR & Preview Inspector/i }).click();
    await expect(page.getByRole('heading', { name: /Local kind Cluster/i })).toBeVisible({
      timeout: 10000,
    });
    await expect(page.getByText(/Resource Diff Tree/i)).toBeVisible({ timeout: 10000 });
    await page.getByRole('button', { name: /Merge PR/i }).click();
    await expect(page.getByText(/merged cleanly/i)).toBeVisible({ timeout: 10000 });

    // 5. Install application (`podinfo`) into `e2e-team`
    await page.getByRole('button', { name: /Catalog & Form Wizard/i }).click();
    await page.locator('#tenant-select-catalog').selectOption('e2e-team');
    await page.getByTestId('catalog-list').getByRole('button', { name: 'Create' }).nth(1).click();

    const backendUrlInput = page.getByLabel(/Backend Service URL/i);
    if (await backendUrlInput.isVisible()) {
      await backendUrlInput.fill('http://e2e-api.local');
    }

    await page.getByRole('button', { name: /Submit & Open Git PR Proposal/i }).click();
    await expect(page.getByText(/Git PR Proposal Successfully Opened!/i)).toBeVisible({
      timeout: 10000,
    });

    await page.getByRole('button', { name: /PR & Preview Inspector/i }).click();
    await expect(page.getByRole('heading', { name: /Podinfo App/i })).toBeVisible({
      timeout: 10000,
    });
    await page.getByRole('button', { name: /Merge PR/i }).click();
    await expect(page.getByText(/merged cleanly/i)).toBeVisible({ timeout: 10000 });

    // 6. Verify status & reconciliation in Topology Map
    await page.getByRole('button', { name: /Topology Map/i }).click();
    await page.getByRole('button', { name: /Refresh Map/i }).click();
    await expect(page.getByText('management-cluster-core', { exact: true })).toBeVisible();
    await expect(page.getByText('e2e-team').first()).toBeVisible({ timeout: 10000 });
    await expect(page.getByText(/Live Reconciler Event Stream/i)).toBeVisible();
    await expect(page.getByText(/podinfo/i).first()).toBeVisible({ timeout: 10000 });
  });
});
