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
        # Bun/TypeScript door library, imported by door-*.ts job scripts.
        # Plain source copy: bun runs .ts directly, nothing to build.
        door-lib = pkgs.runCommand "vaultr-door" { } ''
          mkdir -p $out/lib
          cp -r ${self}/ts/vaultr-door $out/lib/vaultr-door
        '';
      in {
        packages = {
          default = workspace;
          vaultr = workspace;
          plant = workspace;
          inherit door-lib;
        };
        apps = {
          vaultr = { type = "app"; program = "${workspace}/bin/vaultr"; };
          plant = { type = "app"; program = "${workspace}/bin/plant"; };
        };
      });
}
