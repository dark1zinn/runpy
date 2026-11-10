{
  description = "Development shell for the runpy Rust crate and runpyrs Python SDK";

  inputs = {
    nixpkgs.url = "https://flakehub.com/f/NixOS/nixpkgs/0.1"; # unstable Nixpkgs
    fenix = {
      url = "https://flakehub.com/f/nix-community/fenix/0.1";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    { self, ... }@inputs:

    let
      # Shells evaluate for all four systems; CI executes them natively on
      # x86_64 Linux and Apple Silicon macOS only.
      supportedSystems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];
      forEachSupportedSystem =
        f:
        inputs.nixpkgs.lib.genAttrs supportedSystems (
          system:
          f {
            inherit system;
            pkgs = import inputs.nixpkgs {
              inherit system;
              overlays = [
                inputs.self.overlays.default
              ];
            };
          }
        );

      # The Rust release CI and `rust-version` target. The hash is that of
      # https://static.rust-lang.org/dist/channel-rust-1.93.0.toml, checked
      # against the published .sha256.
      rustVersion = "1.93.0";
    in
    {
      overlays.default = final: prev: {
        rustToolchain =
          (inputs.fenix.packages.${prev.stdenv.hostPlatform.system}.fromToolchainName {
            name = rustVersion;
            sha256 = "sha256-vra6TkHITpwRyA5oBKAHSX0Mi6CBDNQD+ryPSpxFsfg=";
          }).withComponents
            [
              "cargo"
              "clippy"
              "rust-src"
              "rustc"
              "rustfmt"
            ];
      };

      devShells = forEachSupportedSystem (
        { pkgs, ... }:
        {
          default = pkgs.mkShell {
            # Python tools (pytest, ruff, mypy, Hypothesis, build, pip-audit)
            # come from the uv lock, not from Nix.
            packages = with pkgs; [
              rustToolchain
              rust-analyzer
              cargo-deny
              cargo-edit
              cargo-watch
              python314
              uv
              git
              gh
              actionlint
            ];

            env = {
              # Required by rust-analyzer
              RUST_SRC_PATH = "${pkgs.rustToolchain}/lib/rustlib/src/rust/library";
              RUST_BACKTRACE = "1";
            };
          };
        }
      );

      # `nix flake check` builds the shell and confirms the documented tools
      # run with the pinned versions on the host system.
      checks = forEachSupportedSystem (
        { pkgs, system }:
        {
          devShell = self.devShells.${system}.default;
          tools =
            pkgs.runCommand "runpy-dev-shell-tools"
              {
                nativeBuildInputs = self.devShells.${system}.default.nativeBuildInputs;
              }
              ''
                rustc --version | grep -F "rustc ${rustVersion} "
                cargo --version
                cargo clippy --version
                cargo fmt --version
                cargo deny --version
                python3.14 --version
                uv --version
                git --version
                gh --version
                actionlint --version
                touch $out
              '';
        }
      );
    };
}
