{ self }:
{ config, lib, pkgs, ... }:
let
  cfg = config.programs.aswap;
  inherit (lib) mkEnableOption mkOption types mkIf;
  providerOptions = { ... }: {
    options = {
      enable = mkEnableOption "the aswap account-selection monitor";
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
      ExecStartPre = [
        "${cfg.package}/bin/aswap auto ${provider} threshold ${toString settings.threshold}"
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
    waybar.enable = mkEnableOption "the custom/aswap Waybar module in mainBar";
  };
  config = mkIf cfg.enable {
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
