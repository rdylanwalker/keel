/**
 * Shared helpers for the mocked Playwright suite (specs 01-06).
 *
 * The mocked harness (`scripts/start-test-server.mjs`) runs a real `keel serve` against a local
 * bare Git repo and a mock OIDC IdP whose login form selects one of the `MOCK_USERS` profiles.
 */
import { execFileSync } from 'node:child_process';
import crypto from 'node:crypto';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';

import {
  expect,
  request as playwrightRequest,
  type APIRequestContext,
  type Page,
} from '@playwright/test';

export const BASE_URL = process.env.KEEL_BASE_URL || 'http://127.0.0.1:18080';

/** Mock IdP identities (keep in sync with `MOCK_USERS` in `scripts/start-test-server.mjs`). */
export const USERS = {
  admin: { username: 'admin@keel.local', name: 'Keel E2E Admin', groups: ['keel-admins'] },
  acmeDev: { username: 'acme-dev@keel.local', name: 'Acme Developer', groups: ['tenant-acme'] },
  acmeAdmin: {
    username: 'acme-admin@keel.local',
    name: 'Acme Tenant Admin',
    groups: ['tenant-acme-admin'],
  },
  defaultDev: {
    username: 'default-dev@keel.local',
    name: 'Default Developer',
    groups: ['tenant-default'],
  },
} as const;

export type MockUser = (typeof USERS)[keyof typeof USERS];

export interface HarnessState {
  serverUrl: string;
  oidcIssuer: string;
  bareRepo: string;
  workDir: string;
}

/** Reads the harness state file written by `start-test-server.mjs`. */
export function harnessState(): HarnessState {
  const file = path.join(os.tmpdir(), 'keel-e2e-harness.json');
  return JSON.parse(fs.readFileSync(file, 'utf8')) as HarnessState;
}

/** Runs a read-only git command against the harness bare repo. */
export function git(args: string[]): string {
  const { bareRepo } = harnessState();
  return execFileSync('git', ['--git-dir', bareRepo, ...args], { encoding: 'utf8' });
}

/** Lists files changed on `branch` relative to its merge-base with `main`. */
export function branchFiles(branch: string): string[] {
  return git(['diff', '--name-only', `main...${branch}`])
    .split('\n')
    .map((l) => l.trim())
    .filter(Boolean);
}

/** Returns the content of `file` at `ref` in the bare repo. */
export function showFile(ref: string, file: string): string {
  return git(['show', `${ref}:${file}`]);
}

/** Signs in through the GUI (`Sign in with OIDC` -> mock IdP form) and waits for the shell. */
export async function uiLogin(page: Page, user: MockUser = USERS.admin): Promise<void> {
  await page.goto('/');
  await expect(page.getByText('Authentication Required')).toBeVisible();
  await page
    .getByRole('button', { name: /Sign in with OIDC/i })
    .first()
    .click();
  await expect(page.getByText('Dex Login')).toBeVisible();
  await page.getByLabel(/Username/i).fill(user.username);
  await page.getByLabel(/Password/i).fill('e2e-password');
  await page.getByRole('button', { name: /Sign In/i }).click();
  await expect(page.getByRole('button', { name: 'Logout' })).toBeVisible({ timeout: 15000 });
}

/** Navigates using the primary sidebar. */
export async function openTab(
  page: Page,
  label:
    | 'Topology Map'
    | 'Catalog & Form Wizard'
    | 'PR & Preview Inspector'
    | 'Control Plane & Providers',
): Promise<void> {
  await page
    .getByRole('navigation', { name: 'Primary' })
    .getByRole('button', { name: label })
    .click();
}

function b64url(buf: Buffer): string {
  return buf.toString('base64url');
}

export interface CliLoginResult {
  sessionToken: string;
  tokenType: string;
  expiresAt: number;
  sub: string;
  email?: string;
  phase: string;
  isAdmin: boolean;
}

/** Everything a CLI loopback login produces before the code is redeemed (for negative tests). */
export interface CliLoopbackCode {
  code: string;
  state: string;
  codeVerifier: string;
  redirectUri: string;
}

/**
 * Drives the RFC 8252 loopback flow exactly as `keel auth login` does, but over HTTP:
 * `GET /api/v1/auth/cli/login` -> mock IdP login -> server callback -> loopback `?code&state`.
 * The loopback listener is never contacted; the redirect `Location` is parsed instead.
 */
