{
  description = "ncspot — a cross-platform ncurses Spotify client written in Rust, using librespot";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    # Lets us materialise the exact toolchain pinned in ./rust-toolchain.toml.
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = {
    self,
    nixpkgs,
    flake-utils,
    rust-overlay,
  }:
    flake-utils.lib.eachDefaultSystem (
      system: let
        overlays = [(import rust-overlay)];
        pkgs = import nixpkgs {inherit system overlays;};
        inherit (pkgs) lib stdenv;

        # Honour the toolchain pinned in ./rust-toolchain.toml (currently 1.96.0 with
        # clippy/rustfmt/rust-analyzer) instead of whatever rustc nixpkgs happens to
        # ship. rust-overlay reads that file directly, so `nix build` and a local
        # `rustup`/`cargo` build always agree on the compiler version.
        rustToolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;

        # buildRustPackage defaults to nixpkgs' cargo/rustc; swap in the pinned one.
        rustPlatform = pkgs.makeRustPlatform {
          cargo = rustToolchain;
          rustc = rustToolchain;
        };
        rustToolchainDev = rustToolchain.override {
          extensions = (lib.importTOML ./rust-toolchain.toml).toolchain.components ++ ["rust-src"];
        };

        # System libraries the *-sys crates link against, derived from the crates
        # actually compiled for the default feature set (see Cargo.lock):
        #   openssl-sys   -> openssl        (native-tls; used by reqwest & rspotify)
        #   libpulse-sys  -> libpulseaudio  (default `pulseaudio_backend` feature)
        #   alsa-sys      -> alsa-lib       (`alsa_backend` feature, enabled below)
        # The clipboard stack (arboard) uses pure-Rust x11rb and a dlopen'd
        # libwayland, so it needs nothing at build time.
        buildInputs =
          [pkgs.openssl]
          ++ lib.optionals stdenv.isLinux [
            pkgs.libpulseaudio
            pkgs.alsa-lib
          ]
          ++ lib.optionals stdenv.isDarwin [
            pkgs.darwin.apple_sdk.frameworks.AppKit
            pkgs.darwin.apple_sdk.frameworks.Security
            pkgs.darwin.apple_sdk.frameworks.SystemConfiguration
          ];

        nativeBuildInputs = [
          pkgs.pkg-config
          # Included on request: ncspot's build does NOT require a Python interpreter,
          # but it is kept here for convenience (ad-hoc scripts, and as a fallback for
          # any future -sys crate whose build.rs shells out to python).
          pkgs.python3
        ];

        # The package; `alsa` says whether the ALSA backend is built in (Linux only).
        mkNcspot = {alsa}:
          rustPlatform.buildRustPackage {
            pname = "ncspot";
            version = (lib.importTOML ./Cargo.toml).package.version;

            src = lib.cleanSource ./.;

            # Reuse the committed lockfile. The librespot-* crates are pinned to a git
            # rev via [patch.crates-io] (see Cargo.toml) to pull the unreleased CDN-URL
            # fallback and oauth refresh-token fixes; git sources need an explicit output
            # hash here. All six share one git rev, so the hashes are identical.
            cargoLock = {
              lockFile = ./Cargo.lock;
              outputHashes = {
                "librespot-audio-0.8.0" = "sha256-RZWJFXeSaiCaR5kRd9uZQc2xMIBT/BEyI23EFaqwUac=";
                "librespot-core-0.8.0" = "sha256-RZWJFXeSaiCaR5kRd9uZQc2xMIBT/BEyI23EFaqwUac=";
                "librespot-metadata-0.8.0" = "sha256-RZWJFXeSaiCaR5kRd9uZQc2xMIBT/BEyI23EFaqwUac=";
                "librespot-oauth-0.8.0" = "sha256-RZWJFXeSaiCaR5kRd9uZQc2xMIBT/BEyI23EFaqwUac=";
                "librespot-playback-0.8.0" = "sha256-RZWJFXeSaiCaR5kRd9uZQc2xMIBT/BEyI23EFaqwUac=";
                "librespot-protocol-0.8.0" = "sha256-RZWJFXeSaiCaR5kRd9uZQc2xMIBT/BEyI23EFaqwUac=";
              };
            };

            inherit nativeBuildInputs buildInputs;

            # On Linux, also build the ALSA backend. librespot lists it before PulseAudio,
            # so it is what plays when the config names no backend. Through PipeWire's ALSA
            # plugin it reaches PipeWire directly, without the pipewire-pulse server and
            # libpulse's own thread in between, which wake up several hundred times a
            # second for one stream. `backend = "pulseaudio"` in the config goes back, and so
          # does the `ncspot-pulseaudio` package, which leaves the ALSA backend out.
            buildFeatures = lib.optionals (alsa && stdenv.isLinux) ["alsa_backend"];

            # The workspace also contains an `xtask` helper crate; only build ncspot.
            cargoBuildFlags = [
              "-p"
              "ncspot"
            ];

            # The test suite reaches out to the Spotify API / needs credentials, so it
            # cannot run inside the hermetic build sandbox.
            doCheck = false;

            # Link against the system OpenSSL via pkg-config instead of vendoring it.
            OPENSSL_NO_VENDOR = 1;

            # buildRustPackage's install phase only copies the compiled binary, so the
            # desktop launcher and app icon that ship in the source tree would be lost.
            # Install them the same way nixpkgs' ncspot does so the app appears in menus
            # and app launchers (misc/ncspot.desktop sets Terminal=true for the TUI).
            postInstall = ''
              install -D --mode=444 misc/ncspot.desktop $out/share/applications/ncspot.desktop
              install -D --mode=444 images/logo.svg $out/share/icons/hicolor/scalable/apps/ncspot.svg
            '';

            meta = {
              description = "Cross-platform ncurses Spotify client written in Rust, using librespot";
              homepage = "https://github.com/hrkfdn/ncspot";
              license = lib.licenses.bsd2;
              mainProgram = "ncspot";
              platforms = lib.platforms.unix;
            };
          };
      in {
        packages.default = self.packages.${system}.ncspot;

        packages.ncspot = mkNcspot {alsa = true;};

        # The way back: PulseAudio only, as before the ALSA backend was added.
        packages.ncspot-pulseaudio = mkNcspot {alsa = false;};

        # `nix run` -> launch ncspot directly.
        apps.default = {
          type = "app";
          program = lib.getExe self.packages.${system}.ncspot;
        };

        # `nix develop` -> same toolchain and libraries as the package build, plus the
        # components pinned in rust-toolchain.toml (clippy, rustfmt, rust-analyzer).
        # pkg-config discovers the buildInputs automatically because both it and the
        # libraries are present in the shell's inputs.
        devShells.default = pkgs.mkShell {
          inherit buildInputs;
          nativeBuildInputs = nativeBuildInputs ++ [rustToolchainDev];
        };

        formatter = pkgs.nixfmt-rfc-style;
      }
    );
}
