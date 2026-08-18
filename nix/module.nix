# services.doom-arcade — the cabinet's service layer.
#
# Owns: the doom user, /var/lib/doom-arcade, IWAD preflight verification,
# the ARCADE_* environment handed to arcade-supervisor (via the cage-tty1
# unit), and the optional local leaderboard service.
#
# The kiosk itself (cage, autologin, boot silencing) lives in kiosk.nix;
# this module only *feeds* cage-tty1 when it exists.
{
  config,
  lib,
  pkgs,
  ...
}:

let
  cfg = config.services.doom-arcade;

  iwadPath = "/var/lib/doom-arcade/iwad/doom2.wad";
  fallbackWad = "${pkgs.freedoom}/share/games/doom/freedoom2.wad";

  leaderboardPort =
    let
      parts = lib.splitString ":" cfg.leaderboard.listen;
    in
    lib.toInt (lib.last parts);

  preflightScript = ''
    set -u

    iwad=${lib.escapeShellArg iwadPath}
    fallback=${fallbackWad}
    expected=${lib.escapeShellArg (if cfg.iwadSha256 == null then "" else cfg.iwadSha256)}

    use="$fallback"
    unverified=1

    if [ -r "$iwad" ]; then
      actual=$(sha256sum "$iwad" | cut -c1-64)
      if [ -n "$expected" ]; then
        if [ "$actual" = "$expected" ]; then
          use="$iwad"
          unverified=0
        else
          echo "doom-arcade-preflight: !!! IWAD HASH MISMATCH !!!" >&2
          echo "doom-arcade-preflight: expected $expected" >&2
          echo "doom-arcade-preflight: got      $actual" >&2
          echo "doom-arcade-preflight: falling back to Freedoom ($fallback)" >&2
        fi
      else
        # services.doom-arcade.iwadSha256 is null: no pin, run the provided
        # IWAD but always flag it so the attract screen shows the banner.
        echo "doom-arcade-preflight: no iwadSha256 pinned; using $iwad UNVERIFIED" >&2
        echo "doom-arcade-preflight: its sha256 is $actual — pin it as services.doom-arcade.iwadSha256" >&2
        use="$iwad"
      fi
    else
      echo "doom-arcade-preflight: $iwad absent; falling back to Freedoom ($fallback)" >&2
    fi

    # Record the hash of whatever we actually run (scores are scoped by it).
    actual_use=$(sha256sum "$use" | cut -c1-64)

    mkdir -p /run/doom-arcade
    {
      echo "ARCADE_IWAD=$use"
      echo "ARCADE_IWAD_UNVERIFIED=$unverified"
      echo "ARCADE_IWAD_SHA256=$actual_use"
    } > /run/doom-arcade/env.new
    chmod 0644 /run/doom-arcade/env.new
    mv /run/doom-arcade/env.new /run/doom-arcade/env
    echo "doom-arcade-preflight: ARCADE_IWAD=$use (unverified=$unverified, sha256=$actual_use)"
  '';
