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
    waybar.enable = mkEnableOption "the custom/aswap Waybar module in mainBar";
  };
  config = mkIf cfg.enable {
    assertions = [{ assertion = !(cfg.refreshOwner == true && cfg.syncHost != null); message = "aswap refreshOwner and syncHost are mutually exclusive"; }];
    home.packages = [ cfg.package ];
    systemd.user.services = lib.mapAttrs' (provider: settings:
      lib.nameValuePair "aswap-${provider}" (serviceFor provider settings)
    ) (lib.filterAttrs (_: settings: settings.enable) cfg.autoswitch);
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
