import { expect, test } from '@playwright/test';

test.describe('01 - Stage 0 Bootstrap & OIDC PKCE Auth Flow', () => {
  test('01.1 - Health check and auth status API endpoints return Stage 0 bootstrap state', async ({
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
      phase: 'bootstrap',
    });
  });

  test('01.2 - Bootstrap login rejects empty and invalid tokens at API layer', async ({
    request,
  }) => {
    const emptyRes = await request.post('/api/v1/auth/bootstrap-login', {
      data: { token: '' },
    });
    expect(emptyRes.status()).toBe(401);
    const emptyErr = await emptyRes.json();
    const msg = emptyErr.error || emptyErr.message || '';
    expect(msg).toContain('must not be empty');

    const invalidRes = await request.post('/api/v1/auth/bootstrap-login', {
      data: { token: 'invalid-256-bit-token-00000000000000000000000000000000000000000000' },
    });
    expect(invalidRes.status()).toBe(401);
    const invalidErr = await invalidRes.json();
    const invalidMsg = invalidErr.error || invalidErr.message || '';
    expect(invalidMsg).toContain('Invalid or already consumed bootstrap token');
  });

  test('01.3 - Embedded React SPA HTML landing page serves Stage 0 bootstrap portal', async ({
    request,
  }) => {
    const spaRes = await request.get('/');
    expect(spaRes.status()).toBe(200);
    const html = await spaRes.text();
    expect(html).toContain('Keel Control Plane');
    expect(html).toContain('id="root"');
  });

  test('01.4 - OIDC login endpoint handles Stage 0 unconfigured vs active state', async ({
    request,
  }) => {
    const oidcRes = await request.get('/api/v1/auth/login', { maxRedirects: 0 });
    expect([302, 503]).toContain(oidcRes.status());
    if (oidcRes.status() === 503) {
      const err = await oidcRes.json();
      const msg = err.error || err.message || '';
      expect(msg).toContain('OIDC is not configured');
    }
  });
});
