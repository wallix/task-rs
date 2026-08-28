# Official Rust Alpine image (musl-native), pinned by digest for reproducible
# builds; the tag matches the channel in rust-toolchain.toml. Alpine (not Debian)
# for a musl-native gcc: the C our deps vendor (ring for rustls, zstd) then
# targets musl directly, so the `task` binary links no system C libraries and
# ships as a single static ELF.
#
# `task-build` contains the build, lint, and format tools. The default
# `task-audit` stage adds cargo-audit.
FROM rust:1.97.1-alpine3.21@sha256:7bae7c67364dad5ebbd4060923b34d734fbed66d7c1cf3af72aa2f062af93eb6 AS task-build

# Build deps (one per line in apk-pins.txt, pinned to exact versions for byte-for-byte
# reproducibility — Alpine has no snapshot service, so the pins + the image digest above
# are what freeze the inputs; regenerate with ./update.sh):
#   gcc + musl-dev          ring/zstd's vendored C compiles; Alpine's gcc is musl-native,
#                           so it is also the musl C compiler.
#   git, ca-certificates, file   dev/build basics.
COPY apk-pins.txt /tmp/apk-pins.txt
RUN apk add --no-cache $(grep -vE '^[[:space:]]*(#|$)' /tmp/apk-pins.txt)

# Run Rust tools as a non-root user with writable Cargo and rustup homes.
ARG USERNAME=dev
RUN adduser -D -s /bin/sh "${USERNAME}" \
 && chown -R "${USERNAME}": "${CARGO_HOME}" "${RUSTUP_HOME}"
USER ${USERNAME}

# Install the components listed in rust-toolchain.toml in the base stage.
RUN rustup component add clippy rustfmt

FROM task-build AS task-audit

# cargo-audit is not a rustup component. Build its pinned dependency graph.
RUN cargo install cargo-audit --locked --version 0.22.2
