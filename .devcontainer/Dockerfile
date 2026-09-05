# The build image: the pinned toolchain every release build, lint, test and audit runs in.
# build.sh / lint.sh / fmt.sh / audit.sh build this file (docker build, or `vk run --file`)
# and run their command inside it.
#
# Two pins freeze the inputs, and both are archival:
#   - the base: the official nixos/nix image, by manifest-list digest below — a public
#     Docker Hub image pulled like any other base, nothing is pushed anywhere. The
#     manifest list resolves to the runner's architecture, which is what makes the same
#     Dockerfile build the x86_64 and aarch64 releases natively.
#   - the packages: nix/flake.nix + nix/flake.lock (nixpkgs and rust-overlay by commit).
#     `nix build` runs INSIDE this image at build time to realize that closure, so the
#     host needs no Nix, only docker or vk. Sources are content-addressed: the same lock
#     rebuilds the exact toolchain years later, from cache.nixos.org or from source.

# nixos/nix 2.35.2 by manifest-list digest. ./update.sh refreshes it
# (`docker buildx imagetools inspect nixos/nix:<ver>` -> Digest).
FROM nixos/nix:2.35.2@sha256:7a007c766426c1877758ddc5cb87a965ac131fc78c582ce0083d922d51ae945c AS task-build

# Flakes are gated behind experimental-features on the stock image, and the Nix sandbox
# needs user namespaces neither Docker's builder nor vk's give it. Turning the sandbox off
# is the one gap in the isolation story: a derivation served by cache.nixos.org is a
# signed binary and unaffected, but anything that has to be BUILT here sees this
# container's filesystem rather than an empty one.
RUN mkdir -p /etc/nix \
 && printf 'experimental-features = nix-command flakes\nsandbox = false\n' >> /etc/nix/nix.conf

# The flake pins Rust inline because rust-toolchain.toml is outside the build context.
# ./update.sh keeps the channels in sync. Commit the archival flake.lock alongside it.
COPY nix /src/nix

# Install Rust, musl cross gcc/binutils, host gcc, GNU test tools, git and cargo-audit
# in /opt/toolchain. --no-update-lock-file rejects a missing or stale lock instead of
# resolving the input branches while build-info.txt records the committed lock's digest.
RUN nix build --no-update-lock-file "path:/src/nix#buildEnv" --out-link /opt/toolchain

# PATH prepends /opt/toolchain/bin to the base image's own (nix's profile dirs, spelled
# out because the base is pinned). SSL_CERT_FILE lets cargo reach crates.io as any uid —
# the scripts run as the host user with HOME=/tmp. Which compilers cargo uses for the musl
# target is declared in .cargo/config.toml, not here.
ENV PATH=/opt/toolchain/bin:/root/.nix-profile/bin:/nix/var/nix/profiles/default/bin:/nix/var/nix/profiles/default/sbin \
    SSL_CERT_FILE=/opt/toolchain/etc/ssl/certs/ca-bundle.crt

# Give dev (uid 1000, the usual first host user) a passwd entry for name/home lookups.
# Docker uses HOME=/tmp for arbitrary uids, so /tmp must be world-writable. The leading
# \n handles a base /etc/passwd without a trailing newline.
RUN printf '\ndev:x:1000:1000:dev:/home/dev:/bin/sh\n' >> /etc/passwd \
 && printf '\ndev:x:1000:\n' >> /etc/group \
 && mkdir -p /home/dev && chown 1000:1000 /home/dev \
 && chmod 1777 /tmp

# Run unprivileged by default, as the previous image did. The docker paths override this
# with `--user "$(id -u):$(id -g)"`; the vk paths pass no user, and lint.sh and fmt.sh
# pass no HOME either — /home/dev is the writable cargo home they land in.
USER 1000:1000
