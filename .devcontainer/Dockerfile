# Official Rust Alpine image (musl-native), pinned by digest for reproducible
# builds; the tag matches the channel in rust-toolchain.toml. Alpine (not Debian)
# for a musl-native gcc: the C our deps vendor (ring for rustls, zstd) then
# targets musl directly, so the `task` binary links no system C libraries and
# ships as a single static ELF.
#
# Named so both build.sh backends can reference it: `docker build -t task-build`
# tags it, and the `vk` backend resolves the base by this stage name.
FROM rust:1.97.1-alpine3.21@sha256:7bae7c67364dad5ebbd4060923b34d734fbed66d7c1cf3af72aa2f062af93eb6 AS task-build

# Build deps (one per line in apk-pins.txt, pinned to exact versions for byte-for-byte
# reproducibility — Alpine has no snapshot service, so the pins + the image digest above
# are what freeze the inputs; regenerate with ./update.sh):
#   gcc + musl-dev          ring/zstd's vendored C compiles; Alpine's gcc is musl-native,
#                           so it is also the musl C compiler.
#   git, ca-certificates, file   dev/build basics.
COPY apk-pins.txt /tmp/apk-pins.txt
RUN apk add --no-cache $(grep -vE '^[[:space:]]*(#|$)' /tmp/apk-pins.txt)

# Lint/format components; x86_64-unknown-linux-musl is Alpine's native target, so
# no `rustup target add`. Versions follow rust-toolchain.toml.
RUN rustup component add clippy rustfmt

# cargo-audit baked in so audit.sh needs no on-demand install. Not a rustup component, so
# install the pinned version as a binary; --locked builds it from its published Cargo.lock
# so the image stays reproducible. Lands in CARGO_HOME/bin (on PATH) and is chowned below.
RUN cargo install cargo-audit --locked --version 0.22.2

# Non-root dev user owning the cargo/rustup homes so it can honour
# rust-toolchain.toml and populate the registry cache without root.
ARG USERNAME=dev
RUN adduser -D -s /bin/sh "${USERNAME}" \
 && chown -R "${USERNAME}": "${CARGO_HOME}" "${RUSTUP_HOME}"
USER ${USERNAME}
