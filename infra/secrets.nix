{
  "terraform.tfstate.age".publicKeys = (import ../keys.nix).roles.ssh;
  "terraform.tfvars.age".publicKeys = (import ../keys.nix).roles.infra;
}
