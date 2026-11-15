import { expect, test } from '@playwright/test';

test.describe('05 - Multi-Tenant Namespace & Secret Isolation Guards', () => {
  test('05.1 - GET /api/v1/stacks rejects unauthenticated requests with 401', async ({
    request,
  }) => {
    const res = await request.get('/api/v1/stacks?tenant=acme');
    expect(res.status()).toBe(401);
    const err = await res.json();
    const msg = err.error || err.message || '';
    expect(msg).toContain('Missing or invalid authentication session');
  });

  test('05.2 - GET /api/v1/releases rejects unauthenticated requests with 401', async ({
    request,
  }) => {
    const res = await request.get('/api/v1/releases?tenant=acme');
    expect(res.status()).toBe(401);
    const err = await res.json();
    const msg = err.error || err.message || '';
    expect(msg).toContain('Missing or invalid authentication session');
  });

  test('05.3 - GET /api/v1/proposals rejects unauthenticated requests with 401', async ({
    request,
  }) => {
    const res = await request.get('/api/v1/proposals?tenant=acme');
    expect(res.status()).toBe(401);
    const err = await res.json();
    const msg = err.error || err.message || '';
    expect(msg).toContain('Missing or invalid authentication session');
  });
});
