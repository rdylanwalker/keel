import { expect, test } from '@playwright/test';

test.describe('04 - Flagger Canary Lifecycle & Live SSE Event Stream', () => {
  test('04.1 - GET /api/v1/releases rejects unauthenticated requests with 401', async ({
    request,
  }) => {
    const res = await request.get('/api/v1/releases');
    expect(res.status()).toBe(401);
    const err = await res.json();
    const msg = err.error || err.message || '';
    expect(msg).toContain('Missing or invalid authentication session');
  });

  test('04.2 - GET /api/v1/events/stream rejects unauthenticated requests with 401', async ({
    request,
  }) => {
    const res = await request.get('/api/v1/events/stream');
    expect(res.status()).toBe(401);
    const err = await res.json();
    const msg = err.error || err.message || '';
    expect(msg).toContain('Missing or invalid authentication session');
  });

  test('04.3 - GET /metrics endpoint returns Prometheus telemetry metrics without auth', async ({
    request,
  }) => {
    const res = await request.get('/metrics');
    expect(res.status()).toBe(200);
    const text = await res.text();
    expect(text).toContain('# HELP');
  });
});
