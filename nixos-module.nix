{self}: {
  config,
  lib,
  pkgs,
  ...
}: let
  cfg = config.services.vaultr;
  home = config.users.users.${cfg.user}.home or "/home/${cfg.user}";
  claude = pkgs.writeShellScriptBin "claude" ''
    export ANTHROPIC_BASE_URL="http://127.0.0.1:18923"
    exec ${lib.getExe' cfg.claudePackage "claude"} "$@"
  '';
  codex = pkgs.writeShellScriptBin "codex" ''
    exec ${lib.getExe' cfg.codexPackage "codex"} \
      -c 'openai_base_url="http://127.0.0.1:18924"' "$@"
  '';
in {
  options.services.vaultr = {
    enable = lib.mkEnableOption "Vaultr session capture";

    user = lib.mkOption {
      type = lib.types.str;
      example = "dev";
      description = "User whose sessions and Vault Plant captures.";
    };

    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
      defaultText = lib.literalExpression "inputs.vaultr.packages.\${pkgs.stdenv.hostPlatform.system}.default";
      description = "Vaultr package containing the vaultr and plant binaries.";
    };

    claudePackage = lib.mkOption {
      type = lib.types.package;
      description = "Claude Code package wrapped with Plant routing.";
    };

    codexPackage = lib.mkOption {
      type = lib.types.package;
      description = "Codex package wrapped with Plant routing.";
    };
  };

  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion = builtins.hasAttr cfg.user config.users.users;
        message = "services.vaultr.user must name a configured NixOS user";
      }
    ];

    environment.systemPackages = [
      cfg.package
      pkgs.zstd
      claude
      codex
    ];

    systemd.services.plant = {
      description = "Plant Claude Code and Codex session capture";
      wantedBy = ["multi-user.target"];
      wants = ["network-online.target"];
      after = ["network-online.target"];
      unitConfig.StartLimitIntervalSec = 0;
      environment = {
        HOME = home;
        PATH = "/run/current-system/sw/bin:/run/wrappers/bin";
      };
      serviceConfig = {
        User = cfg.user;
        WorkingDirectory = home;
        ExecStart = "${lib.getExe' cfg.package "plant"}";
        Restart = "always";
        RestartSec = "5s";
        TimeoutStopSec = "40s";
      };
    };
  };
}
