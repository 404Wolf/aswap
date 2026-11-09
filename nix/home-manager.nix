{ self }:
{ config, lib, pkgs, ... }:
let
  cfg = config.programs.aswap;
  inherit (lib) mkEnableOption mkOption types mkIf;
  # Same quoting rule as nixpkgs' systemd Exec argument helper: JSON quotes,
  # then escape systemd specifiers and environment expansion (no shell involved).
  escapeExecArg = arg: lib.replaceStrings [ "%" "$" ] [ "%%" "$$" ] (builtins.toJSON arg);
  providerOptions = { ... }: {
    options = {
      enable = mkEnableOption "the aswap account-selection monitor";
      resumePrompt = mkOption {
        type = types.nullOr types.str;
        default = "You were interrupted by a usage-limit account switch. Continue exactly where you left off.";
        description = "Follow-up prompt after a forced run --auto account switch; null disables it.";
      };
      threshold = mkOption {
        type = types.addCheck types.number (n: n >= 1 && n <= 100);
        default = 90;
        description = "Switch when any quota window reaches this percentage.";
      };
      prefer = mkOption {
        type = types.enum [ "headroom" "weekly-reset" ];
        default = "headroom";
        description = "Rank eligible accounts by most headroom, or by the soonest weekly reset (new launches also move to an account whose weekly quota resets sooner).";
      };
    };
  };
  # Writes an account's access token into another local user's Claude
  # credentials, replacing only claudeAiOauth: that user's Claude Code keeps
  # its MCP OAuth logins (mcpOAuth) in the same file, and copying the whole
  # file over them logged it out of every MCP server on each run.
  shareMerge = pkgs.writeText "aswap-share-merge.py" ''
    import json, sys

    source, current = sys.argv[1], sys.stdin.read()
    with open(source) as f:
        oauth = json.load(f)["claudeAiOauth"]
    # aswap stays the only refresher, so the refresh token never leaves it.
    oauth.pop("refreshToken", None)
    oauth.pop("refreshTokenExpiresAt", None)
    try:
        credentials = json.loads(current) if current.strip() else {}
    except ValueError:
        credentials = {}
    if not isinstance(credentials, dict):
        credentials = {}
    if credentials.get("claudeAiOauth") == oauth:
        sys.exit(0)
    credentials["claudeAiOauth"] = oauth
    print(json.dumps(credentials))
  '';
  shareScript = user: share: pkgs.writeShellScript "aswap-share-${user}" ''
    set -euo pipefail
    # sudo is setuid on NixOS, so it comes from the wrappers, not the store.
    export PATH=/run/wrappers/bin:$PATH
    source=$HOME/.local/share/aswap/profiles/claude/${share.account}/.credentials.json
    target=${lib.escapeShellArg share.credentials}
    current=$(sudo -n cat "$target" 2>/dev/null || true)
    merged=$(printf '%s' "$current" | ${pkgs.python3}/bin/python3 -I ${shareMerge} "$source")
    if [ -n "$merged" ]; then
      printf '%s' "$merged" \
        | sudo -n install -o ${lib.escapeShellArg user} -g ${lib.escapeShellArg share.group} -m 600 /dev/stdin "$target"
      echo "updated ${user} credentials"
    fi
  '';
  shareOptions = { name, ... }: {
    options = {
      account = mkOption {
        type = types.str;
        description = "aswap Claude account whose access token this user gets.";
      };
      credentials = mkOption {
        type = types.str;
        default = "/home/${name}/.claude/.credentials.json";
        description = "The user's Claude Code credentials file.";
      };
      group = mkOption {
        type = types.str;
        default = name;
        description = "Group that owns the credentials file.";
      };
    };
  };
  serviceFor = provider: settings: {
    Unit = {
      Description = "aswap ${provider} account-selection monitor";
      After = [ "network-online.target" ];
    };
    Service = {
      Type = "simple";
      UMask = "0077";
      ExecStartPre = (lib.optional (cfg.refreshOwner == true) "${cfg.package}/bin/aswap sync --owner")
        ++ (lib.optional (cfg.refreshOwner == false && cfg.syncHost == null) "${cfg.package}/bin/aswap sync --off")
        ++ (lib.optional (cfg.syncHost != null) "${cfg.package}/bin/aswap sync -- ${escapeExecArg cfg.syncHost}")
        ++ [
        "${cfg.package}/bin/aswap auto ${provider} threshold ${toString settings.threshold}"
        "${cfg.package}/bin/aswap auto ${provider} resume-prompt -- ${escapeExecArg (if settings.resumePrompt == null then "off" else settings.resumePrompt)}"
        "${cfg.package}/bin/aswap auto ${provider} prefer ${settings.prefer}"
        "${cfg.package}/bin/aswap auto ${provider} on"
      ];
      ExecStart = "${cfg.package}/bin/aswap auto ${provider} watch";
      Restart = "on-failure";
      RestartSec = 60;
    };
    Install.WantedBy = [ "default.target" ];
  };
