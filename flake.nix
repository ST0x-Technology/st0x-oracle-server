{
  description = "st0x Oracle Server — Signed context oracle for st0x tokenized equities on Raindex";

  inputs = {
    rainix.url = "github:rainlanguage/rainix";
    nixpkgs.follows = "rainix/nixpkgs";
    flake-utils.url = "github:numtide/flake-utils";

    crane.url = "github:ipetkov/crane";
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
      rainix,
      crane,
      ...
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = rainix.pkgs.${system};

        craneLib = (crane.mkLib pkgs).overrideToolchain rainix.rust-toolchain.${system};

        rust = pkgs.callPackage ./rust.nix { inherit craneLib; };

        oracle-rs-test = rainix.mkTask.${system} {
          name = "oracle-rs-test";
          body = ''
            set -euxo pipefail
            cargo test --workspace --all-targets
          '';
        };

        oracle-rs-static = rainix.mkTask.${system} {
          name = "oracle-rs-static";
          body = ''
            set -euxo pipefail
            cargo fmt --all -- --check
            cargo clippy --workspace --all-targets --no-deps -- -D warnings
          '';
        };

      in
      {
        packages = {
          inherit oracle-rs-test oracle-rs-static;

          st0x-oracle-server = rust.package;
          default = rust.package;
        };

        devShells.default = pkgs.mkShell {
          inputsFrom = [ rainix.devShells.${system}.rust-shell ];
          packages = [
            oracle-rs-test
            oracle-rs-static
          ];
        };
      }
    );
}
