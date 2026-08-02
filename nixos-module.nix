{self}: {
  config,
  lib,
  pkgs,
  ...
}: let
  cfg = config.services.vaultr;
  home = config.users.users.${cfg.user}.home or "/home/${cfg.user}";
  caPath = "${home}/.local/state/plant/ca.pem";
  # Routing lives in managed settings, not this wrapper: background agents
  # (`claude agents`, `--bg`, `/background`) run under a shared supervisor that
  # never sees a PATH wrapper's exports. The wrapper only refuses to start
  # uncaptured.
  claude = pkgs.writeShellScriptBin "claude" ''
    if [ ! -f "${caPath}" ]; then
      echo "plant CA missing at ${caPath} — is the plant service running?" >&2
      exit 1
    fi
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

    # Claude Code >= 2.1.196 disables Remote Control whenever ANTHROPIC_BASE_URL
    # names a host other than api.anthropic.com, and there is no override — so
    # capture routes via HTTPS_PROXY instead, leaving the base URL unset. Plant
    # answers CONNECT, intercepts api.anthropic.com with its own CA, and splices
    # every other host through untouched.
    #
    # remoteControlAtStartup only reads from a settings scope, and only the
    # policy scope can turn it *on* — a project or local settings file can still
    # set it false, so this is a default rather than a mandate.
    environment.etc."claude-code/managed-settings.json".text = builtins.toJSON {
      remoteControlAtStartup = true;
      env = {
        HTTP_PROXY = "http://127.0.0.1:18923";
        HTTPS_PROXY = "http://127.0.0.1:18923";
        NO_PROXY = "localhost,127.0.0.1,::1,.lan.cnb.rocks,.svc,.cluster.local";
        NODE_EXTRA_CA_CERTS = caPath;
      };
    };

    systemd.services.plant = {
      description = "Plant Claude Code and Codex session capture";
      wantedBy = ["multi-user.target"];
      wants = ["network-online.target"];
      after = ["network-online.target"];
      unitConfig.StartLimitIntervalSec = 0;
      environment = {
        HOME = home;
        PATH = lib.mkForce "/run/current-system/sw/bin:/run/wrappers/bin";
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
