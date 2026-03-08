{
  description = "st0x Oracle Server - Signed context oracle for st0x tokenized equities";

  inputs = {
    rainix.url = "github:rainlanguage/rainix";
    flake-utils.url = "github:numtide/flake-utils";
    crane.url = "github:ipetkov/crane";
  };

  outputs = { self, flake-utils, rainix, crane }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = rainix.pkgs.${system};
        craneLib =
          (crane.mkLib pkgs).overrideToolchain rainix.rust-toolchain.${system};
        st0xRust = pkgs.callPackage ./rust.nix { inherit craneLib; };
      in {
        packages = rainix.packages.${system} // {
          st0x-oracle-server = st0xRust.package;
          default = st0xRust.package;

          prepSolArtifacts = rainix.mkTask.${system} {
            name = "prep-sol-artifacts";
            additionalBuildInputs = rainix.sol-build-inputs.${system};
            body = ''
              set -euxo pipefail
              (cd lib/rain.math.float/ && forge build)
            '';
          };

          oracle-rs-test = rainix.mkTask.${system} {
            name = "oracle-rs-test";
            body = ''
              set -euxo pipefail
              cargo test
            '';
          };

          oracle-rs-static = rainix.mkTask.${system} {
            name = "oracle-rs-static";
            body = ''
              set -euxo pipefail
              cargo fmt --all -- --check
              cargo clippy -- -D warnings
            '';
          };
        };

        devShells.default = pkgs.mkShell {
          packages = with pkgs; [
            gh
            self.packages.${system}.prepSolArtifacts
            self.packages.${system}.oracle-rs-test
            self.packages.${system}.oracle-rs-static
          ];
          shellHook = rainix.devShells.${system}.default.shellHook;
          buildInputs = rainix.devShells.${system}.default.buildInputs;
          nativeBuildInputs = rainix.devShells.${system}.default.nativeBuildInputs;
        };
      }
    );
}
