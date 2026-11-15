import { expect, test } from '@playwright/test';

test.describe('Keel Platform Smoke Test', () => {
  test('verify smoke environment status or target base URL', async ({ request, baseURL }) => {
    const health = await request.get('/healthz');
    expect(health.status()).toBe(200);
    expect(baseURL).toBeDefined();
  });

  test('validate basic landing page accessibility when server active', async ({ request }) => {
    const res = await request.get('/');
    expect(res.status()).toBe(200);
    const html = await res.text();
    expect(html).toContain('Keel Control Plane');
  });
});
