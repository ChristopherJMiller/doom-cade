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

      overlay = import ./nix/pkgs/overlay.nix;

      doom-arcade = pkgs.callPackage ./nix/pkgs/doom-arcade.nix { };
      arcade-telemetry-pk3 = pkgs.callPackage ./nix/pkgs/telemetry-pk3.nix { };

      # `nix run .#dev` — the full loop, windowed, against Freedoom, with a
      # local ephemeral leaderboard. Works on a normal laptop (SPEC §12).
      devApp = pkgs.writeShellApplication {
        name = "arcade-dev";
        runtimeInputs = [ doom-arcade pkgs.gzdoom ];
        text = ''
          tmp=$(mktemp -d)
          lb_pid=""
          cleanup() {
            if [ -n "$lb_pid" ]; then kill "$lb_pid" 2>/dev/null || true; fi
            rm -rf "$tmp"
          }
          trap cleanup EXIT

          echo "arcade-dev: leaderboard on http://127.0.0.1:8080, scratch state in $tmp" >&2
          arcade-leaderboard --db "$tmp/lb.sqlite" --seed 12 --listen 127.0.0.1:8080 &
          lb_pid=$!

          mkdir -p "$tmp/run"
          export ARCADE_DEV=1
          export ARCADE_IWAD=${pkgs.freedoom}/share/games/doom/freedoom2.wad
          export ARCADE_PK3=${arcade-telemetry-pk3}/share/arcade-telemetry.pk3
          export ARCADE_CONFIG_TEMPLATE=${doom-arcade}/share/doom-arcade/gzdoom.ini
          export ARCADE_RUNTIME_DIR="$tmp/run"
          export ARCADE_SPOOL_DB="$tmp/spool.sqlite"
          export ARCADE_LEADERBOARD_URL=http://127.0.0.1:8080
          export ARCADE_IWAD_UNVERIFIED=1
          arcade-supervisor
        '';
      };

      # `nix run .#leaderboard` — the service alone, seeded, for UI iteration.
      leaderboardApp = pkgs.writeShellApplication {
        name = "arcade-leaderboard-dev";
        runtimeInputs = [ doom-arcade ];
        text = ''
          exec arcade-leaderboard --db :memory: --seed 20 "$@"
        '';
      };

      # `nix run .#vm` — boot the kiosk VM (nixosConfigurations.vm).
      vmApp = pkgs.writeShellApplication {
        name = "arcade-vm";
        text = ''
          exec ${self.nixosConfigurations.vm.config.system.build.vm}/bin/run-doom-cab-vm-vm "$@"
        '';
      };
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

      overlays.default = overlay;

      packages.${system} = {
        inherit doom-arcade arcade-telemetry-pk3;
        default = doom-arcade;
      };

      apps.${system} = {
        dev = {
          type = "app";
          program = "${devApp}/bin/arcade-dev";
        };
        leaderboard = {
          type = "app";
          program = "${leaderboardApp}/bin/arcade-leaderboard-dev";
        };
        vm = {
          type = "app";
          program = "${vmApp}/bin/arcade-vm";
        };
      };

      # Reuse the packages as checks; nothing here needs network access.
      checks.${system} = {
        inherit doom-arcade arcade-telemetry-pk3;
      };

      nixosModules.doom-arcade = {
        imports = [ ./nix/module.nix ];
        nixpkgs.overlays = [ overlay ];
      };
      nixosModules.default = self.nixosModules.doom-arcade;

      nixosConfigurations.cabinet = nixpkgs.lib.nixosSystem {
        inherit system;
        specialArgs = { inherit comin impermanence; };
        modules = [
          { nixpkgs.overlays = [ overlay ]; }
          ./nix/hosts/cabinet.nix
        ];
      };

      nixosConfigurations.vm = nixpkgs.lib.nixosSystem {
        inherit system;
        modules = [
          { nixpkgs.overlays = [ overlay ]; }
          ./nix/hosts/vm.nix
        ];
      };
    };
}
