import { expect, test } from '@playwright/test';

test.describe('03 - PKO Ephemeral Previews & PR Merge Reconciliation', () => {
  test('03.1 - GET /api/v1/proposals rejects unauthenticated requests with 401', async ({
    request,
  }) => {
    const res = await request.get('/api/v1/proposals');
    expect(res.status()).toBe(401);
    const err = await res.json();
    const msg = err.error || err.message || '';
    expect(msg).toContain('Missing or invalid authentication session');
  });

  test('03.2 - GET /api/v1/proposals/:pr_id/preview rejects unauthenticated requests with 401', async ({
    request,
  }) => {
    const res = await request.get('/api/v1/proposals/1/preview');
    expect(res.status()).toBe(401);
    const err = await res.json();
    const msg = err.error || err.message || '';
    expect(msg).toContain('Missing or invalid authentication session');
  });

  test('03.3 - POST /api/v1/proposals/:pr_id/merge rejects unauthenticated requests with 401', async ({
    request,
  }) => {
    const res = await request.post('/api/v1/proposals/1/merge');
    expect(res.status()).toBe(401);
    const err = await res.json();
    const msg = err.error || err.message || '';
    expect(msg).toContain('Missing or invalid authentication session');
  });
});
