{
  description = "DOOM arcade cabinet: kiosk NixOS system, supervisor, attract UI, leaderboard";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    comin = {
      url = "github:nlewo/comin";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    impermanence.url = "github:nix-community/impermanence";
  };

  outputs = { self, nixpkgs, comin, impermanence }:
    let
      system = "x86_64-linux";
      pkgs = nixpkgs.legacyPackages.${system};
      # Libraries winit/glow dlopen at runtime (eframe glow backend).
      guiLibs = with pkgs; [ wayland libxkbcommon libGL ];
    in
    {
      devShells.${system}.default = pkgs.mkShell {
        packages = with pkgs; [
          cargo
          rustc
          clippy
          rustfmt
          rust-analyzer
          pkg-config
          sqlite
          zip
        ];
        buildInputs = guiLibs ++ [ pkgs.fontconfig ];
        LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath guiLibs;
      };

      # Packages, NixOS module, kiosk hosts, and apps are populated in nix/
      # as components land — see docs/SPEC.md §3 for the target layout.
    };
}
