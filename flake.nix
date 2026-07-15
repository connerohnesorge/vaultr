{
  description = "vaultr — vault session tooling + plant wireproxy";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
  inputs.flake-utils.url = "github:numtide/flake-utils";

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
        workspace = pkgs.rustPlatform.buildRustPackage {
          pname = "vaultr";
          version = "0.1.0";
          src = self;
          cargoLock.lockFile = ./Cargo.lock;
          # both workspace binaries (vaultr + plant) install into $out/bin
          doCheck = false;
        };
      in {
        packages = {
          default = workspace;
          vaultr = workspace;
          plant = workspace;
        };
        apps = {
          vaultr = { type = "app"; program = "${workspace}/bin/vaultr"; };
          plant = { type = "app"; program = "${workspace}/bin/plant"; };
        };
      });
}
