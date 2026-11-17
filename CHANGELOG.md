# [0.6.0](https://github.com/rdylanwalker/keel/compare/v0.5.1...v0.6.0) (2026-11-17)


### Bug Fixes

* **cli:** avoid 30s timeout on preview status --watch ([8d5a1b8](https://github.com/rdylanwalker/keel/commit/8d5a1b87a6070bffa95860fbcc1f848a8d8751e1))
* **cli:** validate secret key template and harden Forgejo bootstrap ([92f0176](https://github.com/rdylanwalker/keel/commit/92f017610be2f9fe372a5e08206cd1d371e4a845))
* **core:** clear stale kind state on stage 0 re-bootstrap ([f7f6935](https://github.com/rdylanwalker/keel/commit/f7f693599c958f3de4ec9485222619115d93cec4))
* **core:** harden SOPS temp file permissions and CLI path validation ([8fd337e](https://github.com/rdylanwalker/keel/commit/8fd337e7d32cbd6156ad767dfd004c65215dbd54))
* **core:** include field details in schema validation errors ([45c66a0](https://github.com/rdylanwalker/keel/commit/45c66a020595e9c6694894d2cb4622a66899730d))
* **deploy:** parameterize Hetzner management firewall CIDRs ([add4325](https://github.com/rdylanwalker/keel/commit/add4325e4a9e81a9fd2bcebc5fbc4a3e03c34ceb))
* **git:** expose GitRunner log/show helpers and warn on SSH TOFU ([474e035](https://github.com/rdylanwalker/keel/commit/474e035c43ea6f1665332f20e2443ae50ebeab31))
* **kube:** include LimitRange and reconciliation RBAC in tenant bundle ([08e2444](https://github.com/rdylanwalker/keel/commit/08e2444e5e8f96843a9a102dcc8924351d736271))
* **kube:** validate container host before invoking docker port ([124bcb9](https://github.com/rdylanwalker/keel/commit/124bcb9ec815f24e37ac78824f9da85b8ee4eccc))
* **kyverno:** migrate policies to policies.kyverno.io/v1 for v1.20 ([5906f46](https://github.com/rdylanwalker/keel/commit/5906f46baf138de5bc8afc9f4bc15996c2752d46))
* **rauthy:** prevent startup crash and readiness probe restarts ([fe3e5eb](https://github.com/rdylanwalker/keel/commit/fe3e5ebb7113174f1f5f10225100e7043b98d28a))
* **server:** add logout endpoint to clear session cookies ([c252580](https://github.com/rdylanwalker/keel/commit/c25258041debda23f20cd5787b9483c3da1a073a))
* **server:** enforce OIDC JWKS verification, RBAC, CSRF, and SSRF guards ([e2ef2c8](https://github.com/rdylanwalker/keel/commit/e2ef2c80341afc2cd917a10eb7458bed89a7c868))
* **server:** enforce tenant access checks on stacks, releases, and tenants list ([4188096](https://github.com/rdylanwalker/keel/commit/418809644c54eed285d5d6c8a95009b6ffe300db))
* **server:** force OIDC prompt=login on login after logout ([91d627a](https://github.com/rdylanwalker/keel/commit/91d627a215ad85bbb4dda190ff168478a6767d3e))
* **server:** keep tenants/kustomization.yaml synced on tenant lifecycle ([a1b6470](https://github.com/rdylanwalker/keel/commit/a1b6470c895e4d2148295dacfdedc96760db724e))
* **ui:** call logout endpoint to clear session cookie ([b00fbb3](https://github.com/rdylanwalker/keel/commit/b00fbb3d47c49690d7136ad06560b4353908580a))
* **ui:** keep tenant remove button mounted and disabled for default ([f6aa1d1](https://github.com/rdylanwalker/keel/commit/f6aa1d1c63980bee1541cd96d679025035491da5))
* **ui:** make header user and admin role chips distinct ([e557bd0](https://github.com/rdylanwalker/keel/commit/e557bd06f0fb3424ef29315f06240128ea1d0a74))
* **ui:** remove duplicate plus from New Tenant button ([bbc69e1](https://github.com/rdylanwalker/keel/commit/bbc69e1704fab3a9d3bc720649b0a86f4cff369a))
* **ui:** remove duplicate sign-in prompts ([e46329a](https://github.com/rdylanwalker/keel/commit/e46329a7ea86096ffa7d79bca2fb4d836fb2b54a))
* **ui:** store bearer token in memory instead of localStorage ([00ca343](https://github.com/rdylanwalker/keel/commit/00ca34309daae8674c8bcd6fb2f282fc382d7431))


### Features

* **cli:** bootstrap Rauthy, cert-manager, and Forgejo OIDC SSO ([434264b](https://github.com/rdylanwalker/keel/commit/434264ba091412b4ae5f61e0840a4654895158a6))
* **core,server:** add serviceUrl metadata and OIDC CA/issuer support ([5817f5f](https://github.com/rdylanwalker/keel/commit/5817f5f229e7ac5e4aebbdba4d372faff1b38dc0))
* **deploy:** replace Dex with Rauthy, add cert-manager and HTTPS gateway ([2d8d6f6](https://github.com/rdylanwalker/keel/commit/2d8d6f6d7185b735c8b5189f9cb9c384cc983d1c))
* **server:** expose user identity and admin status in auth status ([b8eb8d8](https://github.com/rdylanwalker/keel/commit/b8eb8d83bbaf280fa6f265dc54f16a04159484cc))
* **server:** support tenant-<id>-admin scoped PR triage and merge ([d035df4](https://github.com/rdylanwalker/keel/commit/d035df48e35a76fb39a912f477a72c8e7914e138))
* **ui:** add header user identity pill and tenant context switcher ([a08adf6](https://github.com/rdylanwalker/keel/commit/a08adf6e928c4c7c83b34d05e563229fa820ffb3))
* **ui:** render service link on management plane base addon cards ([fdf2829](https://github.com/rdylanwalker/keel/commit/fdf28290469ddb3e43d55e5cf6949d7a9578b9e1))


### Performance Improvements

* **cli:** parallelize Stage 0 addons and batch Pulumi config ([92eb614](https://github.com/rdylanwalker/keel/commit/92eb6146e084346d3ce3fffbdb861f423729fa95))

## [0.5.1](https://github.com/rdylanwalker/keel/compare/v0.5.0...v0.5.1) (2026-11-16)


### Bug Fixes

* **e2e:** set git author and cleanup partial kind cluster ([abfa1ae](https://github.com/rdylanwalker/keel/commit/abfa1ae39f2f5d87162beb89121cbc28f2f69b4a))

# [0.5.0](https://github.com/rdylanwalker/keel/compare/v0.4.1...v0.5.0) (2026-11-16)


### Bug Fixes

* **ui:** suppress 401 errors in demo mode ([2cc518d](https://github.com/rdylanwalker/keel/commit/2cc518d5eb133fe6161497ff2e9c57d9512d20c8))


### Features

* **e2e:** add screenshot capture script and just recipe ([ce72d5a](https://github.com/rdylanwalker/keel/commit/ce72d5a84ab22541090703bb1c91cdecc0789b19))

## [0.4.1](https://github.com/rdylanwalker/keel/compare/v0.4.0...v0.4.1) (2026-11-16)


### Bug Fixes

* **ci:** upload release assets before publishing ([f297334](https://github.com/rdylanwalker/keel/commit/f2973348c4c92b2eb6e0a7537ea649acb9d4982c))

# [0.4.0](https://github.com/rdylanwalker/keel/compare/v0.3.0...v0.4.0) (2026-11-16)


### Bug Fixes

* **ui:** remove duplicate plus on Add Catalog Item button ([6d3a50e](https://github.com/rdylanwalker/keel/commit/6d3a50e8a8d80a4bd4268d6764ad68b6ef93d530))


### Features

* **ui:** support nested objects and arrays in schema builder ([4d102b3](https://github.com/rdylanwalker/keel/commit/4d102b3503de3a6e7f1bde249d1da788d5415489))

# [0.3.0](https://github.com/rdylanwalker/keel/compare/v0.2.0...v0.3.0) (2026-11-16)


### Bug Fixes

* **bootstrap:** use pulumi:providers:kubernetes and --plaintext config ([1cdb8e5](https://github.com/rdylanwalker/keel/commit/1cdb8e5253e55009854080352a3dd836ace7023e))


### Features

* **bootstrap:** install Stage 0 control-plane addons via Pulumi ([1d55572](https://github.com/rdylanwalker/keel/commit/1d555724af7747ab0994d77306ba00045e0f7bc0))
* **catalog:** add curated schemas for management core-addons ([bf4d234](https://github.com/rdylanwalker/keel/commit/bf4d2341ba33d09577b643e22ee31aa8d6382ce4))
* **cli:** seed core-addons and catalog/core into GitOps repo ([7f28bc2](https://github.com/rdylanwalker/keel/commit/7f28bc27b591dc273cdb46b1fac646b77f3959af))
* **cli:** wire --pulumi-program-dir into bootstrap init ([4647d9e](https://github.com/rdylanwalker/keel/commit/4647d9e7570121c0842004a5ac16d2485ac57057))
* **core:** unify Stage 0 targets under PulumiBootstrapTarget ([fc40d00](https://github.com/rdylanwalker/keel/commit/fc40d00187ac69187706c8d400ebafc0496935c5))
* **deploy:** replace kind-config with Stage 0 Pulumi.yaml ([88a2c0d](https://github.com/rdylanwalker/keel/commit/88a2c0d3e60032b1c7d3fd3da6c9d54876199890))
* **seed:** protect core-addons with auto-rollback and prune=false ([3935245](https://github.com/rdylanwalker/keel/commit/39352457ca8df29822ada20542de2aee2782bad3))
* **server:** add management core-addons upgrade, rollback, and delete guard ([2e18d19](https://github.com/rdylanwalker/keel/commit/2e18d191d238f9605e623842c9d9dc734d799277))
* **ui:** manage protected core-addons with sync-upstream and rollback ([06d28c4](https://github.com/rdylanwalker/keel/commit/06d28c44964541a5e5ff9df3c92fcf80b361f94f))

# [0.2.0](https://github.com/rdylanwalker/keel/compare/v0.1.0...v0.2.0) (2026-11-16)


### Bug Fixes

* **canary:** fix gateway config, remote canary, and metrics ([8da9bc4](https://github.com/rdylanwalker/keel/commit/8da9bc43f7a6b01dc44284498f85795ccd50f959))
* **ci:** resolve e2e, docs, and release workflow failures ([36ad6e4](https://github.com/rdylanwalker/keel/commit/36ad6e402f9aadb36db3d256c12d11ee5bacee0e))
* **ci:** resolve Node 20 deprecation and Fast Refresh warnings ([712a37b](https://github.com/rdylanwalker/keel/commit/712a37be85841486afa70f0998c7546e91da3433))
* **cli:** always provision Forgejo repo when --with-forge is set ([e52668f](https://github.com/rdylanwalker/keel/commit/e52668f054b232233bb4043f0ca7700cb719a096))
* **reconcile:** fix PKO workspace admission, docker proxy, and remote HelmRelease ([4c35998](https://github.com/rdylanwalker/keel/commit/4c359980278026d1b2a236278d9ea601ee53a9b0))
* **server:** allow missing ui/dist during rust-embed compile ([e98cc88](https://github.com/rdylanwalker/keel/commit/e98cc887205be82ad76a14cea47c984a9146b4fd))
* **ui:** use dark theme colors on live reconciler CRD filter select ([d2e07ec](https://github.com/rdylanwalker/keel/commit/d2e07ec0a36b346fc4f7f2762c2510a0a4c74b4f))


### Features

* add Dex OIDC, SeaweedFS S3, schema builder, and consistent docs ([40a078b](https://github.com/rdylanwalker/keel/commit/40a078bd62b56a27ced85fb0bfd3b025f4f8e01a))
* add initial keel platform implementation ([9c85280](https://github.com/rdylanwalker/keel/commit/9c85280d9f9f263d90d3920c931158693c268f81))
* **canary:** wire live Prometheus metrics and workload cluster canary catalog ([668a3cf](https://github.com/rdylanwalker/keel/commit/668a3cfd81df40e08fca857d51b3622bf6c7a2a5))
* **catalog:** support editing catalog items via PR ([c1da7d6](https://github.com/rdylanwalker/keel/commit/c1da7d62b96d442f24233d3ecec8a9eae1f94ce2))
* **git:** add reopen_pull_request and rewrite local forge URLs ([608b6f6](https://github.com/rdylanwalker/keel/commit/608b6f63c119383a2b148de8b0c599014de54680))
* **infra:** wire catalog config, encrypted envRefs, and SOPS decryption ([646da36](https://github.com/rdylanwalker/keel/commit/646da36f03a457f10ba0f9d819bb0ce0966d305d))
* **kube:** add Flagger canary synthesis and live canary status ([4079059](https://github.com/rdylanwalker/keel/commit/407905962b04bc6d80b458da15c12649a7a5269d))
* **server,cli:** wire canary rollouts, PR reopen, Envoy/Flagger addons, and local OIDC port-forwards ([b136c9a](https://github.com/rdylanwalker/keel/commit/b136c9ac74f7f8d3517e7fbe84909a5ed3a5665f))
* **topology,catalog:** fix topology columns, deduplicate events, and edit Pulumi.yaml ([a1a1c40](https://github.com/rdylanwalker/keel/commit/a1a1c4029630b33a30aff094650321de7c853d75))
* **ui,e2e:** add Control Plane & Providers tab, PR reopen, and canary UI ([faf93bb](https://github.com/rdylanwalker/keel/commit/faf93bb5aa000231458e8f0657a089937501606f))
* **ui,pko:** consolidate topology console and wire nested kind provisioning ([e53d219](https://github.com/rdylanwalker/keel/commit/e53d2196bbce0d100ff1b547005f0695dd72b271))
