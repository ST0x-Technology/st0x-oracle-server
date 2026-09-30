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

          # OCI image for Cloud Run — the nix-built binary containerised
          # with NO base image: just the binary's runtime closure + CA
          # certs. No config is baked in: every service mounts its config
          # and sets CONFIG_PATH, and one that does not fails to boot
          # instead of starting on stale rows. No shell, no package manager —
          # less surface than distroless. `created` is pinned so the same
          # commit rebuilds to the same digest (promote-by-digest =
          # content-address of the commit). Built in CI (linux) and
          # streamed to Artifact Registry; secrets stay in Secret Manager
          # env injection, never in the image.
          oci = pkgs.dockerTools.streamLayeredImage {
            name = "st0x-oracle-server";
            tag = "latest";
            created = "1970-01-01T00:00:01Z";
            contents = [
              rust.package
              pkgs.cacert
            ];
            config = {
              Cmd = [ "${rust.package}/bin/st0x-oracle-server" ];
              Env = [
                "SSL_CERT_FILE=/etc/ssl/certs/ca-bundle.crt"
              ];
              ExposedPorts."3000/tcp" = { };
            };
          };
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
