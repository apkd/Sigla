{
  description = "Sigla source navigation over MCP";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { nixpkgs, ... }: let
    system = "x86_64-linux";
    pkgs = nixpkgs.legacyPackages.${system};
  in {
    packages.${system} = rec {
      sigla = pkgs.callPackage ./nix/package.nix { };
      default = sigla;
    };
  };
}
