{
  pkgs,
  lib,
  modulesPath,
  tailscalePkg,
  ...
}:

let
  inherit (import ./keys.nix) roles;

  services = import ./services.nix;
  enabledServices = lib.filterAttrs (_: v: v.enabled) services;

  # mkService — systemd unit template (mirrors st0x.bebop / st0x.pricing).
  # Per-service nix profile + DynamicUser + Restart=always +
  # ReadWritePaths=/mnt/data + EnvironmentFile from ragenix. Secrets
  # (signer key + pricing/Alpaca creds) come from the env file.
  mkService =
    name: cfg:
    let
      path = "/nix/var/nix/profiles/per-service/${name}/bin/${cfg.bin}";
      configFile = ./config/${name}.toml;
    in
    {
      description = "st0x ${cfg.bin} (${name})";

      wantedBy = [ ];

      restartIfChanged = false;
      stopIfChanged = false;

      unitConfig = {
        "X-OnlyManualStart" = true;
        ConditionPathExists = "/run/st0x/${name}.ready";
      };

      serviceConfig = {
        DynamicUser = true;
        SupplementaryGroups = [ "st0x" ];
        ExecStart = "${path} --config ${configFile}";
        EnvironmentFile = "/run/agenix/${name}-env";
        Restart = "always";
        RestartSec = 5;
        ReadWritePaths = [ "/mnt/data" ];
      };
    };

