{
  # .devcontainer/Dockerfile installs this toolchain as `buildEnv`. Keeping it under
  # .devcontainer/nix puts it in the Docker/vk build context for `COPY nix /src/nix`.
  #
  # Static-musl releases need:
  #   - Rust with the host arch's musl target, clippy and rustfmt. ./update.sh keeps the
  #     inline channel in sync with rust-toolchain.toml; see rustToolchain below.
  #   - musl cross gcc/binutils for ring and zstd-sys's vendored C, and host gcc for
  #     build scripts and proc-macros. .cargo/config.toml selects the compilers.
  #   - GNU coreutils, bash, sed, grep, find and diff for the test corpus; git,
  #     ca-certificates and cargo-audit for audit.sh.
  #
  # Linux releases are native on x86_64-linux and aarch64-linux; the system determines
  # the musl target. macOS and Windows use their own runners, outside this closure.
  #
  # Everything is pinned by flake.lock (nixpkgs and rust-overlay by git rev), so the inputs
  # are rebuildable-from-source years later. cache.nixos.org serves the nixpkgs half; the
  # compiler itself is a hash-pinned fetch of the official static.rust-lang.org tarball,
  # which is kept for every release. Nix runs only INSIDE the build image (a `RUN nix
  # build` at image-build time) — no Nix on any host.
  #
  # On a host with Nix:  nix develop ./.devcontainer/nix    (the same toolchain, interactively)
  #                      nix build ./.devcontainer/nix#buildEnv

  inputs = {
    # flake.lock pins the commit; this is the branch `nix flake update` follows. It is
    # nixos-unstable rather than a release branch because rust-overlay tracks it, and a
    # release build wants the newest stable Rust the day it ships — the lock is what makes
    # that reproducible, so the branch only decides what the next ./update.sh picks up.
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, rust-overlay }:
    let
      # genAttrs covers both systems without adding a flake-utils input to pin.
      systems = [ "x86_64-linux" "aarch64-linux" ];
      eachSystem = nixpkgs.lib.genAttrs systems;

      envFor = system:
        let
          pkgs = import nixpkgs {
            inherit system;
            overlays = [ rust-overlay.overlays.default ];
          };

          # Compile ring and zstd-sys's C for musl, as Alpine's musl-native gcc did.
          # cc-rs selects a musl compiler only for the target triple; the glibc-hosted
          # rustc compiles Rust for musl. Each runner uses its native architecture.
          # pkgsCross, unlike pkgsMusl, names compilers `<triple>-cc`, matching
          # .cargo/config.toml without colliding with host `cc` in the merged /bin.
          muslPkgs = {
            x86_64-linux = pkgs.pkgsCross.musl64;
            aarch64-linux = pkgs.pkgsCross.aarch64-multiplatform-musl;
          }.${system};
          # x86_64-unknown-linux-musl / aarch64-unknown-linux-musl — nixpkgs' config string
          # for these two triples is also their rustc target name.
          muslTarget = muslPkgs.stdenv.hostPlatform.config;

          # Exact toolchain: channel 1.97.1 — the same channel, minimal profile and
          # clippy/rustfmt components ../../rust-toolchain.toml pins, kept in sync by
          # ./update.sh, plus the musl target the release build needs (that file
          # deliberately pins no target). Reading it directly (rust-overlay's
          # fromRustupToolchainFile) would require the flake at the repo ROOT — a flake
          # cannot read `..` outside its own dir in pure eval — so with the flake under
          # .devcontainer/nix/ the channel is inline.
          rustToolchain = pkgs.rust-bin.stable."1.97.1".minimal.override {
            extensions = [ "clippy" "rustfmt" ];
            targets = [ muslTarget ];
          };

          buildTools = with pkgs; [
            rustToolchain
            # The musl cross cc. It propagates its bintools, so `<triple>-{cc,gcc,ar,
            # ranlib,ld,...}` all land in the merged /bin — .cargo/config.toml names the
            # first two as CC_<triple>/AR_<triple> for the vendored C in ring and
            # zstd-sys, and points the musl target's linker at the same driver, so
            # nothing musl goes through the glibc one.
            muslPkgs.stdenv.cc
            # Build scripts and proc-macros target the glibc host and link with `cc`.
            # Host cc/gcc/ld are unprefixed, so they do not clash with the musl tools.
            stdenv.cc
            git
            cacert
            cargo-audit # audit.sh (RUSTSEC scan)
            # The `cargo test --workspace` parity corpus uses cp, cat, touch, sleep,
            # test, etc. Use GNU tools to match Ubuntu runners. bash also provides
            # `sh` for build.sh's `docker run ... sh -c "$BUILD_CMD"`.
            coreutils
            bash
            gnugrep
            gnused
            findutils
            diffutils
          ];

          # Merge the toolchain under one PATH prefix. buildEnv defaults to all of
          # "/"; link only /bin, /etc for cacert's ca-bundle.crt (SSL_CERT_FILE), and
          # /lib + /libexec for gcc's runtime and compiler executables. The image
          # does not read docs or headers, so omit /share and /include.
          binEnv = pkgs.buildEnv {
            name = "task-build-env";
            paths = buildTools;
            pathsToLink = [ "/bin" "/etc" "/lib" "/libexec" ];
          };
        in
        { inherit pkgs muslTarget buildTools binEnv; };
    in
    {
      # `nix develop ./.devcontainer/nix` provides the build toolchain interactively.
      # .cargo/config.toml supplies CC_*/AR_*/linker settings, so
      # `cargo build --target <musl triple>` behaves as it does in the image.
      devShells = eachSystem (system:
        let env = envFor system; in
        {
          default = env.pkgs.mkShell {
            packages = env.buildTools;
            SOURCE_DATE_EPOCH = "0";
            shellHook = ''
              echo "task nix devShell — $(rustc --version), musl target: ${env.muslTarget}"
            '';
          };
        });

      # One closure with a merged /bin. .devcontainer/Dockerfile runs
      # `nix build .#buildEnv --out-link /opt/toolchain` inside nixos/nix and adds it
      # to PATH. /opt/toolchain keeps store hashes out of the Dockerfile.
      # Nix itself neither builds nor pushes an image.
      packages = eachSystem (system:
        let env = envFor system; in
        {
          buildEnv = env.binEnv;
          default = env.binEnv;
        });
    };
}
