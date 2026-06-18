{
  description = "st0x Oracle Server — Signed context oracle for st0x tokenized equities on Raindex";

  inputs = {
    rainix.url = "github:rainlanguage/rainix";
    nixpkgs.follows = "rainix/nixpkgs";
    flake-utils.url = "github:numtide/flake-utils";

    crane.url = "github:ipetkov/crane";

    ragenix.url = "github:yaxitech/ragenix";
    ragenix.inputs.nixpkgs.follows = "nixpkgs";

    deploy-rs.url = "github:serokell/deploy-rs";
    deploy-rs.inputs.nixpkgs.follows = "nixpkgs";

    disko.url = "github:nix-community/disko";
    disko.inputs.nixpkgs.follows = "nixpkgs";

    nixos-anywhere.url = "github:nix-community/nixos-anywhere";
    nixos-anywhere.inputs.nixpkgs.follows = "nixpkgs";

    # Used ONLY to source `tailscale` past 1.98.2. The rainix-pinned
    # nixpkgs ships tailscale 1.98.0, which has a regression
    # (NixOS/nixpkgs#520715) that breaks MagicDNS by rewriting
    # systemd-resolved to point at Tailscale's public ts.net
    # nameservers instead of the local MagicDNS resolver. Filed
    # upstream as rainlanguage/rainix#183; remove this input once
    # the rainix nixpkgs pin moves past 1.98.2.
    nixpkgs-tailscale-fix.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs =
    {
      self,
      nixpkgs,
      nixpkgs-tailscale-fix,
      flake-utils,
      rainix,
      crane,
      ragenix,
      deploy-rs,
      disko,
      nixos-anywhere,
      ...
    }:
    let
      nodeName = "st0x-oracle-server";

      deployModule = import ./deploy.nix { inherit deploy-rs self; };
    in
    {
      nixosConfigurations.${nodeName} = nixpkgs.lib.nixosSystem {
        system = "x86_64-linux";
        specialArgs = {
          inherit (self.packages.x86_64-linux) st0x-oracle-server;
          tailscalePkg = (import nixpkgs-tailscale-fix { system = "x86_64-linux"; }).tailscale;
        };
        modules = [
          disko.nixosModules.disko
          ragenix.nixosModules.default
          ./os.nix
        ];
      };

      deploy = deployModule.config;
    }
    // flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = rainix.pkgs.${system};

        nixpkgsForTf = import nixpkgs {
          inherit system;
          config.allowUnfreePredicate = pkg: builtins.elem (pkgs.lib.getName pkg) [ "terraform" ];
        };

        craneLib = (crane.mkLib pkgs).overrideToolchain rainix.rust-toolchain.${system};

        rust = pkgs.callPackage ./rust.nix { inherit craneLib; };

        infraPkgs = import ./infra {
          inherit ragenix rainix system;
          pkgs = nixpkgsForTf;
        };

        deployWrappers = deployModule.wrappers {
          inherit pkgs infraPkgs;
          localSystem = system;
        };

        # First-boot bootstrap: terraform-apply has already provisioned
        # the droplet; this wipes Ubuntu, installs NixOS via
        # nixos-anywhere, waits for the reboot, reads the new host key,
        # and rewrites it into keys.nix in-place. Subsequent rolls go
        # through deploy-rs.
        bootstrap-nixos = rainix.mkTask.${system} {
          name = "bootstrap-nixos";
          additionalBuildInputs = infraPkgs.buildInputs ++ [ nixos-anywhere.packages.${system}.default ];
          body = ''
            ${infraPkgs.resolveIp}

            ssh_opts="-o StrictHostKeyChecking=no -o ConnectTimeout=5 -i $identity"

            nixos-anywhere --flake .#${nodeName} \
              --option pure-eval false \
              --ssh-option "IdentityFile=$identity" \
              --target-host "root@$host_ip" "$@"

            echo "Waiting for host to come back up..."
            retries=0
            until ssh $ssh_opts "root@$host_ip" true 2>/dev/null; do
              retries=$((retries + 1))
              if [ "$retries" -ge 60 ]; then
                echo "Host did not come back up after 5 minutes" >&2
                exit 1
              fi
              sleep 5
            done

            new_key=$(
              ssh $ssh_opts "root@$host_ip" \
                cat /etc/ssh/ssh_host_ed25519_key.pub \
                | awk '{print $1 " " $2}'
            )

            valid_key='^ssh-ed25519 [A-Za-z0-9+/=]+$'
            if [ -z "$new_key" ] || ! echo "$new_key" | grep -qE "$valid_key"; then
              echo "ERROR: SSH host key is empty or malformed: '$new_key'" >&2
              exit 1
            fi

            ${pkgs.gnused}/bin/sed -i \
              "/host =$/{n;s|\"ssh-ed25519 [A-Za-z0-9+/=_]*\"|\"$new_key\"|;}" \
              keys.nix

            echo "Updated host key in keys.nix. Now: tf-rekey to re-encrypt"
            echo "Terraform vars/state, then commit keys.nix + secrets/."
          '';
        };

        remote = rainix.mkTask.${system} {
          name = "remote";
          additionalBuildInputs = infraPkgs.buildInputs;
          body = ''
            ${infraPkgs.parseIdentity}
            exec ssh -i "$identity" "root@${nodeName}" "$@"
          '';
        };

        tf-rekey = rainix.mkTask.${system} {
          name = "tf-rekey";
          additionalBuildInputs = infraPkgs.buildInputs;
          body = infraPkgs.tfRekey;
        };

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
          inherit (infraPkgs)
            tfInit
            tfPlan
            tfApply
            tfDestroy
            tfEditVars
            ;
          inherit (deployWrappers) deployNixos deployService deployAll;
          inherit bootstrap-nixos remote tf-rekey;

          st0x-oracle-server = rust.package;
          default = rust.package;

          tf-init = infraPkgs.tfInit;
          tf-plan = infraPkgs.tfPlan;
          tf-apply = infraPkgs.tfApply;
          tf-destroy = infraPkgs.tfDestroy;
          tf-edit-vars = infraPkgs.tfEditVars;
          deploy-nixos = deployWrappers.deployNixos;
          deploy-service = deployWrappers.deployService;
          deploy-all = deployWrappers.deployAll;
        };

        devShells.default = pkgs.mkShell {
          inputsFrom = [ rainix.devShells.${system}.rust-shell ];
          packages = [
            oracle-rs-test
            oracle-rs-static
            infraPkgs.tfInit
            infraPkgs.tfPlan
            infraPkgs.tfApply
            infraPkgs.tfDestroy
            infraPkgs.tfEditVars
            tf-rekey
            deployWrappers.deployNixos
            deployWrappers.deployService
            deployWrappers.deployAll
            bootstrap-nixos
            remote
          ];
        };
      }
    );
}
