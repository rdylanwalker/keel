import { expect, test } from '@playwright/test';

import { BASE_URL, USERS, cliExchange, cliLoopbackCode, uiLogin } from './support/harness.js';

test.describe('01 - OIDC PKCE Auth Flow (GUI + CLI loopback)', () => {
  test('01.1 - Health check and auth status API endpoints return OIDC active state', async ({
    request,
  }) => {
    const healthRes = await request.get('/healthz');
    expect(healthRes.status()).toBe(200);
    expect((await healthRes.text()).trim()).toBe('OK');

    const statusRes = await request.get('/api/v1/auth/status');
    expect(statusRes.status()).toBe(200);
    expect(await statusRes.json()).toMatchObject({
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

  test('01.3 - Protected API rejects unauthenticated and forged sessions', async ({ request }) => {
    for (const p of ['/api/v1/catalog', '/api/v1/proposals', '/api/v1/stacks']) {
      const res = await request.get(p);
      expect(res.status(), p).toBe(401);
    }
    const forged = await request.get('/api/v1/catalog', {
      headers: { Authorization: 'Bearer not-a-real-session-token' },
    });
    expect(forged.status()).toBe(401);
    const html = await (await request.get('/')).text();
    expect(html).toContain('Keel Control Plane');
    expect(html).toContain('id="root"');
  });

  test('01.4 - OIDC login endpoint initiates PKCE S256 + nonce redirect to the IdP', async ({
    request,
  }) => {
    const oidcRes = await request.get('/api/v1/auth/login', { maxRedirects: 0 });
    expect(oidcRes.status()).toBe(302);
    const location = new URL(oidcRes.headers()['location'] || '');
    expect(location.searchParams.get('code_challenge_method')).toBe('S256');
    expect(location.searchParams.get('client_id')).toBe('keel-server');
    expect(location.searchParams.get('nonce')?.length ?? 0).toBeGreaterThan(8);
    expect(location.searchParams.get('state')?.length ?? 0).toBeGreaterThan(8);
    expect(oidcRes.headers()['set-cookie']).toContain('keel_pkce=');
  });

  test('01.5 - GUI login, then logout clears the session and forces re-authentication', async ({
    page,
  }) => {
    await uiLogin(page, USERS.admin);
    await expect(page.getByText(USERS.admin.name)).toBeVisible();

    const logoutButton = page.getByRole('button', { name: 'Logout' });
    const logoutPromise = page.waitForResponse(
      (res) => res.url().includes('/api/v1/auth/logout') && res.request().method() === 'POST',
    );
    await logoutButton.click();
    expect((await logoutPromise).status()).toBe(200);
    await expect(page.getByText('Authentication Required')).toBeVisible();

    await page.reload();
    await expect(page.getByText('Authentication Required')).toBeVisible();
    await expect(logoutButton).toHaveCount(0);

    // Signing in again after logout must prompt for credentials (prompt=login).
    await page
      .getByRole('button', { name: /Sign in with OIDC/i })
      .first()
      .click();
    await expect(page.getByText('Dex Login')).toBeVisible();
  });

  test('01.6 - CLI loopback login (RFC 8252) validates its inputs', async ({ request }) => {
    const challenge = 'A'.repeat(43);
    const cases: Array<[string, Record<string, string>]> = [
      [
        'non-loopback redirect',
        { redirect_uri: 'http://evil.example:8000/callback', state: 'abcdefgh123' },
      ],
      ['loopback without port', { redirect_uri: 'http://127.0.0.1/callback', state: 'abcdefgh' }],
      ['wrong path', { redirect_uri: 'http://127.0.0.1:43123/other', state: 'abcdefgh' }],
      ['short state', { redirect_uri: 'http://127.0.0.1:43123/callback', state: 'x' }],
      [
        'plain PKCE method',
        {
          redirect_uri: 'http://127.0.0.1:43123/callback',
          state: 'abcdefgh123',
          code_challenge_method: 'plain',
        },
      ],
    ];
    for (const [name, params] of cases) {
      const res = await request.get('/api/v1/auth/cli/login', {
        params: { code_challenge: challenge, ...params },
        maxRedirects: 0,
      });
      expect(res.status(), name).toBe(400);
    }
  });

  test('01.7 - CLI loopback login issues a single-use code bound to PKCE + redirect URI', async ({
    request,
  }) => {
    const c = await cliLoopbackCode(USERS.admin);

    // Wrong verifier and mismatched redirect URI are rejected without consuming the code.
    const badVerifier = await cliExchange(request, { ...c, codeVerifier: 'B'.repeat(43) });
    expect(badVerifier.status()).toBe(401);
    const badRedirect = await cliExchange(request, {
      ...c,
      redirectUri: 'http://127.0.0.1:1/callback',
    });
    expect(badRedirect.status()).toBe(401);

    const ok = await cliExchange(request, c);
    expect(ok.status(), await ok.text()).toBe(200);
    const session = await ok.json();
    expect(session).toMatchObject({
      tokenType: 'Bearer',
      sub: USERS.admin.username,
      phase: 'oidc-active',
      isAdmin: true,
    });
    expect(session.expiresAt).toBeGreaterThan(Date.now() / 1000);

    // Replay of the same code is rejected.
    const replay = await cliExchange(request, c);
    expect(replay.status()).toBe(401);

    // The Bearer session authenticates API calls like `keel auth status` does.
    const status = await request.get('/api/v1/auth/status', {
      headers: { Authorization: `Bearer ${session.sessionToken}` },
    });
    expect(await status.json()).toMatchObject({ authenticated: true, isAdmin: true });
    const catalog = await request.get('/api/v1/catalog', {
      headers: { Authorization: `Bearer ${session.sessionToken}` },
    });
    expect(catalog.status()).toBe(200);
  });

  test('01.8 - IdP groups map to roles (tenant member is not an admin)', async ({ request }) => {
    const c = await cliLoopbackCode(USERS.acmeDev);
    const res = await cliExchange(request, c);
    expect(res.status()).toBe(200);
    const session = await res.json();
    expect(session.isAdmin).toBe(false);
    const status = await request.get('/api/v1/auth/status', {
      headers: { Authorization: `Bearer ${session.sessionToken}` },
    });
    const body = await status.json();
    expect(body.authenticated).toBe(true);
    expect(body.isAdmin).toBe(false);
    expect(body.groups).toEqual(['tenant-acme']);
  });

  test('01.9 - CSRF guard rejects cookie-authenticated cross-site mutations', async ({ page }) => {
    await uiLogin(page, USERS.admin);
    // page.request shares the browser cookie jar (keel_session) but sends no Origin by default.
    const noProvenance = await page.request.post('/api/v1/tenants/default/proposals/999999/reject');
    expect(noProvenance.status()).toBe(403);
    const crossOrigin = await page.request.post('/api/v1/tenants/default/proposals/999999/reject', {
      headers: { Origin: 'http://evil.example' },
    });
    expect(crossOrigin.status()).toBe(403);
    const crossSite = await page.request.post('/api/v1/tenants/default/proposals/999999/reject', {
      headers: { 'Sec-Fetch-Site': 'cross-site' },
    });
    expect(crossSite.status()).toBe(403);
    // Same-origin requests pass the guard and reach the handler (unknown PR -> 404).
    const sameOrigin = await page.request.post('/api/v1/tenants/default/proposals/999999/reject', {
      headers: { Origin: new URL(BASE_URL).origin },
    });
    expect(sameOrigin.status()).toBe(404);
  });
});