in
{
  options.services.doom-arcade = {
    enable = lib.mkEnableOption "the DOOM arcade cabinet service layer";

    package = lib.mkOption {
      type = lib.types.package;
      default = pkgs.doom-arcade;
      defaultText = lib.literalExpression "pkgs.doom-arcade";
      description = "The doom-arcade package (supervisor, attract, leaderboard binaries).";
    };

    pk3Package = lib.mkOption {
      type = lib.types.package;
      default = pkgs.arcade-telemetry-pk3;
      defaultText = lib.literalExpression "pkgs.arcade-telemetry-pk3";
      description = "Package providing share/arcade-telemetry.pk3.";
    };

    iwadSha256 = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      description = ''
        SHA-256 (lowercase hex) of the specific doom2.wad this cabinet runs.
        Compute it from the copy you provision (`sha256sum doom2.wad`) — do
        not copy a hash from an external list; doom2.wad variants differ.
        When null the cabinet still boots, but the IWAD is always treated as
        unverified and the attract screen shows the UNVERIFIED IWAD banner.
      '';
    };

    cabinetId = lib.mkOption {
      type = lib.types.str;
      default = "cab-1";
      description = "Identifier recorded on every score submission.";
    };

    dev = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = "Run windowed 1280x720 (sets ARCADE_DEV=1 for the supervisor).";
    };

    leaderboard = {
      enable = lib.mkEnableOption "running the leaderboard service locally on this machine";

      listen = lib.mkOption {
        type = lib.types.str;
        default = "127.0.0.1:8080";
        description = "host:port the local leaderboard service listens on.";
      };

      url = lib.mkOption {
        type = lib.types.str;
        default = "http://${cfg.leaderboard.listen}";
        defaultText = lib.literalExpression ''"http://''${services.doom-arcade.leaderboard.listen}"'';
        description = ''
          Leaderboard base URL the supervisor and attract screen talk to.
          Defaults to the local service; point it elsewhere when the
          leaderboard runs off-cabinet.
        '';
      };

      tokenFile = lib.mkOption {
        type = lib.types.nullOr lib.types.path;
        default = null;
        description = ''
          File containing the bearer token used to authenticate POST /v1/runs.
          Passed to the local leaderboard service (--token-file) and exported
          to the supervisor (ARCADE_TOKEN_FILE). Null in dev means open POST.
        '';
      };

      openFirewall = lib.mkOption {
        type = lib.types.bool;
        default = false;
        description = "Open the leaderboard port in the firewall.";
      };
    };
  };

  config = lib.mkIf cfg.enable {
    assertions =
      let
        listenHost = lib.head (lib.splitString ":" cfg.leaderboard.listen);
        firewallOpen = cfg.leaderboard.enable && cfg.leaderboard.openFirewall;
      in
      [
        {
          # SPEC §7.3: POST /v1/runs carries a shared bearer token. Without
          # one, opening the firewall exposes an unauthenticated write
          # endpoint: any LAN host could POST forged runs.
          assertion = !firewallOpen || cfg.leaderboard.tokenFile != null;
          message = ''
            services.doom-arcade.leaderboard.openFirewall = true requires
            leaderboard.tokenFile — otherwise POST /v1/runs would accept
            unauthenticated score submissions from the whole LAN.
          '';
        }
        {
          # openFirewall punches the port, but the service still binds the
          # listen address verbatim; on loopback the hole is useless and the
          # failure looks like a firewall problem.
          assertion = !firewallOpen || !(lib.elem listenHost [ "127.0.0.1" "localhost" "::1" "[::1]" ]);
          message = ''
            services.doom-arcade.leaderboard.openFirewall = true opens port
            ${toString leaderboardPort}, but leaderboard.listen
            ("${cfg.leaderboard.listen}") binds loopback, so LAN connections
            would be refused anyway. Set leaderboard.listen to a
            non-loopback address (e.g. "0.0.0.0:8080").
          '';
        }
      ];

    users.users.doom = {
      isNormalUser = true;
      group = "doom";
      extraGroups = [
        "video"
        "input"
      ];
      description = "DOOM arcade cabinet user";
    };
    users.groups.doom = { };

    systemd.tmpfiles.rules = [
      "d /var/lib/doom-arcade 0755 doom doom -"
      "d /var/lib/doom-arcade/iwad 0755 doom doom -"
      # The supervisor's runtime dir (per-run config/session state, event fifo).
      "d /run/doom-arcade 0755 doom doom -"
    ];

    # Verifies the IWAD before the kiosk starts and publishes the result as
    # an environment file consumed by cage-tty1.
    systemd.services.doom-arcade-preflight = {
      description = "DOOM arcade IWAD preflight verification";
      before = [ "cage-tty1.service" ];
      requiredBy = [ "cage-tty1.service" ];
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
      };
      script = preflightScript;
    };

    # Static environment for arcade-supervisor. cage passes its environment
    # down to the program it launches.
    services.cage.environment = {
      ARCADE_GZDOOM = "${pkgs.gzdoom}/bin/gzdoom";
      ARCADE_PK3 = "${cfg.pk3Package}/share/arcade-telemetry.pk3";
      ARCADE_CONFIG_TEMPLATE = "${cfg.package}/share/doom-arcade/gzdoom.ini";
      ARCADE_RUNTIME_DIR = "/run/doom-arcade";
      ARCADE_SPOOL_DB = "/var/lib/doom-arcade/spool.sqlite";
      ARCADE_LEADERBOARD_URL = cfg.leaderboard.url;
      ARCADE_CABINET_ID = cfg.cabinetId;
      ARCADE_ATTRACT_BIN = "${cfg.package}/bin/arcade-attract";
    }
    // lib.optionalAttrs (cfg.leaderboard.tokenFile != null) {
      ARCADE_TOKEN_FILE = toString cfg.leaderboard.tokenFile;
    }
    // lib.optionalAttrs cfg.dev { ARCADE_DEV = "1"; };

    # Dynamic (preflight-computed) environment: ARCADE_IWAD,
    # ARCADE_IWAD_UNVERIFIED, ARCADE_IWAD_SHA256.
    systemd.services."cage-tty1" = lib.mkIf config.services.cage.enable {
      serviceConfig.EnvironmentFile = "/run/doom-arcade/env";
    };

    systemd.services.arcade-leaderboard = lib.mkIf cfg.leaderboard.enable {
      description = "DOOM arcade leaderboard service";
      wantedBy = [ "multi-user.target" ];
      after = [ "network.target" ];
      serviceConfig = {
        ExecStart = lib.concatStringsSep " " (
          [
            "${cfg.package}/bin/arcade-leaderboard"
            "--listen"
            cfg.leaderboard.listen
            "--db"
            "/var/lib/doom-arcade/leaderboard.sqlite"
          ]
          ++ lib.optionals (cfg.leaderboard.tokenFile != null) [
            "--token-file"
            (toString cfg.leaderboard.tokenFile)
          ]
        );
        User = "doom";
        Group = "doom";
        Restart = "always";
        RestartSec = 2;
        # StateDirectory both ensures ownership and punches the write hole
        # through ProtectSystem=strict. The DB deliberately lives under
        # /var/lib/doom-arcade so the cabinet's persistence list covers it.
        StateDirectory = "doom-arcade";
        ProtectSystem = "strict";
        ProtectHome = true;
        PrivateTmp = true;
        NoNewPrivileges = true;
      };
    };

    networking.firewall.allowedTCPPorts = lib.optional (
      cfg.leaderboard.enable && cfg.leaderboard.openFirewall
    ) leaderboardPort;
  };
}
