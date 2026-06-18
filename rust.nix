{ pkgs, craneLib }:

let
  commonArgs = {
    pname = "st0x-oracle-server";
    version = "0.1.0";
    src = ./.;

    nativeBuildInputs = [
      pkgs.pkg-config
      pkgs.perl
    ];
    buildInputs = [
      pkgs.openssl
    ]
    ++ pkgs.lib.optionals pkgs.stdenv.hostPlatform.isDarwin [ pkgs.apple-sdk_15 ];
  };

  cargoArtifacts = craneLib.buildDepsOnly commonArgs;

in
{
  # `nix build .#st0x-oracle-server` produces the release binary that the
  # OCI image wraps for Cloud Run. `doCheck = false` to avoid running the
  # full test suite at build time — CI already gated it. Mirrors the
  # bebop / pricing rust.nix.
  package = craneLib.buildPackage (
    commonArgs
    // {
      inherit cargoArtifacts;
      doCheck = false;

      meta = {
        description = "Signed context oracle server for st0x tokenized equities on Raindex";
        homepage = "https://github.com/ST0x-Technology/st0x-oracle-server";
      };
    }
  );

  clippy = craneLib.cargoClippy (
    commonArgs
    // {
      inherit cargoArtifacts;
      cargoClippyExtraArgs = "--all-targets -- -D clippy::all";
    }
  );
}
