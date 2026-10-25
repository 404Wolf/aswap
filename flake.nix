{
  description = "agentswap development and packaging";
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  outputs = { self, nixpkgs }: let
    systems = [ "x86_64-linux" "aarch64-linux" ];
    eachSystem = nixpkgs.lib.genAttrs systems;
  in {
    devShells = eachSystem (system: let pkgs = import nixpkgs { inherit system; }; in {
      default = pkgs.mkShell {
        packages = with pkgs; [ cargo rustc rustfmt clippy gitleaks pkg-config ];
        shellHook = ''
          git config --local core.hooksPath .githooks
          echo "agentswap: secret-scanning commit/push hooks installed"
        '';
      };
    });
  };
}
