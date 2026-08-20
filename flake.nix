{
  description = "vaultr — vault session tooling + plant wireproxy";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
  inputs.flake-utils.url = "github:numtide/flake-utils";

  outputs = {
    self,
    nixpkgs,
    flake-utils,
  }: let
    nixosModule = import ./nixos-module.nix {inherit self;};
  in
    {
      nixosModules.default = nixosModule;
    }
    // flake-utils.lib.eachDefaultSystem (system:
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
        linuxPkgs = nixpkgs.legacyPackages.x86_64-linux;
        fakeClaude = linuxPkgs.writeShellScriptBin "claude" "exit 0";
        fakeCodex = linuxPkgs.writeShellScriptBin "codex" "exit 0";
        moduleSystem = nixpkgs.lib.nixosSystem {
          system = "x86_64-linux";
          modules = [
            nixosModule
            {
              boot.isContainer = true;
              system.stateVersion = "25.11";
              users.users.vaultr-test = {
                isNormalUser = true;
                home = "/home/vaultr-test";
              };
              services.vaultr = {
                enable = true;
                user = "vaultr-test";
                claudePackage = fakeClaude;
                codexPackage = fakeCodex;
              };
            }
          ];
        };
        modulePackages = moduleSystem.config.environment.systemPackages;
        plantUnit = moduleSystem.config.systemd.units."plant.service".text;
        claudeWrapper =
          nixpkgs.lib.findFirst
          (package: nixpkgs.lib.getName package == "claude")
          null
          modulePackages;
        codexWrapper =
          nixpkgs.lib.findFirst
          (package: nixpkgs.lib.getName package == "codex")
          null
          modulePackages;
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
        # Without this, `nix develop` falls back to the default package's build
        # environment, which carries cargo and rustc but not clippy or rustfmt —
        # so the two lint gates CI runs could not be reproduced locally at all.
        # inputsFrom keeps that environment (cc, linker, native deps) verbatim
        # and only adds the missing tools, at the same nixpkgs revision as the
        # compiler so clippy-driver and rustc never disagree on metadata.
        devShells.default = pkgs.mkShell {
          inputsFrom = [workspace];
          packages = [pkgs.clippy pkgs.rustfmt];
        };
        checks = {
          nixos-module =
            assert moduleSystem.config.systemd.services.plant.serviceConfig.User == "vaultr-test";
            assert moduleSystem.config.systemd.services.plant.serviceConfig.TimeoutStopSec == "40s";
            assert plantUnit != "";
            assert claudeWrapper != null;
            assert codexWrapper != null;
            pkgs.runCommand "vaultr-nixos-module-check" {} ''
              touch "$out"
            '';
        };
      });
}
