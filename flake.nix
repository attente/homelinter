{
  description = "Rust development environment";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, flake-utils, fenix }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = nixpkgs.legacyPackages.${system};

        # Single source of truth for the Rust version, parsed from
        # ./rust-toolchain.toml (the sha256 is pinned at flake time).
        toolchain = fenix.packages.${system}.fromToolchainFile {
          file = ./rust-toolchain.toml;
          sha256 = "sha256-AJ6LX/Q/Er9kS15bn9iflkUwcgYqRQxiOIL2ToVAXaU=";
        };

        # Fenix's standalone rust-analyzer build
        rustAnalyzer = fenix.packages.${system}.rust-analyzer;

        devTools = with pkgs; [
          cargo-nextest
          cargo-audit
          cargo-deny
          taplo
        ];
      in
      {
        devShells.default = pkgs.mkShell {
          # Flat list; nested lists are deprecated in Nixpkgs >= 26.05.
          packages = [ toolchain rustAnalyzer pkgs.pkg-config ] ++ devTools;

          # Native libraries your crates link against, when needed:
          # buildInputs = with pkgs; [ openssl sqlite ];
          # Their pkgconfig -dev outputs land in PKG_CONFIG_PATH automatically,
          # and stdenv rpaths libs into built binaries, so neither needs manual
          # wiring. The one gap: crates that dlopen a .so by name at runtime
          # (bevy/raylib-style, libudev) cannot find it via rpath; uncomment
          # and name the exact package when that happens:
          # LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath [ pkgs.libudev-zero ];

          env = {
            RUST_BACKTRACE = "1";
            # Legacy fallback so older IDE tooling finds std sources.
            # Modern rust-analyzer locates them via the sysroot instead.
            RUST_SRC_PATH = "${toolchain}/lib/rustlib/src/rust/library";
          };

          shellHook = ''
            echo "Rust $(rustc --version) ready"
          '';
        };
      });
}