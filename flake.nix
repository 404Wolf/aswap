{
  description = "aswap: private account profiles for Claude Code and Codex CLI";
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  inputs.home-manager = {
    url = "github:nix-community/home-manager";
    inputs.nixpkgs.follows = "nixpkgs";
  };
  outputs = { self, nixpkgs, home-manager }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      eachSystem = nixpkgs.lib.genAttrs systems;
      pkgsFor = system: import nixpkgs { inherit system; };
      packageFor = system:
        let pkgs = pkgsFor system;
        in pkgs.rustPlatform.buildRustPackage {
          pname = "aswap";
          version = "0.1.0";
          src = pkgs.lib.cleanSource ./.;
          cargoLock.lockFile = ./Cargo.lock;
          nativeCheckInputs = [ pkgs.python3 ];
          preCheck = ''
            aswapTestRoot=$(mktemp -d)
            export HOME="$aswapTestRoot/home"
            export XDG_DATA_HOME="$aswapTestRoot/data"
            export XDG_CONFIG_HOME="$aswapTestRoot/config"
            export XDG_CACHE_HOME="$aswapTestRoot/cache"
            export CLAUDE_CONFIG_DIR="$aswapTestRoot/claude"
            export CODEX_HOME="$aswapTestRoot/codex"
            mkdir -p "$HOME" "$CLAUDE_CONFIG_DIR" "$CODEX_HOME"
            unset ANTHROPIC_API_KEY ANTHROPIC_AUTH_TOKEN CLAUDE_CODE_OAUTH_TOKEN OPENAI_API_KEY CODEX_API_KEY
          '';
          postCheck = ''rm -rf "$aswapTestRoot"'';
          meta = {
            description = "Private account profiles for Claude Code and Codex CLI";
            license = pkgs.lib.licenses.mit;
            mainProgram = "aswap";
            platforms = pkgs.lib.platforms.linux;
          };
        };
    in {
      packages = eachSystem (system: { default = packageFor system; aswap = packageFor system; });
      apps = eachSystem (system: {
        default = { type = "app"; program = "${self.packages.${system}.default}/bin/aswap"; meta.description = "Manage isolated provider accounts"; };
      });
      devShells = eachSystem (system: let pkgs = pkgsFor system; in {
        default = pkgs.mkShell {
          packages = with pkgs; [ cargo rustc rustfmt clippy gitleaks pkg-config python3 ];
          shellHook = ''
            git config --local core.hooksPath .githooks
            echo "aswap: secret-scanning commit/push hooks installed"
          '';
        };
      });
      homeManagerModules.default = import ./nix/home-manager.nix { inherit self; };
      checks = eachSystem (system:
        let
          pkgs = pkgsFor system;
          package = self.packages.${system}.default;
          hm = home-manager.lib.homeManagerConfiguration {
            inherit pkgs;
            modules = [
              self.homeManagerModules.default
              {
                home.username = "synthetic";
                home.homeDirectory = "/home/synthetic";
                home.stateVersion = "26.05";
                programs.waybar.enable = true;
                programs.waybar.settings.mainBar.modules-right = [ "custom/aswap" ];
                programs.aswap = {
                  enable = true;
                  autoswitch.claude.enable = true;
                  autoswitch.claude.resumePrompt = ''synthetic "quotes" $HOME %n'';
                  autoswitch.codex.enable = true;
                  autoswitch.codex.resumePrompt = null;
                  waybar.enable = true;
                };
              }
            ];
          };
        in {
          tests = package;
          clippy = package.overrideAttrs (old: {
            pname = "aswap-clippy";
            nativeBuildInputs = (old.nativeBuildInputs or []) ++ [ pkgs.clippy ];
            buildPhase = ''
              runHook preBuild
              cargo clippy --offline --locked --all-targets -- -D warnings
              runHook postBuild
            '';
            doCheck = false;
            installPhase = "touch $out";
          });
          fmt = pkgs.runCommand "aswap-fmt" { nativeBuildInputs = [ pkgs.cargo pkgs.rustfmt ]; } ''
            cd ${self}
            cargo fmt --all --check
            touch $out
          '';
          gitleaks = pkgs.runCommand "aswap-gitleaks" { nativeBuildInputs = [ pkgs.gitleaks ]; } ''
            cd ${self}
            gitleaks dir --redact --no-banner .
            touch $out
          '';
          home-manager = assert builtins.elem package hm.config.home.packages;
            assert hm.config.systemd.user.services.aswap-claude.Service.UMask == "0077";
            assert hm.config.systemd.user.services.aswap-codex.Service.RestartSec == 60;
            assert pkgs.lib.hasInfix " resume-prompt -- " (builtins.elemAt hm.config.systemd.user.services.aswap-claude.Service.ExecStartPre 1);
            assert pkgs.lib.hasInfix "$$HOME %%n" (builtins.elemAt hm.config.systemd.user.services.aswap-claude.Service.ExecStartPre 1);
            assert pkgs.lib.hasInfix "off" (builtins.elemAt hm.config.systemd.user.services.aswap-codex.Service.ExecStartPre 1);
            assert hm.config.programs.waybar.settings.mainBar."custom/aswap".return-type == "json";
            pkgs.runCommand "aswap-home-manager" {} "touch $out";
        });
    };
}
