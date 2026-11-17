import { expect, test } from '@playwright/test';

test.describe('01 - Stage 0 OIDC PKCE Auth Flow', () => {
  test('01.1 - Health check and auth status API endpoints return OIDC active state', async ({
    request,
  }) => {
    const healthRes = await request.get('/healthz');
    expect(healthRes.status()).toBe(200);
    const healthText = await healthRes.text();
    expect(healthText.trim()).toBe('OK');

    const statusRes = await request.get('/api/v1/auth/status');
    expect(statusRes.status()).toBe(200);
    const statusData = await statusRes.json();
    expect(statusData).toMatchObject({
      authenticated: false,
      phase: 'oidc-active',
      oidcEnabled: true,
    });
  });

  test('01.2 - Legacy bootstrap-login endpoint is removed', async ({ request }) => {
    const res = await request.post('/api/v1/auth/bootstrap-login', {
      data: { token: 'legacy-token' },
    });
    expect([404, 405]).toContain(res.status());
  });

  test('01.3 - Embedded React SPA HTML landing page serves OIDC portal', async ({ request }) => {
    const spaRes = await request.get('/');
    expect(spaRes.status()).toBe(200);
    const html = await spaRes.text();
    expect(html).toContain('Keel Control Plane');
    expect(html).toContain('id="root"');
  });

  test('01.4 - OIDC login endpoint initiates PKCE S256 redirect to Dex', async ({ request }) => {
    const oidcRes = await request.get('/api/v1/auth/login', { maxRedirects: 0 });
    expect(oidcRes.status()).toBe(302);
    const location = oidcRes.headers()['location'] || '';
    expect(location).toContain('code_challenge_method=S256');
    expect(location).toContain('client_id=keel-server');
  });

  test('01.5 - Logout clears session cookie so page reload stays unauthenticated', async ({
    page,
  }) => {
    await page.goto('/');
    await expect(page.getByText('Authentication Required')).toBeVisible();

    await page
      .getByRole('button', { name: /Sign in with OIDC/i })
      .first()
      .click();
    await expect(page.getByText('Dex Login')).toBeVisible();
    await page.getByLabel(/Username/i).fill('admin@keel.local');
    await page.getByLabel(/Password/i).fill('e2e-password');
    await page.getByRole('button', { name: /Sign In/i }).click();

    const logoutButton = page.getByRole('button', { name: 'Logout' });
    await expect(logoutButton).toBeVisible({ timeout: 10000 });

    const logoutPromise = page.waitForResponse(
      (res) => res.url().includes('/api/v1/auth/logout') && res.request().method() === 'POST',
    );
    await logoutButton.click();
    const logoutRes = await logoutPromise;
    expect(logoutRes.status()).toBe(200);

    await expect(page.getByText('Authentication Required')).toBeVisible();

    await page.reload();
    await expect(page.getByText('Authentication Required')).toBeVisible();
    await expect(logoutButton).toHaveCount(0);

    // Clicking Sign in with OIDC after logout must prompt for credentials (prompt=login)
    await page
      .getByRole('button', { name: /Sign in with OIDC/i })
      .first()
      .click();
    await expect(page.getByText('Dex Login')).toBeVisible();
  });
});
