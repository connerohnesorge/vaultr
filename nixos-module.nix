{self}: {
  config,
  lib,
  pkgs,
  ...
}: let
  cfg = config.services.vaultr;
  home = config.users.users.${cfg.user}.home or "/home/${cfg.user}";
  caPath = "${home}/.local/state/plant/ca.pem";
  caBundlePath = "${home}/.local/state/plant/ca-bundle.crt";
  # Routing lives in managed settings, not this wrapper: background agents
  # (`claude agents`, `--bg`, `/background`) run under a shared supervisor that
  # never sees a PATH wrapper's exports. The wrapper only refuses to start
  # uncaptured.
  claude = pkgs.writeShellScriptBin "claude" ''
    if [ ! -f "${caPath}" ] || [ ! -f "${caBundlePath}" ]; then
      echo "plant CA missing at ${caPath} — is the plant service running?" >&2
      exit 1
    fi
    # These MUST be real environment variables, not settings.json `env`: the
    # runtime initialises its TLS trust store at process start, before any
    # settings file is read, so a value delivered through settings arrives too
    # late to be trusted. Verified — with SSL_CERT_FILE supplied only through
    # managed settings, Remote Control eligibility fails; exported here, it
    # passes. Everything else can safely live in managed settings.
    export SSL_CERT_FILE="${caBundlePath}"
    export NIX_SSL_CERT_FILE="${caBundlePath}"
    export NODE_EXTRA_CA_CERTS="${caPath}"
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
        # Blank, not absent. The user's stowed dotfiles ship a workstation
        # ~/.claude/settings.json that sets ANTHROPIC_BASE_URL to the old
        # reverse-proxy port, and settings outrank shell environment — so the
        # wrapper cannot clear it and the Remote Control host check fails on a
        # VM that stows dotfiles. Managed scope wins over user scope, and the
        # check treats an empty value as unset.
        ANTHROPIC_BASE_URL = "";
        # Also exported by the wrapper, which is what actually makes the TLS
        # trust take effect (see the comment there). Repeated here so
        # background agents, which the supervisor starts outside the wrapper,
        # still see consistent values.
        NODE_EXTRA_CA_CERTS = caPath;
        SSL_CERT_FILE = caBundlePath;
        NIX_SSL_CERT_FILE = caBundlePath;
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
        # Exact store path rather than /etc/ssl/certs/*, so the merged bundle
        # plant writes can never be built from a missing or partial file.
        PLANT_SYSTEM_CA_BUNDLE = "${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt";
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
