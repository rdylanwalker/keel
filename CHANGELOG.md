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
