import { expect, test } from '@playwright/test';

test.describe('02 - Catalog Dynamic Forms & PR Proposal Generation', () => {
  test('02.1 - GET /api/v1/catalog rejects unauthenticated request with 401', async ({
    request,
  }) => {
    const res = await request.get('/api/v1/catalog');
    expect(res.status()).toBe(401);
    const err = await res.json();
    const msg = err.error || err.message || '';
    expect(msg).toContain('Missing or invalid authentication session');
  });

  test('02.2 - POST /api/v1/proposals rejects unauthenticated request with 401', async ({
    request,
  }) => {
    const res = await request.post('/api/v1/proposals', {
      data: {
        catalogId: 'podinfo',
        tenant: 'tenant-acme',
        values: { replicas: 3 },
      },
    });
    expect(res.status()).toBe(401);
    const err = await res.json();
    const msg = err.error || err.message || '';
    expect(msg).toContain('Missing or invalid authentication session');
  });

  test('02.3 - Embedded SPA HTML renders catalog form wizard entrypoint layout', async ({
    request,
  }) => {
    const res = await request.get('/');
    expect(res.status()).toBe(200);
    const html = await res.text();
    expect(html).toContain('Keel Control Plane');
    expect(html).toContain('id="root"');
  });
});
