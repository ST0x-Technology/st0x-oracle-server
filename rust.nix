{ pkgs, craneLib }:

let
  commonArgs = {
    pname = "st0x-oracle-server";
    version = "0.1.0";
    src = ./.;

    nativeBuildInputs = [ pkgs.pkg-config pkgs.perl ];
    buildInputs = [ pkgs.openssl ]
      ++ pkgs.lib.optionals pkgs.stdenv.hostPlatform.isDarwin
      [ pkgs.apple-sdk_15 ];
  };

  cargoArtifacts = craneLib.buildDepsOnly commonArgs;

in {
  package = craneLib.buildPackage (commonArgs // {
    inherit cargoArtifacts;
    doCheck = true;

    meta = {
      description = "Signed context oracle server for st0x tokenized equities on Raindex";
      homepage = "https://github.com/ST0x-Technology/st0x-oracle-server";
    };
  });

  clippy = craneLib.cargoClippy (commonArgs // {
    inherit cargoArtifacts;
    cargoClippyExtraArgs = "--all-targets -- -D clippy::all";
  });
}