in
{
  imports = [
    (modulesPath + "/virtualisation/digital-ocean-config.nix")
    (modulesPath + "/profiles/qemu-guest.nix")
    ./disko.nix
  ];

  boot.loader.grub = {
    efiSupport = true;
    efiInstallAsRemovable = true;
  };

  # Currently closed to public — the parity-window deploy mirrors bebop /
  # pricing exactly. Public ingress (so Raindex consumers can hit the
  # new oracle URL) will be added in the cutover PR after the diff
  # window proves parity. Until then, ops + the diff observer reach
  # this droplet over the tailnet only.
  networking = {
    useDHCP = lib.mkForce false;
    hostName = "st0x-oracle-server";
    firewall = {
      enable = true;
      # Bootstrap chicken-and-egg: secrets at install time are encrypted
      # to roles.ssh (operator keys). The new host's SSH key doesn't yet
      # exist on disk so agenix can't decrypt anything on first boot —
      # which means tailscale-authkey never gets read, tailscaled never
      # starts, and the droplet has no tailnet ingress. Leaving port 22
      # open publicly during bootstrap lets the operator SSH in over
      # the public IP to commit the new host key + push a tf-rekey'd
      # set of secrets. Close it (remove `22` from this list) in the
      # follow-up deploy once tailscale is up. See DEPLOY.md.
      allowedTCPPorts = [ 22 ];
      trustedInterfaces = [ "tailscale0" ];
    };
  };

  services = {
    cloud-init = {
      enable = true;
      network.enable = true;
      settings = {
        datasource_list = [
          "ConfigDrive"
          "Digitalocean"
        ];
        datasource.ConfigDrive = { };
        datasource.Digitalocean = { };
        cloud_init_modules = [
          "seed_random"
          "bootcmd"
          "write_files"
          "growpart"
          "resizefs"
          "set_hostname"
          "update_hostname"
          "set_password"
        ];
        cloud_config_modules = [
          "ssh-import-id"
          "keyboard"
          "runcmd"
          "disable_ec2_metadata"
        ];
        cloud_final_modules = [
          "write_files_deferred"
          "ssh_authkey_fingerprints"
          "keys_to_console"
          "phone_home"
          "final_message"
        ];
      };
    };

    openssh = {
      enable = true;
      # Public SSH closed. sshd still listens on all interfaces but
      # the public firewall doesn't expose port 22; the tailscale0
      # interface is trusted via networking.firewall.trustedInterfaces
      # so SSH over the tailnet keeps working. Break-glass is the DO
      # web console (VNC).
      openFirewall = false;
      settings = {
        PasswordAuthentication = false;
        PermitRootLogin = "prohibit-password";
      };
    };

    # Oracle dials st0x-pricing (post-RAI-360) + Alpaca's broker API
    # over the public internet (the calendar fetch). Tailscale carries
    # ops traffic; log shipping (Alloy → obs) goes via MagicDNS.
    tailscale = {
      enable = true;
      # Override the rainix-pinned tailscale (1.98.0) with a newer
      # build sourced from nixpkgs-tailscale-fix. 1.98.0/1.98.1 have
      # NixOS/nixpkgs#520715: tailscaled rewrites systemd-resolved to
      # point at Tailscale's public ts.net nameservers, breaking
      # MagicDNS for our own tailnet. Filed upstream as
      # rainlanguage/rainix#183 — remove this override (and the
      # `nixpkgs-tailscale-fix` input in flake.nix) once the rainix
      # nixpkgs pin moves past 1.98.2.
      package = tailscalePkg;
      authKeyFile = "/run/agenix/tailscale-authkey";
      openFirewall = true;
      extraUpFlags = [
        "--ssh"
        "--accept-routes=false"
        # Oracle dials st0x-pricing (WS) + st0x-obs (alloy → Loki) by
        # MagicDNS short name. Accept tailnet DNS or those resolutions
        # silently fail with "no such host".
        "--accept-dns=true"
        "--hostname=st0x-oracle-server"
      ];
    };

    logrotate = {
      enable = true;
      settings."/mnt/data/st0x-oracle-server/logs/*.log" = {
        su = "root st0x";
        rotate = 14;
        weekly = true;
        compress = true;
        missingok = true;
        notifempty = true;
      };
    };

    # Ship journald output to the obs droplet's Loki over Tailscale.
    # Grafana Alloy replaces deprecated promtail; config in
    # /etc/alloy/config.alloy below.
    alloy.enable = true;
  };

  # Alloy reads every `.alloy` file in /etc/alloy. Single file: scrape
  # journald, tag rows from the st0x-oracle-server unit, ship to Loki
  # on the obs droplet over MagicDNS.
  environment.etc."alloy/config.alloy".text = ''
    loki.relabel "journal" {
      forward_to = []

      rule {
        source_labels = ["__journal__systemd_unit"]
        target_label  = "unit"
      }

      // Tag rows from the oracle service so Grafana queries like
      // `{service="st0x-oracle-server"}` resolve cleanly.
      rule {
        source_labels = ["__journal__systemd_unit"]
        regex         = "st0x-oracle-server\\.service"
        target_label  = "service"
        replacement   = "st0x-oracle-server"
      }
    }

    loki.source.journal "journal" {
      forward_to    = [loki.write.obs.receiver]
      relabel_rules = loki.relabel.journal.rules
      labels        = {
        job  = "systemd-journal",
        host = "st0x-oracle-server",
      }
    }

    loki.write "obs" {
      endpoint {
        url = "http://st0x-obs:3100/loki/api/v1/push"
      }
    }
  '';

  users.users.root.openssh.authorizedKeys.keys = roles.ssh;

  fileSystems."/mnt/data" = {
    device = "/dev/disk/by-id/scsi-0DO_Volume_st0x-oracle-server-data";
    fsType = "ext4";
  };

  nix = {
    settings = {
      experimental-features = [
        "nix-command"
        "flakes"
      ];
      auto-optimise-store = true;
      download-buffer-size = 268435456;
      sandbox = false;
    };

    gc = {
      automatic = true;
      dates = "weekly";
      options = "--delete-older-than 30d";
    };
  };

  age.secrets = {
    "tailscale-authkey" = {
      file = ./secrets/tailscale-authkey.age;
      mode = "0400";
      owner = "root";
    };
    "st0x-oracle-server-env" = {
      file = ./secrets/st0x-oracle-server-env.age;
      mode = "0440";
      owner = "root";
      group = "st0x";
    };
    # Diff observer (RAI-361). No upstream creds — both oracle URLs it
    # probes are open — but the systemd unit's EnvironmentFile= still
    # wants a file. Single-line `RUST_LOG=…` blob; gets removed when
    # the observer service is disabled (services.nix).
    "st0x-oracle-diff-observer-env" = {
      file = ./secrets/st0x-oracle-diff-observer-env.age;
      mode = "0440";
      owner = "root";
      group = "st0x";
    };
  };

  users.groups.st0x = { };
  programs.bash.interactiveShellInit = "set -o vi";

  systemd.tmpfiles.rules = [
    "d /mnt/data/st0x-oracle-server 0775 root st0x -"
    "d /mnt/data/st0x-oracle-server/logs 0775 root st0x -"
  ];
  systemd.services = (lib.mapAttrs mkService enabledServices) // {
    # NixOS firewall doesn't restart on config change by default —
    # its reloadIfChanged path skips the iptables-restore that picks
    # up allowedTCPPorts edits. Force a restart so future deploys
    # actually roll the new ruleset.
    firewall.restartIfChanged = true;
    # cloud-final exits non-zero with status 2 on fresh DO droplets
    # when `phone_home` can't reach the metadata service. Tell
    # systemd to treat 2 as success so the unit doesn't enter
    # `failed` on first boot.
    cloud-final.serviceConfig.SuccessExitStatus = [ "2" ];
  };

  environment.systemPackages = with pkgs; [
    bat
    curl
    htop
    jq
    tailscale
    zellij
  ];

  system.activationScripts.per-service-profiles.text = "mkdir -p /nix/var/nix/profiles/per-service";

  system.stateVersion = "24.11";
}
