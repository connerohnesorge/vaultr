# vaultr

Stay on top of life and work with Vault session utilities and Plant capture.

## NixOS

The flake exports `nixosModules.default`. Enable it for one user and supply the
CLI packages that its routed `claude` and `codex` wrappers execute:

```nix
{
  imports = [inputs.vaultr.nixosModules.default];
  services.vaultr = {
    enable = true;
    user = "dev";
    claudePackage = inputs.llm-agents.packages.${pkgs.system}.claude-code;
    codexPackage = unstablePkgs.codex;
  };
}
```

The module runs Plant as a system service on `127.0.0.1:18923` and
`127.0.0.1:18924`.

## Native session forks

Fork a captured session into Claude Code, Codex, or Pi:

```bash
vaultr session fork <session-id> --into pi
vaultr session fork <session-id> --into claude --read-only --prompt "review this"
vaultr session fork <session-id> --into codex --no-launch
```

Vaultr reads only the Session Capture. It writes a fresh native target session
and launches it in the captured working directory unless `--cwd` overrides it.
