# Local test VM (`nix run .#vm`) — boots straight into the kiosk for
# iterating on the attract/kiosk UX on a laptop.
#
# Deliberately imports only module + kiosk: no impermanence (the VM's disk is
# throwaway already) and no comin (self-update pulling at a dev VM would only
# fight local iteration).
{
  config,
  lib,
  pkgs,
  ...
}:

{
  imports = [
    ../module.nix
    ../kiosk.nix
    # Thumb-drive IWAD import, same as the cabinet: the VM is where the
    # import unit/CLI wiring gets exercised (qemu usb-storage can stand in
    # for a real stick).
    ../wad-import.nix
  ];

  networking.hostName = "doom-cab-vm";

  services.doom-arcade = {
    enable = true;
    # No pin: preflight falls back to Freedoom, which exercises the
    # UNVERIFIED IWAD banner path in the attract screen.
    iwadSha256 = null;
    cabinetId = "vm";
    leaderboard.enable = true;
  };

  # Base (non-VM) boot config so the plain toplevel also evaluates; the
  # qemu-vm variant below overrides the disk layout anyway.
  fileSystems."/" = {
    device = "/dev/disk/by-label/nixos";
    fsType = "ext4";
  };
  boot.loader.grub.device = "/dev/vda";

  virtualisation.vmVariant = {
    virtualisation = {
      memorySize = 4096;
      cores = 4;
      graphics = true;
      # The run-*-vm script splices these words into its exec line unescaped,
      # so shell parameter expansion happens at VM start time. Default stays
      # the interactive GL window (`nix run .#vm`); set ARCADE_VM_DISPLAY to
      # replace the whole GPU/display stanza, e.g. for a headless test boot
      # that still gives cage a DRM device (virtio-vga-gl refuses to start
      # without a GL-capable display, hence plain virtio-vga):
      #   ARCADE_VM_DISPLAY='-device virtio-vga -display none' run-doom-cab-vm-vm
      qemu.options = [
        ''''${ARCADE_VM_DISPLAY:--device virtio-vga-gl -display gtk,gl=on,show-cursor=off}''
      ];
      # ssh -p 2222 doom@localhost
      forwardPorts = [
        {
          from = "host";
          host.port = 2222;
          guest.port = 22;
        }
      ];
    };

    # Dev affordance, VM-variant only: the kiosk config is key-only SSH with
    # no keys, which would make the VM a locked box. Give doom a password and
    # allow password auth so you can actually get in.
    users.users.doom.initialPassword = "doom";
    services.openssh.settings.PasswordAuthentication = lib.mkForce true;
  };

  system.stateVersion = "26.05";
}
