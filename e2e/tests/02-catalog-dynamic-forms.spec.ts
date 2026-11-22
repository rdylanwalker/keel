import { expect, test, type Page } from '@playwright/test';

import {
  USERS,
  apiAs,
  branchFiles,
  createProposal,
  git,
  openTab,
  showFile,
  uiLogin,
  uniq,
} from './support/harness.js';

/** Returns true when `needle` occurs anywhere in the tree of `ref` (git grep, fixed string). */
function treeContains(ref: string, needle: string): boolean {
  try {
    git(['grep', '-F', '-q', needle, ref]);
    return true;
  } catch {
    return false;
  }
}

async function openPodinfoDeployForm(page: Page) {
  await openTab(page, 'Catalog & Form Wizard');
  await expect(
    page.getByRole('heading', { name: /Self-Service Catalog & Dynamic Form Wizard/ }),
  ).toBeVisible();
  const card = page.getByTestId('catalog-list').locator('.p-5', { hasText: 'Podinfo App' });
  await card.getByRole('button', { name: 'Create' }).click();
  const modal = page.getByTestId('deploy-instance-modal');
  await expect(modal).toBeVisible();
  return modal;
}

test.describe('02 - Catalog Dynamic Forms & PR Proposal Generation', () => {
  test('02.1 - Unauthenticated catalog and proposal APIs are rejected', async ({ request }) => {
    expect((await request.get('/api/v1/catalog')).status()).toBe(401);
    const res = await request.post('/api/v1/tenants/acme/proposals', {
      data: { catalogId: 'podinfo', tenant: 'acme', values: { replicas: 3 } },
    });
    expect(res.status()).toBe(401);
  });

  test('02.2 - Catalog API exposes nested JSONSchema with writeOnly secret fields', async () => {
    const api = await apiAs(USERS.admin);
    const res = await api.get('/api/v1/catalog');
    expect(res.status()).toBe(200);
    const body = await res.json();
    const apps = body.applications as Array<{
      metadata: { id: string };
      spec: {
        upstream: { repoURL: string; chart: string };
        schema: { properties: Record<string, Record<string, unknown>>; required?: string[] };
      };
    }>;
    const podinfo = apps.find((a) => a.metadata.id === 'podinfo');
    expect(podinfo).toBeDefined();
    const schema = podinfo!.spec.schema;
    expect(schema.required).toContain('replicas');
    expect(schema.properties.adminPassword).toMatchObject({ writeOnly: true, format: 'password' });
    expect(schema.properties.ingress).toMatchObject({ type: 'object' });
    expect(podinfo!.spec.upstream).toMatchObject({ chart: 'podinfo' });
    await api.dispose();
  });

  test('02.3 - Server-side JSON Schema validation rejects invalid values with field details', async () => {
    const api = await apiAs(USERS.admin);
    const res = await api.post('/api/v1/tenants/acme/proposals', {
      data: { tenant: 'acme', catalogId: 'podinfo', name: uniq('bad'), values: { replicas: 0 } },
    });
    expect(res.status()).toBe(422);
    const err = await res.json();
    expect(err.details?.[0]).toMatchObject({ fieldPath: '/replicas' });
    // Friendly error envelope, never the legacy "API error (...)" prefix.
    expect(String(err.error)).not.toMatch(/^API error/);
    await api.dispose();
  });

  test('02.4 - writeOnly secrets are SOPS-encrypted in the PR branch; upstream chart unmodified', async () => {
    const api = await apiAs(USERS.admin);
    const name = uniq('sec');
    const secret = `S3cr3t-${uniq('pw')}`;
    const created = await createProposal(api, {
      tenant: 'acme',
      catalogId: 'podinfo',
      name,
      values: {
        replicas: 3,
        adminPassword: secret,
        ingress: { enabled: true, host: 'podinfo.e2e' },
      },
    });
    expect(created.proposal.title).toBe(`feat(acme): deploy ${name}`);
    expect(created.proposal.sourceBranch).toBe(created.branch);

    const files = branchFiles(created.branch);
    const dir = `tenants/acme/apps/${name}`;
    // Helm secret fields: one SOPS-encrypted values fragment per field in the root
    // `secrets.enc.yaml` Secret, listed in `tenants/kustomization.yaml` (applied by the root
    // `keel-tenants` Kustomization for any tenant) and merged through `HelmRelease.spec.valuesFrom`.
    // No tenant-namespace `<name>-config` Kustomization: `acme` has no tenant GitRepository.
    expect(files).toContain(`${dir}/secrets.enc.yaml`);
    expect(files).not.toContain(`${dir}/config/secrets.enc.yaml`);
    expect(files).not.toContain(`${dir}/config-kustomization.yaml`);
    expect(files).toContain(`${dir}/values.yaml`);
    const tenantsKustomization = showFile(created.branch, 'tenants/kustomization.yaml');
    expect(tenantsKustomization).toContain(`acme/apps/${name}/secrets.enc.yaml`);
    expect(tenantsKustomization).not.toContain(`acme/apps/${name}/config-kustomization.yaml`);

    // Plaintext secret appears nowhere in the branch tree or its commit message.
    expect(treeContains(created.branch, secret)).toBe(false);
    expect(git(['log', '-1', '--format=%B', created.branch])).not.toContain(secret);

    const enc = showFile(created.branch, `${dir}/secrets.enc.yaml`);
    expect(enc).toMatch(/adminPassword: ENC\[AES256_GCM,/);
    expect(enc).toContain('sops:');
    expect(enc).toMatch(/recipient: age1/);

    const values = showFile(created.branch, `${dir}/values.yaml`);
    expect(values).toContain('replicas: 3');
    expect(values).toContain('host: podinfo.e2e');
    expect(values).not.toContain('adminPassword');

    // Upstream chart is consumed as-is: HelmRepository -> upstream URL, chart name + pinned version.
    expect(showFile(created.branch, `${dir}/helm-repository.yaml`)).toContain(
      'https://stefanprodan.github.io/podinfo',
    );
    const hr = showFile(created.branch, `${dir}/helmrelease.yaml`);
    expect(hr).toContain('chart: podinfo');
    expect(hr).toMatch(/version: "?6\.5\.0"?/);
    expect(hr).not.toContain(secret);
    expect(hr).toContain('valuesFrom:');
    expect(hr).toContain(`name: ${name}-secrets`);
    expect(hr).toContain('valuesKey: adminPassword');

    // Nothing was committed to main (PR-only workflow).
    expect(treeContains('main', name)).toBe(false);
    await api.dispose();
  });

  test('02.5 - GUI form: required markers, client-side validation, masked secret preview, PR', async ({
    page,
  }) => {
    await uiLogin(page, USERS.admin);
    const modal = await openPodinfoDeployForm(page);

    // Required fields carry a visual " *" marker; nested object fields are rendered.
    await expect(modal.locator('label', { hasText: 'Replica Count' })).toHaveText(
      /Replica Count\s*\*$/,
    );
    await expect(modal.getByLabel('Ingress Hostname')).toBeVisible();
    await expect(modal.getByLabel('Admin Secret Password')).toHaveAttribute('type', 'password');

    const name = uniq('gui');
    await modal.locator('#instance-name-input').fill(name);
    await modal.locator('#deploy-modal-tenant-select').selectOption('acme');

    // Client-side validation blocks submission (no POST is sent).
    let posted = 0;
    page.on('request', (r) => {
      if (r.method() === 'POST' && /\/api\/v1\/tenants\/[^/]+\/proposals$/.test(r.url())) posted++;
    });
    await modal.getByLabel('Replica Count').fill('0');
    await modal.getByLabel('Ingress Hostname').fill('Bad Host!');
    await modal.getByRole('button', { name: /Submit & Open Git PR Proposal/i }).click();
    await expect(modal.getByText('Please fix the following:')).toBeVisible();
    await expect(modal.locator('.error-detail').first()).toBeVisible();
    expect(posted).toBe(0);

    await modal.getByLabel('Replica Count').fill('4');
    await modal.getByLabel('Ingress Hostname').fill('gui.e2e.local');
    const secret = `GuiS3cret-${uniq('pw')}`;
    await modal.getByLabel('Admin Secret Password').fill(secret);

    // Live values.yaml preview masks writeOnly values and never shows the plaintext.
    const preview = modal.getByLabel('values.yaml preview');
    await expect(preview).toContainText('replicas: 4');
    await expect(preview).toContainText('host: gui.e2e.local');
    await expect(preview).toContainText('********');
    await expect(preview).not.toContainText(secret);

    const createResponse = page.waitForResponse(
      (r) =>
        r.request().method() === 'POST' && /\/api\/v1\/tenants\/[^/]+\/proposals$/.test(r.url()),
    );
    await modal.getByRole('button', { name: /Submit & Open Git PR Proposal/i }).click();
    const res = await createResponse;
    expect(res.status()).toBe(201);
    const body = await res.json();
    await expect(page.getByText(/Git PR Proposal Successfully Opened!/i)).toBeVisible();
    await expect(page.getByText(`feat(acme): deploy ${name}`).first()).toBeVisible();

    expect(treeContains(body.branch, secret)).toBe(false);
    expect(showFile(body.branch, `tenants/acme/apps/${name}/values.yaml`)).toContain('replicas: 4');
  });

  test('02.6 - Deploy dialog closes with Escape without opening a PR', async ({ page }) => {
    await uiLogin(page, USERS.admin);
    const modal = await openPodinfoDeployForm(page);
    await page.keyboard.press('Escape');
    await expect(modal).toHaveCount(0);
    await expect(page.getByText(/Git PR Proposal Successfully Opened!/i)).toHaveCount(0);
  });

  test('02.7 - Cluster-scoped apps (spec.placement.scope: cluster) cannot target the management cluster', async ({
    page,
  }) => {
    const api = await apiAs(USERS.admin);
    const catalog = await (await api.get('/api/v1/catalog')).json();
    const certManager = (
      catalog.applications as Array<{
        metadata: { id: string };
        spec: { placement?: { scope?: string } };
      }>
    ).find((a) => a.metadata.id === 'cert-manager');
    expect(certManager?.spec.placement?.scope).toBe('cluster');

    for (const targetCluster of [undefined, 'local']) {
      const res = await api.post('/api/v1/tenants/acme/proposals', {
        data: {
          tenant: 'acme',
          catalogId: 'cert-manager',
          name: uniq('cm'),
          values: { replicaCount: 1 },
          ...(targetCluster ? { targetCluster } : {}),
        },
      });
      expect(res.status()).toBe(400);
      expect(JSON.stringify(await res.json())).toContain('spec.placement.scope: cluster');
    }
    await api.dispose();

    await uiLogin(page, USERS.admin);
    let posted = 0;
    page.on('request', (r) => {
      if (r.method() === 'POST' && /\/api\/v1\/tenants\/[^/]+\/proposals$/.test(r.url()))
        posted += 1;
    });
    await openTab(page, 'Catalog & Form Wizard');
    const card = page.getByTestId('catalog-list').locator('.p-5', { hasText: 'cert-manager' });
    await card.getByRole('button', { name: 'Create' }).click();
    const modal = page.getByTestId('deploy-instance-modal');
    await expect(modal).toBeVisible();
    await expect(modal.getByTestId('cluster-placement-hint')).toContainText(
      /cluster-scoped resources/i,
    );
    await expect(
      modal.locator('#cluster-select-catalog option', { hasText: /Management Cluster/ }),
    ).toHaveCount(0);
    // Workload cluster stacks from other specs may be preselected: pick a blank custom Secret.
    await modal.locator('#cluster-select-catalog').selectOption('__custom__');
    await modal.getByLabel('Custom kubeconfig Secret name').fill('');
    await modal.getByRole('button', { name: /Submit & Open Git PR Proposal/i }).click();
    await expect(
      modal.getByText(/can only be deployed to a remote workload cluster/i),
    ).toBeVisible();
    expect(posted).toBe(0);
  });

  test('02.8 - Self-service Forgejo and SeaweedFS Helm apps are namespaced with guarded secrets and routes', async () => {
    const api = await apiAs(USERS.admin);
    const catalog = await (await api.get('/api/v1/catalog')).json();
    const apps = catalog.applications as Array<{
      metadata: { id: string; runtime: string };
      spec: { placement?: { scope?: string }; upstream: { version?: string } };
    }>;
    for (const [id, version] of [
      ['forgejo', '17.3.3'],
      ['seaweedfs', '4.54.0'],
    ]) {
      const app = apps.find((a) => a.metadata.id === id);
      expect(app?.metadata.runtime, id).toBe('helm');
      expect(app?.spec.placement?.scope ?? 'namespaced', id).toBe('namespaced');
      expect(app?.spec.upstream.version, id).toBe(version);
    }

    // Required secret fields are validated before anything is encrypted or committed.
    const missingKeys = await api.post('/api/v1/tenants/acme/proposals', {
      data: { tenant: 'acme', catalogId: 'seaweedfs', name: uniq('s3'), values: { s3: {} } },
    });
    expect(missingKeys.status()).toBe(422);

    // The chart's HTTPRoute may never claim a platform host on the shared keel-ingress Gateway.
    const platformHost = await api.post('/api/v1/tenants/acme/proposals', {
      data: {
        tenant: 'acme',
        catalogId: 'forgejo',
        name: uniq('git'),
        values: {
          gitea: {
            admin: {
              username: 'forgejo-admin',
              email: 'ops@acme.example',
              password: 'Forgejo-Admin#Passw0rd!',
            },
          },
          httpRoute: { enabled: true, hostnames: ['forgejo.keel.local'] },
        },
      },
    });
    expect(platformHost.status()).toBe(400);
    expect(JSON.stringify(await platformHost.json())).toContain('reserved platform host');

    // Chart values outside the catalog schema (here a second, unguarded route) are rejected.
    const undeclared = await api.post('/api/v1/tenants/acme/proposals', {
      data: {
        tenant: 'acme',
        catalogId: 'seaweedfs',
        name: uniq('s3'),
        values: {
          s3: {
            credentials: {
              admin: {
                accessKey: '0123456789abcdef0123',
                secretKey: 'c2VjcmV0c2VjcmV0c2VjcmV0c2VjcmV0c2Vj',
              },
            },
          },
          filer: { httpRoute: { enabled: true } },
        },
      },
    });
    expect(undeclared.status()).toBe(400);
    expect(JSON.stringify(await undeclared.json())).toContain('filer.httpRoute');
    await api.dispose();
  });

  test('02.9 - Kyverno, PKO and Flux twins are workload-cluster only, use dedicated namespaces and pin the core versions', async () => {
    const api = await apiAs(USERS.admin);
    const catalog = await (await api.get('/api/v1/catalog')).json();
    const apps = catalog.applications as Array<{
      metadata: { id: string; runtime: string };
      spec: { placement?: { scope?: string; namespace?: string }; upstream: { version?: string } };
    }>;
    for (const [id, version, namespace] of [
      ['kyverno', '3.10.0', 'kyverno'],
      ['pulumi-kubernetes-operator', '2.10.1', 'pulumi-kubernetes-operator'],
      ['flux2', '2.20.0', 'flux-system'],
    ]) {
      const app = apps.find((a) => a.metadata.id === id);
      expect(app?.metadata.runtime, id).toBe('helm');
      expect(app?.spec.placement?.scope, id).toBe('cluster');
      // Dedicated install namespace, never the tenant's shared `tenant-<id>`.
      expect(app?.spec.placement?.namespace, id).toBe(namespace);
      expect(app?.spec.upstream.version, id).toBe(version);

      // Cluster-wide CRDs, RBAC and webhooks never land on the management cluster.
      const local = await api.post('/api/v1/tenants/acme/proposals', {
        data: { tenant: 'acme', catalogId: id, name: uniq(id.slice(0, 8)), values: {} },
      });
      expect(local.status(), id).toBe(400);
    }

    // The form is the whole interface: chart values outside the schema are rejected.
    const undeclared = await api.post('/api/v1/tenants/acme/proposals', {
      data: {
        tenant: 'acme',
        catalogId: 'flux2',
        name: uniq('flux'),
        targetCluster: 'workload-kubeconfig',
        targetNamespace: 'flux-system',
        values: { multitenancy: { enabled: true }, extraObjects: [{ kind: 'ClusterRoleBinding' }] },
      },
    });
    expect(undeclared.status()).toBe(400);
    expect(JSON.stringify(await undeclared.json())).toContain('extraObjects');

    // Any namespace other than the dedicated one is rejected.
    const sharedNs = await api.post('/api/v1/tenants/acme/proposals', {
      data: {
        tenant: 'acme',
        catalogId: 'kyverno',
        name: uniq('kyverno'),
        targetCluster: 'workload-kubeconfig',
        targetNamespace: 'tenant-acme',
        values: {},
      },
    });
    expect(sharedNs.status()).toBe(400);
    expect(JSON.stringify(await sharedNs.json())).toContain('spec.placement.namespace');

    // The Flux multi-tenancy lockdown switch is required (no server-side schema defaults).
    const noLockdown = await api.post('/api/v1/tenants/acme/proposals', {
      data: {
        tenant: 'acme',
        catalogId: 'flux2',
        name: uniq('flux'),
        targetCluster: 'workload-kubeconfig',
        values: {},
      },
    });
    expect(noLockdown.status()).toBe(422);
    await api.dispose();
  });
});
