{
  description = "Sigla source navigation over MCP";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { nixpkgs, ... }: let
    systems = [ "x86_64-linux" "aarch64-linux" ];
  in {
    packages = nixpkgs.lib.genAttrs systems (system: rec {
      sigla = nixpkgs.legacyPackages.${system}.callPackage ./nix/package.nix { };
      default = sigla;
    });
  };
}
