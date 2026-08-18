# Kiosk layer — SPEC §9 as the base, plus: only tty1, PipeWire with a capped
# default volume, and a default-closed firewall with SSH allowed.
#
# Deviation from the SPEC's verbatim snippet: services.logind.extraConfig was
# removed from nixpkgs (nixos-unstable); the equivalent settings now live in
# services.logind.settings.Login.
{
  config,
  lib,
  pkgs,
  ...
}:

{
  services.getty.autologinUser = "doom";
  users.users.doom = {
    isNormalUser = true;
    extraGroups = [
      "video"
      "input"
    ];
  };

  services.cage = {
    enable = true;
    user = "doom";
    program = lib.mkDefault "${config.services.doom-arcade.package}/bin/arcade-supervisor";
  };
  systemd.services."cage-tty1" = {
    serviceConfig = {
      Restart = "always";
      RestartSec = 2;
    };
    # The upstream cage module ships restartIfChanged = false, which makes
    # comin self-update (SPEC §9) silently inert for the kiosk payload: a
    # switched-in generation would leave the old supervisor/attract/
    # gzdoom/pk3 store paths running until a power cycle. Force restarts on
    # unit change — the cabinet is an unattended appliance, so a deploy
    # interrupting an in-progress game is the accepted trade-off.
    restartIfChanged = lib.mkForce true;
  };

  boot.loader.timeout = 0;
  boot.kernelParams = [
    "quiet"
    "loglevel=0"
    "vt.global_cursor_default=0"
    "systemd.show_status=0"
  ];
  boot.plymouth.enable = true;

  services.logind.settings.Login = {
    HandlePowerKey = "ignore";
    HandleSuspendKey = "ignore";
    HandleLidSwitch = "ignore";
    # Only tty1, part 1: logind normally spawns autovt@ttyN gettys on VT
    # switch and reserves VT6. Zero both so no other VT ever gets a getty.
    NAutoVTs = 0;
    ReserveVT = 0;
  };
  # Only tty1, part 2: mask the autovt template outright so nothing can pull
  # a getty onto another VT. cage-tty1 conflicts getty@tty1 and owns tty1;
  # getty.autologinUser above is a fallback if cage ever fails permanently.
  systemd.services."autovt@".enable = false;

  # Neutralize ctrl-alt-del. `systemd.targets."ctrl-alt-del".enable = false`
  # cannot work on NixOS: systemd-lib always symlinks ctrl-alt-del.target ->
  # systemd.ctrlAltDelUnit (non-force ln), so the masked unit collides at
  # build time. Point the salute at an inert target instead.
  systemd.targets."arcade-cad-ignore" = {
    description = "Ignored ctrl-alt-del (kiosk)";
  };
  systemd.ctrlAltDelUnit = "arcade-cad-ignore.target";

  # No way to escape to a shell from the cabinet itself; SSH with keys only.
  services.openssh = {
    enable = true;
    settings = {
      PasswordAuthentication = false;
      KbdInteractiveAuthentication = false;
    };
  };

  # Firewall default-closed; the openssh module opens port 22 itself
  # (services.openssh.openFirewall defaults to true).
  networking.firewall.enable = true;

  # Audio: PipeWire, no mixer UI anywhere. Cap the default sink volume so the
  # cabinet cannot be turned into a nuisance. Best-effort: this is the
  # WirePlumber 0.5 device-defaults setting applied when a device has no
  # stored state; state cannot accumulate on the cabinet (tmpfs home), so in
  # practice it applies on every boot.
  services.pipewire = {
    enable = true;
    alsa.enable = true;
    pulse.enable = true;
    wireplumber.extraConfig."60-doom-arcade-volume" = {
      "wireplumber.settings" = {
        "device.routes.default-sink-volume" = 0.55;
      };
    };
  };
  security.rtkit.enable = true;
}
