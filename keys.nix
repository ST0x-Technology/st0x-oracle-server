rec {
  # Mirror of st0x.bebop / st0x.pricing keys.nix so the same SSH
  # identities can operate this droplet. Add a teammate here when they
  # need infra / SSH access; tf-rekey re-encrypts the .age files with
  # the new roles.
  #
  # `host` is a placeholder until the first `nix run .#bootstrap-nixos`.
  keys = {
    st0x-op = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIPZ56nOYbGDd0ZfbqxeY7AbvaQGQrHnlC80ccpRGpCoj";
    host = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIKsOOJ7HWyxx/i1PA0eVoavt0qGolFSijmkh8c9O2XVG";
    ci = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIPTd2zKSwHgWegi290EiK5nYp1Wp4+x2fDYqFxbd0WLN";
    arda = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIAyTREGZCOzMsl7N9dp1saN/t7DCs7YesusVUKApMJ78";
    alastair = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIJArH3PA+bFIon0JkCVQGs9aWr45lnVjiiTLLO9BPItn";
    josh = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIG7gguZKImawXMzz6Auqx+IEdvUEZ7hygjv27XWSOgri";
    jai = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIFF9dwudulfhtbdz/DDMJKZF9XMY+RTZ6lc82DbNEkfl";
  };

  roles = with keys; {
    # `infra` can decrypt Terraform vars (DO API token).
    infra = [
      st0x-op
      ci
      alastair
      josh
    ];

    # `ssh` are the operator keys that can SSH into the droplet and
    # decrypt Terraform state.
    ssh = [
      st0x-op
      ci
      arda
      alastair
      josh
      jai
    ];

    # `host-secrets` are recipients for runtime secrets that the host
    # itself needs to decrypt at boot (tailscale auth key, service
    # EnvironmentFile). Same as `ssh` plus the host's own key.
    #
    # WARNING: do not encrypt to this role until `host` has been pinned
    # by `bootstrap-nixos` — the PLACEHOLDER above is a malformed key
    # and rage will reject it. During the first deploy, initial-encrypt
    # to `ssh` only; after bootstrap pins the real host key, re-encrypt
    # to `host-secrets`. See DEPLOY.md.
    host-secrets = [
      st0x-op
      ci
      arda
      alastair
      josh
      jai
      host
    ];
  };
}
