# syntax=docker/dockerfile:1
#
# Keel container image (SDD §9.2, NFR-PERF-04, NFR-SEC-06).
#
#   docker build -t keel:dev .                     # default `runtime` target (keel serve)
#   docker build --target static -t keel:static .  # binary-only distroless/static image
#   docker buildx build --platform linux/amd64,linux/arm64 .
#
# Stages:
#   1. ui-build    - builds the React SPA with the frozen pnpm lockfile (build platform).
#   2. rust-build  - cross-compiles a fully static musl `keel` binary with cargo-zigbuild on the
#                    build platform for $TARGETARCH (no QEMU needed for the Rust build), embedding
#                    the SPA via rust-embed.
#   3. static      - gcr.io/distroless/static (nonroot) with only the static binary.
#   4. runtime     - DEFAULT. Chainguard wolfi-base (nonroot) plus the external CLIs that
#                    `keel serve` shells out to at runtime: `git` (+ `ssh` for SSH remotes,
#                    AGENTS.md §3.5), `sops`, and `age-keygen`. A pure distroless/static image
#                    cannot run `keel serve`, which initializes its Git workdir on startup.
#
# Base images are pinned by digest; Renovate's `dockerfile` manager keeps tags + digests current.

ARG NODE_IMAGE=node:26-alpine@sha256:6bd9ab43c0ec0190ca7b2d193906cb8b7edf6e7f4afa43ddeccd6452710bffb5
ARG RUST_IMAGE=rust:1.100-trixie@sha256:223cc32ddca8f3ee90c436dbc94b6b5f052ea7f9653c5201e9612f08ad7d21b8
ARG STATIC_IMAGE=gcr.io/distroless/static-debian13:nonroot@sha256:aafe993665bf6200db38a02c11a9351e782a84b6a45e7f33b6695638c88e3931
ARG RUNTIME_IMAGE=cgr.dev/chainguard/wolfi-base:latest@sha256:be019c1c74fe69204a42c4e10bc4d886a49bf5403ffa74374ef7baa408fc9a26

# ---------------------------------------------------------------------------------------------
# 1. UI build (architecture-independent output, so always run on the build platform)
# ---------------------------------------------------------------------------------------------
FROM --platform=$BUILDPLATFORM ${NODE_IMAGE} AS ui-build
# renovate: datasource=npm depName=pnpm
ARG PNPM_VERSION=12.21.0
RUN npm install --global --no-fund --no-audit "pnpm@${PNPM_VERSION}"
WORKDIR /src/ui
COPY ui/package.json ui/pnpm-lock.yaml ui/pnpm-workspace.yaml ./
RUN --mount=type=cache,id=keel-pnpm-store,target=/root/.local/share/pnpm/store \
    pnpm install --frozen-lockfile
COPY ui/ ./
RUN pnpm build

# ---------------------------------------------------------------------------------------------
# 2. Rust build: static musl binary via cargo-zigbuild
# ---------------------------------------------------------------------------------------------
FROM --platform=$BUILDPLATFORM ${RUST_IMAGE} AS rust-build
ARG TARGETARCH
# Zig is bumped manually together with its SHA-256s (keep in sync with release.yml ZIG_VERSION).
ARG ZIG_VERSION=0.16.0
ARG ZIG_SHA256_X86_64=70e49664a74374b48b51e6f3fdfbf437f6395d42509050588bd49abe52ba3d00
ARG ZIG_SHA256_AARCH64=ea4b09bfb22ec6f6c6ceac57ab63efb6b46e17ab08d21f69f3a48b38e1534f17
# renovate: datasource=crate depName=cargo-zigbuild
ARG CARGO_ZIGBUILD_VERSION=0.24.1
RUN set -eux; \
    arch="$(uname -m)"; \
    case "${arch}" in \
      x86_64) sha="${ZIG_SHA256_X86_64}" ;; \
      aarch64) sha="${ZIG_SHA256_AARCH64}" ;; \
      *) echo "unsupported build platform: ${arch}" >&2; exit 1 ;; \
    esac; \
    curl -fsSLo /tmp/zig.tar.xz "https://ziglang.org/download/${ZIG_VERSION}/zig-${arch}-linux-${ZIG_VERSION}.tar.xz"; \
    echo "${sha}  /tmp/zig.tar.xz" | sha256sum -c -; \
    mkdir -p /opt/zig; \
    tar -xJf /tmp/zig.tar.xz --strip-components=1 -C /opt/zig; \
    ln -s /opt/zig/zig /usr/local/bin/zig; \
    rm /tmp/zig.tar.xz; \
    zig version
RUN --mount=type=cache,id=keel-cargo-registry,target=/usr/local/cargo/registry \
    cargo install --locked "cargo-zigbuild@${CARGO_ZIGBUILD_VERSION}" \
 && rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl

WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates/ crates/
COPY deploy/ deploy/
COPY --from=ui-build /src/ui/dist/ ui/dist/
RUN --mount=type=cache,id=keel-cargo-registry,target=/usr/local/cargo/registry \
    --mount=type=cache,id=keel-target-${TARGETARCH},target=/src/target \
    set -eux; \
    case "${TARGETARCH}" in \
      amd64) target=x86_64-unknown-linux-musl ;; \
      arm64) target=aarch64-unknown-linux-musl ;; \
      *) echo "unsupported TARGETARCH: ${TARGETARCH}" >&2; exit 1 ;; \
    esac; \
    cargo zigbuild --release --locked --target "${target}" --bin keel; \
    install -D -m 0755 "target/${target}/release/keel" /out/keel

# ---------------------------------------------------------------------------------------------
# 3. Binary-only static image (CLI use; `keel serve` needs the `runtime` target's git/sops/age)
# ---------------------------------------------------------------------------------------------
FROM ${STATIC_IMAGE} AS static
LABEL org.opencontainers.image.title="keel" \
      org.opencontainers.image.description="Keel GitOps control plane (static binary only)" \
      org.opencontainers.image.licenses="MIT"
COPY --from=rust-build /out/keel /usr/local/bin/keel
USER 65532:65532
WORKDIR /home/nonroot
ENTRYPOINT ["/usr/local/bin/keel"]
CMD ["serve", "--bind", "0.0.0.0:8080"]

# ---------------------------------------------------------------------------------------------
# 4. Default runtime image for `keel serve`
# ---------------------------------------------------------------------------------------------
FROM ${RUNTIME_IMAGE} AS runtime
LABEL org.opencontainers.image.title="keel" \
      org.opencontainers.image.description="Keel GitOps control plane (keel serve)" \
      org.opencontainers.image.licenses="MIT"
RUN apk add --no-cache git openssh-client sops age ca-certificates-bundle
COPY --from=rust-build /out/keel /usr/local/bin/keel
USER 65532:65532
ENV HOME=/home/nonroot
WORKDIR /home/nonroot
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/keel"]
CMD ["serve", "--bind", "0.0.0.0:8080"]
