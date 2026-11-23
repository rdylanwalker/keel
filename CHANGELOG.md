## [0.12.1](https://github.com/rdylanwalker/keel/compare/v0.12.0...v0.12.1) (2026-11-23)

### Bug Fixes

* address fresh-user quick start snags ([4d6f956](https://github.com/rdylanwalker/keel/commit/4d6f956858d6a0d7af03b387ad662311dc98ff32))
* **kube:** let tenant identities manage Flagger canaries in their namespace ([c308c5a](https://github.com/rdylanwalker/keel/commit/c308c5a90284e0d88f0018fb708e2c0a8b5fead0))
* **server:** apply catalog schema defaults to API installs and surface release failure reasons ([e28f4cb](https://github.com/rdylanwalker/keel/commit/e28f4cbf61ae09125d3ca61cb8950f5844e7939b)), closes [#4](https://github.com/rdylanwalker/keel/issues/4)
* **server:** bring stale proposal branches up to date instead of failing merges ([12f5c1b](https://github.com/rdylanwalker/keel/commit/12f5c1b54d6239469cbf95d1753a034e307b4cde)), closes [#9](https://github.com/rdylanwalker/keel/issues/9)
* **server:** keep bundle kubeconfig wiring over item schema defaults ([356392f](https://github.com/rdylanwalker/keel/commit/356392fb25223b48011012c68e29e843a16f84c2))

## [0.12.0](https://github.com/rdylanwalker/keel/compare/v0.11.0...v0.12.0) (2026-11-22)

### Features

* **cli:** seed every built-in catalog entry and remove orphan seed templates ([c0782ab](https://github.com/rdylanwalker/keel/commit/c0782abe9a3cfb1314e98e6da238807a04e66a2c))

### Bug Fixes

* **security:** refuse credentials over non-local http and resolve CodeQL alerts ([06ce1fa](https://github.com/rdylanwalker/keel/commit/06ce1fa6b94c2217c5f29f45dccb7cc72f09151e)), closes [48-#52](https://github.com/rdylanwalker/48-/issues/52) [#56](https://github.com/rdylanwalker/keel/issues/56) [#57](https://github.com/rdylanwalker/keel/issues/57) [#53](https://github.com/rdylanwalker/keel/issues/53) [#54](https://github.com/rdylanwalker/keel/issues/54) [#55](https://github.com/rdylanwalker/keel/issues/55) [#47](https://github.com/rdylanwalker/keel/issues/47)

## [0.11.0](https://github.com/rdylanwalker/keel/compare/v0.10.0...v0.11.0) (2026-11-22)

### ⚠ BREAKING CHANGES

* existing installations are not upgraded; re-run
`keel bootstrap init` on a fresh cluster with new GitOps and tenant
repositories. Removed migration-only API routes and CLI commands.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
* **server:** the flat proposal routes /api/v1/proposals/{id}[...]
and POST /api/v1/proposals are removed. Use
/api/v1/tenants/{tenant}/proposals[/{id}/preview|merge|reject|reopen]
(`management` for the platform repository). PR ids are per repository.
GET /api/v1/proposals remains as an aggregated read-only listing.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
* **server:** tenant-default and every new tenant live in their own
repository; KEEL_TENANT_GIT_TOKEN and the shared tenant git-credentials Secret
are gone, an external --git-url requires --git-token, keel bootstrap init exits
non-zero when tenant-default cannot be provisioned, and tenant administrators
can no longer merge platform-repository PRs under tenants/<id>/ for tenants
with their own repository. Start from a fresh bootstrap.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>

### Features

* **catalog:** substitute the http-echo apiToken into a workload Secret ([75c71df](https://github.com/rdylanwalker/keel/commit/75c71df16eb51d049772da668c04fd67921f6854))
* **cli:** provision a Forgejo bot user and keel-tenants organisation for tenant repositories ([eaa3833](https://github.com/rdylanwalker/keel/commit/eaa38338b8c48a82c3db0cad3aadd90c09814172))
* **git:** add per-repository ForgeProvider operations for tenant repositories ([48899cf](https://github.com/rdylanwalker/keel/commit/48899cfa19e2b0cf23336715007e6f9a60fac589))
* **kube:** synthesize per-tenant repository stubs and credentials ([2802627](https://github.com/rdylanwalker/keel/commit/28026276d770f50f9c86c360dfe0d7cfdbc1bc15))
* **server:** provision a dedicated repository and read credential for new tenants ([4e90f28](https://github.com/rdylanwalker/keel/commit/4e90f286b5fa40cacb358a08804c080ae58e7435))
* **server:** route tenant proposals through per-tenant repositories ([c4ae9d7](https://github.com/rdylanwalker/keel/commit/c4ae9d70f761a2458489a742f26ba623a7186e17))
* **ui:** show per-tenant repositories and tenant-scoped proposals ([c7840e6](https://github.com/rdylanwalker/keel/commit/c7840e6370affac37ff74571ec20508474de20d8))

### Bug Fixes

* **deploy:** deny tenant writes to PKO workspaces, updates, stack status and protect labels ([ba5d5d8](https://github.com/rdylanwalker/keel/commit/ba5d5d8c122995268e4e2519ed991fc5b11de27c))
* **git:** align Forgejo tenant-repo provisioning with the Forgejo 15 API ([47ecca2](https://github.com/rdylanwalker/keel/commit/47ecca295d591774f66b18de80579fc573276e36))
* **server:** enforce tenant scoping on every tenant read endpoint ([1b7622c](https://github.com/rdylanwalker/keel/commit/1b7622cf95974c2a9fb3a1a7e67446f4207ebd02))

### Code Refactoring

* drop upgrade and migration paths for the alpha ([9a054da](https://github.com/rdylanwalker/keel/commit/9a054da94ddf1eede3d533638efe5d3a7164a32b))

## [0.10.0](https://github.com/rdylanwalker/keel/compare/v0.9.0...v0.10.0) (2026-11-22)

### Features

* **pko:** clone tenant and preview Stack programs with gitSource ([485dd6d](https://github.com/rdylanwalker/keel/commit/485dd6d7ade33ef4a5e2ef92fd1d45f4c6d7a8ae))

### Bug Fixes

* **deploy:** allow slow first image pulls in Stage 0 and live E2E waits ([2590dae](https://github.com/rdylanwalker/keel/commit/2590daef496b7a9080d8954c7b11a85d60a16b81))
* **deploy:** close source-controller artifacts to tenants and add tenant source policies ([4a80063](https://github.com/rdylanwalker/keel/commit/4a80063dad8ed7c33418589ff6537a8312050acc))
* **kube:** fail closed when a tenant preview has no tenant Git source ([0c55a55](https://github.com/rdylanwalker/keel/commit/0c55a55a8e611768a80ac7afc73f0665675bc96b))
* **kube:** let tenant PKO workspaces reach the local-dev Docker proxy ([d958759](https://github.com/rdylanwalker/keel/commit/d95875918b93a5be5baeed07bb0a7886e110a1c3))
* **pko:** fetch tenant Stack sources over HTTPS with the keel CA trusted ([62aff06](https://github.com/rdylanwalker/keel/commit/62aff06b08abdd45c1cc31c9a5b95455653d4821))
* **server:** create preview Git credentials before the preview Stack ([b5cec7f](https://github.com/rdylanwalker/keel/commit/b5cec7f43f8ee5811e0bb49b86af41102634fb92))

## [0.9.0](https://github.com/rdylanwalker/keel/compare/v0.8.0...v0.9.0) (2026-11-22)

### Features

* **catalog:** add self-service Forgejo and SeaweedFS catalog apps ([d63c753](https://github.com/rdylanwalker/keel/commit/d63c753038e5968a33572c81a1d52c27b9e0341c))
* **catalog:** add self-service keel-server catalog app ([142d21e](https://github.com/rdylanwalker/keel/commit/142d21eda820fd3872b86b028f2c0600d7f35b40))
* **catalog:** add self-service Kyverno, Pulumi Kubernetes Operator and Flux catalog apps ([20cee98](https://github.com/rdylanwalker/keel/commit/20cee980df012321b9a98d026b9fd0b23251feca))
* **catalog:** add self-service Rauthy OIDC provider catalog app ([16de4eb](https://github.com/rdylanwalker/keel/commit/16de4eb93cb473b136fc4381febd0ca708417791))
* **catalog:** declare workload-cluster placement for cluster-scoped apps ([c0b8efb](https://github.com/rdylanwalker/keel/commit/c0b8efb0f27bdecc7f5148115d9b12d5fd635150))
* **cli:** add opt-in --with-keel-server Flux add-on with catalog entry ([c95237d](https://github.com/rdylanwalker/keel/commit/c95237dd8d1d1089729fe35a7fa228959a3d4dff))
* **deploy:** let Flux upgrade and roll back itself via a protected flux2 HelmRelease ([2b2a5c5](https://github.com/rdylanwalker/keel/commit/2b2a5c5d734d5cf418048b8c202e9f48b51f2c2a))
* **deploy:** manage optional Forgejo as a Flux add-on with a catalog entry ([ff6f907](https://github.com/rdylanwalker/keel/commit/ff6f90766b260b0042b2cb28e6d7610e93422274))
* **server:** list management add-ons as protected releases for cluster admins ([341d3bc](https://github.com/rdylanwalker/keel/commit/341d3bc5a8103fefa7d23f7aef398ed70bb58eef))
* **ui:** render management add-ons as rows in the topology releases table ([fcaef14](https://github.com/rdylanwalker/keel/commit/fcaef14ce43af5af2101745d6dcc444cf8d56dc0))
* **ui:** show management add-ons in the topology applications section ([9ece8d8](https://github.com/rdylanwalker/keel/commit/9ece8d88e5de7415b0a31063469ea11af84a3fee))
* **ui:** use Edit for management add-on rows to match tenant release actions ([4553027](https://github.com/rdylanwalker/keel/commit/4553027c48ef751a8e4cd02a0182f4760f7773b5))

### Bug Fixes

* **catalog:** align self-service envoy-gateway, flagger and prometheus with management versions ([abcfc7e](https://github.com/rdylanwalker/keel/commit/abcfc7e04c4a6e52f18b9ec38f5ec703d9296da0))
* **cli:** reconcile management add-ons with Flux and manage Rauthy as a Flux add-on ([c22c29c](https://github.com/rdylanwalker/keel/commit/c22c29c2003b31ec2ef3a3ec41c4c5af8e57d02a))
* **deploy:** disable bundled k3s Traefik in favour of Envoy Gateway ([bddb4d8](https://github.com/rdylanwalker/keel/commit/bddb4d88da911ce573dae460ded9b4d7b003ea21))
* **security:** resolve CodeQL command-injection, path-injection and cleartext-logging alerts ([206c5a0](https://github.com/rdylanwalker/keel/commit/206c5a0ec5b76fa63564213769e34036a64a9944))
* **server:** apply manifest app secrets before substitution and fix remote canary placement ([64702c1](https://github.com/rdylanwalker/keel/commit/64702c102ca9ca8407537db20d3714e21679808c))
* **server:** create the target namespace in remote canary Kustomizations ([67904fe](https://github.com/rdylanwalker/keel/commit/67904fe5a55e5c89ec2a7fa12ddb8ea7fe390747))
* **server:** keep Helm app secrets in the root tenant kustomization ([f30f0ab](https://github.com/rdylanwalker/keel/commit/f30f0ab94213c61fcd4fcd4aecb776939ff24389))
* **server:** keep SOPS secret metadata readable so Flux can decrypt under kustomize transforms ([79cf735](https://github.com/rdylanwalker/keel/commit/79cf735bfe196e4c64f43f8cc567fa4b009d8bce))
* **server:** make core add-on upgrades multi-document safe and kind-aware ([8435e49](https://github.com/rdylanwalker/keel/commit/8435e49c96534d370335960a2775168748338ec9))

# [0.8.0](https://github.com/rdylanwalker/keel/compare/v0.7.0...v0.8.0) (2026-11-21)


### Bug Fixes

* **cli:** enforce scoped state credentials only with a live cluster ([aff8e2e](https://github.com/rdylanwalker/keel/commit/aff8e2e28d964cb6031ea47e6bc7ae4f75396f64))
* **deploy:** lock down flux tenancy, pin core charts and bind tenant flux identity ([01f6b6a](https://github.com/rdylanwalker/keel/commit/01f6b6a210aee4682db7e90edaf24ad68f463be2))
* **server:** report missing age-keygen as a server misconfiguration ([ebe6911](https://github.com/rdylanwalker/keel/commit/ebe69112d20f8c9953dcee5ca949898e174635e9))


### Features

* **cli:** add proposal, catalog and auth commands with durable stage 0 state escrow ([68bcd41](https://github.com/rdylanwalker/keel/commit/68bcd41b353dd5e3802f42726c87928c59c9b0ac))
* **core:** add catalog protection, tenant scope, and cloud k8s blueprints ([9b94bfb](https://github.com/rdylanwalker/keel/commit/9b94bfb50fd11e66418e5dacf77ae98a3cdb2774))
* **core:** escape manifest template values, harden ssh targets and zeroize key material ([762c053](https://github.com/rdylanwalker/keel/commit/762c053934dfdc087e2c2e285ecb327e3c3eaa98))
* **git:** pin merges to reviewed sha, use merge-base diffs and paginate forge listings ([8000af4](https://github.com/rdylanwalker/keel/commit/8000af445992ece6e43e4b32c8b82c784ac626e3))
* **kube:** add tenant s3 identities, preview runner and null-safe cel policies ([76bfb02](https://github.com/rdylanwalker/keel/commit/76bfb027ba3de4bfc0843728bf1c46afc84c0009))
* **pko:** run real pko previews with tenant-scoped state and secret passphrases ([872fed7](https://github.com/rdylanwalker/keel/commit/872fed73fa6e1524682cf7efa114c388cbb8fb99))
* **server:** add catalog deletion PRs, tenant RBAC, and stack resource kinds ([87b7dfe](https://github.com/rdylanwalker/keel/commit/87b7dfe8428ee8764a8d955fbc220065a101f58c))
* **server:** add cli loopback login, csp headers, preview lifecycle and auth hardening ([2e0a8d0](https://github.com/rdylanwalker/keel/commit/2e0a8d0e363e8f44972980dcdf69aa1b11c34ef4))
* **ui:** add catalog protection controls, PR search, and 3-section topology ([2c8ebc4](https://github.com/rdylanwalker/keel/commit/2c8ebc4b83b95ba8f0559d90d51b73f6d25bf7ba))
* **ui:** add role-aware shadcn ui, wterm ssh terminal and responsive layouts ([247f865](https://github.com/rdylanwalker/keel/commit/247f865922c2ce7b9bb4e5143aefd9fabc2d0bf8))

# [0.7.0](https://github.com/rdylanwalker/keel/compare/v0.6.12...v0.7.0) (2026-11-19)

### Bug Fixes

* **catalog,e2e:** lowercase gcp firewall name, sync blueprints, and retry 409 merge ([d9ce98c](https://github.com/rdylanwalker/keel/commit/d9ce98cc4bf9ca10df952ee5d752fc311f00c095))
* **catalog,e2e:** remove ternary in aws Pulumi.yaml and use unique VM names per run ([7ad84de](https://github.com/rdylanwalker/keel/commit/7ad84de3f5505272a0083eea9b071adcab3f549a))
* **cli:** fix Forgejo OIDC secret length and PKCE ([6b35e4d](https://github.com/rdylanwalker/keel/commit/6b35e4d8348f158b6b6e9f9e0d83d73a745afbd7))
* **cli:** seed free-tier cloud blueprints and fix Forgejo Rauthy OIDC ([abe5801](https://github.com/rdylanwalker/keel/commit/abe58019fbee8d01b67e390f6ca7930df46e5b75))
* **crypto,server:** include cluster sops-age recipient when encrypting tenant secrets ([435145a](https://github.com/rdylanwalker/keel/commit/435145ae44f55430d350abc6b42477ced5861e92))
* **e2e:** generate valid ephemeral ed25519 ssh public key for cloud VMs ([d29d28f](https://github.com/rdylanwalker/keel/commit/d29d28f8d7b35affdbbf9521c94d86852d65cf82))
* **e2e:** merge cloud PRs before parallel reconciliation and seed github token ([e87f86e](https://github.com/rdylanwalker/keel/commit/e87f86e2b34f1730016246a3ebc58ba883f960f4))
* **e2e:** preserve primary stack error on cleanup and extend cloud timeout to 600s ([ef6ad89](https://github.com/rdylanwalker/keel/commit/ef6ad899f612a8f406e7b73ad40ab2fc174f46ce))
* **e2e:** push synced blueprints after keel serve starts forgejo port-forward ([14f2072](https://github.com/rdylanwalker/keel/commit/14f207209386a4ebdb6f313236a58fb8bb95ea03))
* **kube,ui:** surface terminating state and force-delete warnings ([bab4e6b](https://github.com/rdylanwalker/keel/commit/bab4e6b28f9a0179d97ffd43eebc7e99a39fb4c4))
* **pko,server:** map cloud secret fields to secretsRef and provider envRefs ([ed53682](https://github.com/rdylanwalker/keel/commit/ed53682061a6198a56f9bf099a3d150c8beca755))
* **pko:** raise workspace memory and use standard azure ip ([25772c3](https://github.com/rdylanwalker/keel/commit/25772c3f212a9ab076bf532c79a59bbe5effc99d))
* **server,e2e:** set provider region/project/location config and fail fast on stalled stacks ([0e252dc](https://github.com/rdylanwalker/keel/commit/0e252dc39e61fd742e45c06544c60e1c2cd5f557))
* **server,e2e:** sync Kyverno restrict-pulumi-providers on provider PR merge ([919a466](https://github.com/rdylanwalker/keel/commit/919a4661d96995557280c8428b8f7d23cb7e151e))
* **ui:** align catalog and deploy forms to runtime ([f175806](https://github.com/rdylanwalker/keel/commit/f17580626321285c9229fd9f0e4a650a5e01fcb5))
* **ui:** clear pending refresh timers on unmount ([a2a9234](https://github.com/rdylanwalker/keel/commit/a2a9234b7c43c1d96dc3d7e8c8d8b69466235b63))
* **ui:** make catalog edit modal scrollable ([8727443](https://github.com/rdylanwalker/keel/commit/8727443216cae1b83d7ff6e39b542c5d24317f40))

### Features

* **bootstrap,e2e:** wire cloud stage0 k3s kubeconfig and run stage1 cloud e2e via kind ([4e86214](https://github.com/rdylanwalker/keel/commit/4e8621415e4604da237c637929e6d42237eb380a))
* **catalog:** generalize cloud VM blueprints with free-tier defaults ([7520568](https://github.com/rdylanwalker/keel/commit/752056820ce40ab4c2d4bca6793a758c12d1b1f7))
* **catalog:** support raw k8s manifests and ssh access in backend and cli ([fa6c505](https://github.com/rdylanwalker/keel/commit/fa6c5051a2738bfd9b10b38318eb0707fd0d1442))
* **ssh:** bridge browser terminal via russh and disable non-VM SSH buttons ([eea4a9d](https://github.com/rdylanwalker/keel/commit/eea4a9d20e398d073da511bc958511823d6e00fb))
* **stacks:** add force-delete support for stuck PKO stacks ([d5736bc](https://github.com/rdylanwalker/keel/commit/d5736bcd3d55de0b638c5fbf882a472bb4f887d2))
* **stacks:** add native stack unlock to CLI, API, and UI ([2e1b2ec](https://github.com/rdylanwalker/keel/commit/2e1b2ec2cea3ae26a422707a7e1612e3acc6a562))
* **stacks:** add stack reconcile action and move restart to inspector ([d28a191](https://github.com/rdylanwalker/keel/commit/d28a191ba1c2251beb2492ff61837f82c76283e6))
* **ui:** add raw manifest wizard and vm ssh browser terminal modal ([b2595d8](https://github.com/rdylanwalker/keel/commit/b2595d818cb1f601eaaa0cbb2425ec5e5c5337af))
* **ui:** allow editing host in ssh modal and pass to bridge ([796841a](https://github.com/rdylanwalker/keel/commit/796841ad26f2ce85f31e74665036b8dbe3ea00e1))

## [0.6.12](https://github.com/rdylanwalker/keel/compare/v0.6.11...v0.6.12) (2026-11-18)

### Bug Fixes

* **ci:** raise inotify limits for multi-cluster kind ([7c1bfb0](https://github.com/rdylanwalker/keel/commit/7c1bfb06bb1189650c3bf67619da0f68833c6b9b))

## [0.6.11](https://github.com/rdylanwalker/keel/compare/v0.6.10...v0.6.11) (2026-11-18)

### Bug Fixes

* **pko:** propagate optional github token to stack pods ([4ad7dc1](https://github.com/rdylanwalker/keel/commit/4ad7dc1ecf944284024eeed092a20832ca5b0897))

## [0.6.10](https://github.com/rdylanwalker/keel/compare/v0.6.9...v0.6.10) (2026-11-17)

### Bug Fixes

* **catalog:** standardize kindest/node default to v1.35.8 ([22f7372](https://github.com/rdylanwalker/keel/commit/22f7372c5b9a6616c164885df25621ba51fe8369))

## [0.6.9](https://github.com/rdylanwalker/keel/compare/v0.6.8...v0.6.9) (2026-11-17)

### Bug Fixes

* **e2e:** disable live retries and add stack timeout diagnostics ([47b8e48](https://github.com/rdylanwalker/keel/commit/47b8e4865b1dfd17c4da86d92196b68120ee669e))

## [0.6.8](https://github.com/rdylanwalker/keel/compare/v0.6.7...v0.6.8) (2026-11-17)

### Bug Fixes

* **yaml:** ensure trailing newlines in multi-doc and kustomization YAML ([7e1a9ce](https://github.com/rdylanwalker/keel/commit/7e1a9cea865c71ca6f5dd116a8e4275a7765f7bb))

## [0.6.7](https://github.com/rdylanwalker/keel/compare/v0.6.6...v0.6.7) (2026-11-17)

### Bug Fixes

* **kube:** ensure trailing newline in multi-doc YAML output ([f7c0fd7](https://github.com/rdylanwalker/keel/commit/f7c0fd705038f78bb269cc5099488f5d4c143208))

## [0.6.6](https://github.com/rdylanwalker/keel/compare/v0.6.5...v0.6.6) (2026-11-17)

### Bug Fixes

* **ci:** install pulumi on ubuntu-26.04 and drop unused pnpm cache ([0c5375b](https://github.com/rdylanwalker/keel/commit/0c5375bb32a9f385be7a74936efc06a8d87c6581))

## [0.6.5](https://github.com/rdylanwalker/keel/compare/v0.6.4...v0.6.5) (2026-11-17)

### Bug Fixes

* **security:** avoid zero-init key buffer and SHA256 key derivation ([6870e6f](https://github.com/rdylanwalker/keel/commit/6870e6fc5cad7eb4c67c8340518d5ddf1f8154da))
* **security:** resolve GitHub CodeQL code-scanning vulnerabilities ([576dc04](https://github.com/rdylanwalker/keel/commit/576dc048275484a4a5fd82f2125bf49e8d809826))

## [0.6.4](https://github.com/rdylanwalker/keel/compare/v0.6.3...v0.6.4) (2026-11-17)


### Bug Fixes

* **cli:** handle Kyverno webhook startup race and existing S3 bucket ([ec66c3d](https://github.com/rdylanwalker/keel/commit/ec66c3d8c34158ccb54d831b2997ddab8b1becec))

## [0.6.3](https://github.com/rdylanwalker/keel/compare/v0.6.2...v0.6.3) (2026-11-17)


### Bug Fixes

* **ci:** pin terraform-provider version and pass GITHUB_TOKEN ([f45b6a5](https://github.com/rdylanwalker/keel/commit/f45b6a5de205575f946cf9e22b88e8ccc24e2de5))

## [0.6.2](https://github.com/rdylanwalker/keel/compare/v0.6.1...v0.6.2) (2026-11-17)


### Bug Fixes

* **e2e:** fix OIDC login assertion and live DNS resolution ([7cb918f](https://github.com/rdylanwalker/keel/commit/7cb918faf4f15ffa8a9dc07a9c9fb56b6b5ea38c))

## [0.6.1](https://github.com/rdylanwalker/keel/compare/v0.6.0...v0.6.1) (2026-11-17)


### Bug Fixes

* **ui:** align header tenant select height and option labels ([95ba8b2](https://github.com/rdylanwalker/keel/commit/95ba8b27fa6290ed702de64b55ffb57c66f72a2d))
* **ui:** avoid duplicate v prefix on version chips ([a91c2f2](https://github.com/rdylanwalker/keel/commit/a91c2f257cdc5692c29a7cdede5b4f8c32a0d487))

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