export async function cliLoopbackCode(user: MockUser): Promise<CliLoopbackCode> {
  const ctx = await playwrightRequest.newContext({ baseURL: BASE_URL });
  try {
    const codeVerifier = b64url(crypto.randomBytes(32));
    const codeChallenge = b64url(crypto.createHash('sha256').update(codeVerifier).digest());
    const state = b64url(crypto.randomBytes(12));
    const redirectUri = `http://127.0.0.1:${40000 + Math.floor(Math.random() * 20000)}/callback`;

    const start = await ctx.get('/api/v1/auth/cli/login', {
      params: {
        redirect_uri: redirectUri,
        state,
        code_challenge: codeChallenge,
        code_challenge_method: 'S256',
      },
      maxRedirects: 0,
    });
    expect(start.status(), await start.text()).toBe(302);
    const idpUrl = new URL(start.headers()['location']);
    expect(idpUrl.searchParams.get('code_challenge_method')).toBe('S256');
    // The server runs its own PKCE leg with the IdP; the CLI challenge never reaches the IdP.
    expect(idpUrl.searchParams.get('code_challenge')).not.toBe(codeChallenge);

    const idpLogin = await ctx.post(`${idpUrl.origin}/auth`, {
      form: {
        redirect_uri: idpUrl.searchParams.get('redirect_uri') ?? '',
        state: idpUrl.searchParams.get('state') ?? '',
        nonce: idpUrl.searchParams.get('nonce') ?? '',
        username: user.username,
        password: 'e2e-password',
      },
      maxRedirects: 0,
    });
    expect(idpLogin.status()).toBe(302);
    const callbackUrl = idpLogin.headers()['location'];
    expect(callbackUrl).toContain('/api/v1/auth/callback');

    const callback = await ctx.get(callbackUrl, { maxRedirects: 0 });
    expect(callback.status(), await callback.text()).toBe(302);
    const loopback = new URL(callback.headers()['location']);
    expect(`${loopback.origin}${loopback.pathname}`).toBe(redirectUri);
    expect(loopback.searchParams.get('state')).toBe(state);
    // CLI login must not mint a browser session cookie.
    expect(callback.headers()['set-cookie'] ?? '').not.toMatch(/keel_session=[^;\s]/);
    const code = loopback.searchParams.get('code') ?? '';
    expect(code.length).toBeGreaterThan(20);
    return { code, state, codeVerifier, redirectUri };
  } finally {
    await ctx.dispose();
  }
}

/** Redeems a loopback code at `POST /api/v1/auth/cli/exchange`. */
export async function cliExchange(
  ctx: APIRequestContext,
  c: Pick<CliLoopbackCode, 'code' | 'codeVerifier' | 'redirectUri'>,
) {
  return ctx.post('/api/v1/auth/cli/exchange', {
    data: { code: c.code, codeVerifier: c.codeVerifier, redirectUri: c.redirectUri },
  });
}

/** Full CLI loopback login returning the Bearer session. */
export async function cliLogin(user: MockUser): Promise<CliLoginResult> {
  const c = await cliLoopbackCode(user);
  const ctx = await playwrightRequest.newContext({ baseURL: BASE_URL });
  try {
    const res = await cliExchange(ctx, c);
    expect(res.status(), await res.text()).toBe(200);
    return (await res.json()) as CliLoginResult;
  } finally {
    await ctx.dispose();
  }
}

/** Returns an API client authenticated as `user` with a Bearer token (CSRF-exempt like the CLI). */
export async function apiAs(user: MockUser): Promise<APIRequestContext> {
  const session = await cliLogin(user);
  return playwrightRequest.newContext({
    baseURL: BASE_URL,
    extraHTTPHeaders: { Authorization: `Bearer ${session.sessionToken}` },
  });
}

/** Unique, DNS-safe suffix for resources created by a test. */
export function uniq(prefix: string): string {
  return `${prefix}-${Date.now().toString(36)}${crypto.randomBytes(2).toString('hex')}`;
}

export interface ProposalInfo {
  id: string;
  title: string;
  state: string;
  sourceBranch: string;
  targetBranch: string;
  headSha: string;
  tenant?: string | null;
  url: string;
  mergeCommitSha?: string;
  baseSha?: string;
  /** Route scope: a tenant id, or `management` for the platform repository. */
  scope: string;
  repository: { label: string; fullName?: string };
}

/** `POST`/`GET` collection of one tenant's proposals (`/api/v1/tenants/{tenant}/proposals`). */
export function tenantProposalsPath(tenant: string): string {
  return `/api/v1/tenants/${encodeURIComponent(tenant)}/proposals`;
}

/** Scoped proposal route; PR ids are only unique within their repository. */
export function proposalPath(
  proposal: { scope: string; id: string },
  action?: 'preview' | 'merge' | 'reject' | 'reopen',
): string {
  const base = `${tenantProposalsPath(proposal.scope)}/${encodeURIComponent(proposal.id)}`;
  return action ? `${base}/${action}` : base;
}

/** Creates a deploy proposal (in `data.tenant`'s scope) and returns the parsed response. */
export async function createProposal(
  api: APIRequestContext,
  data: Record<string, unknown> & { tenant: string },
): Promise<{
  proposal: ProposalInfo;
  branch: string;
  report: { source: string; run?: { phase: string; message?: string } };
}> {
  const res = await api.post(tenantProposalsPath(data.tenant), { data });
  expect(res.status(), await res.text()).toBe(201);
  return res.json();
}

/**
 * Reads an SSE stream for up to `ms` milliseconds and returns the `data:` payloads.
 * Uses Node's fetch so the stream can be aborted (Playwright's request API buffers bodies).
 */
export async function collectSse(
  headers: Record<string, string>,
  ms: number,
  onOpen?: () => Promise<void>,
  query?: Record<string, string>,
): Promise<string[]> {
  const ac = new AbortController();
  const qs = query ? `?${new URLSearchParams(query).toString()}` : '';
  const res = await fetch(`${BASE_URL}/api/v1/events/stream${qs}`, { headers, signal: ac.signal });
  expect(res.status).toBe(200);
  const reader = res.body!.getReader();
  const decoder = new TextDecoder();
  let buf = '';
  const timer = setTimeout(() => ac.abort(), ms);
  const pump = (async () => {
    try {
      for (;;) {
        const { value, done } = await reader.read();
        if (done) break;
        buf += decoder.decode(value, { stream: true });
      }
    } catch {
      // aborted
    }
  })();
  if (onOpen) await onOpen();
  await pump;
  clearTimeout(timer);
  return buf
    .split('\n')
    .filter((l) => l.startsWith('data:'))
    .map((l) => l.slice(5).trim());
}