in {
  options.programs.aswap = {
    enable = mkEnableOption "aswap private account profiles";
    package = mkOption {
      type = types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
      description = "The aswap package to install.";
    };
    autoswitch = {
      claude = mkOption { type = types.submodule providerOptions; default = {}; };
      codex = mkOption { type = types.submodule providerOptions; default = {}; };
    };
    refreshOwner = mkOption {
      type = types.nullOr types.bool;
      default = null;
      description = "True enables this refresh owner; false disables refresh without syncHost; null preserves CLI configuration.";
    };
    syncHost = mkOption {
      type = types.nullOr types.str;
      default = null;
      description = "SSH host alias for the refresh/usage owner; followers receive access-only auth.";
    };
    shareClaudeAccess = mkOption {
      type = types.attrsOf (types.submodule shareOptions);
      default = {};
      example = { trader.account = "work"; };
      description = ''
        Local users (by name) that get an account's Claude access token, never
        its refresh token, re-copied every 10 minutes. Only claudeAiOauth in
        their credentials file changes; their MCP logins stay. Needs
        passwordless sudo for cat and install.
      '';
    };
    waybar.enable = mkEnableOption "the custom/aswap Waybar module in mainBar";
  };
  config = mkIf cfg.enable {
    assertions = [{ assertion = !(cfg.refreshOwner == true && cfg.syncHost != null); message = "aswap refreshOwner and syncHost are mutually exclusive"; }];
    home.packages = [ cfg.package ];
    systemd.user.services = lib.mapAttrs' (provider: settings:
      lib.nameValuePair "aswap-${provider}" (serviceFor provider settings)
    ) (lib.filterAttrs (_: settings: settings.enable) cfg.autoswitch)
    // lib.mapAttrs' (user: share: lib.nameValuePair "aswap-share-${user}" {
      Unit.Description = "Share the ${share.account} Claude access token with ${user}";
      Service = {
        Type = "oneshot";
        UMask = "0077";
        ExecStart = "${shareScript user share}";
      };
    }) cfg.shareClaudeAccess;
    systemd.user.timers = lib.mapAttrs' (user: share: lib.nameValuePair "aswap-share-${user}" {
      Unit.Description = "Re-share the ${share.account} Claude access token with ${user} every 10 minutes";
      Timer = {
        OnBootSec = "1min";
        OnUnitActiveSec = "10min";
      };
      Install.WantedBy = [ "timers.target" ];
    }) cfg.shareClaudeAccess;
    programs.waybar.settings = mkIf cfg.waybar.enable {
      mainBar."custom/aswap" = {
        exec = "${cfg.package}/bin/aswap waybar";
        return-type = "json";
        restart-interval = 30;
        escape = true;
        tooltip = true;
      };
    };
    # No home.file/xdg.configFile entries: provider homes are never managed here.
  };
}
