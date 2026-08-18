# The physical cabinet.
#
# `comin` and `impermanence` arrive via specialArgs from flake.nix.
{
  config,
  lib,
  pkgs,
  comin,
  impermanence,
  ...
}:

{
  imports = [
    ../module.nix
    ../kiosk.nix
    ../wad-import.nix
    impermanence.nixosModules.impermanence
    comin.nixosModules.comin
  ];

  networking.hostName = "doom-cab";

  services.doom-arcade = {
    enable = true;
    # TODO: pin this. Provision doom2.wad (thumb drive or
    # `doom-arcade-import-wad` over SSH), read the sha256 the import/preflight
    # logs print, and paste it here. Until then the cabinet runs the IWAD (or
    # the Freedoom fallback) with the UNVERIFIED banner.
    iwadSha256 = null;
    cabinetId = "cab-1";
    # Local leaderboard. Point .url at another host instead if the
    # leaderboard moves off-cabinet (e.g. the k8s cluster), and disable this.
    leaderboard.enable = true;
  };

  # TODO: replace with your actual public key(s). PasswordAuthentication is
  # off (kiosk.nix) — without a key here the machine is SSH-inaccessible.
  users.users.doom.openssh.authorizedKeys.keys = [
    # "ssh-ed25519 AAAA... you@laptop"
  ];

  # ---------------------------------------------------------------------------
  # GitOps self-update: comin polls the repo and switches to what main builds.
  services.comin = {
    enable = true;
    # comin evaluates nixosConfigurations."<hostname>" from the flake; our
    # attribute is named `cabinet` while networking.hostName is doom-cab, so
    # set it explicitly.
    hostname = "cabinet";
    remotes = [
      {
        name = "origin";
        # Public repo — comin polls anonymously over https (push access uses
        # the git@ SSH remote and is not needed here).
        url = "https://github.com/ChristopherJMiller/doom-cade.git";
        branches.main.name = "main";
      }
    ];
  };

  # Comin adds a full system closure per merged commit but never collects
  # garbage (it only unlinks stale profiles), so an unattended cabinet's
  # /nix grows until builds fail with ENOSPC and self-update dies silently.
  # Reclaim regularly and keep the ESP bounded too.
  nix.gc = {
    automatic = true;
    dates = "weekly";
    options = "--delete-older-than 14d";
  };
  nix.optimise.automatic = true;
  boot.loader.systemd-boot.configurationLimit = 5;

  # ---------------------------------------------------------------------------
  # Impermanence: tmpfs root, state survives only in /persist (SPEC §9).
  environment.persistence."/persist" = {
    hideMounts = true;
    directories = [
      "/var/lib/doom-arcade" # IWAD + score spool (+ local leaderboard DB)
      "/etc/ssh"             # host keys
      "/var/lib/comin"       # comin's deployment state (state.json etc.)
      "/var/lib/nixos"       # uid/gid maps — keeps doom's uid stable
    ];
    files = [
      "/etc/machine-id"
    ];
  };

  # ===========================================================================
  # PLACEHOLDER DISK LAYOUT — REPLACE ON THE REAL MACHINE.
  #
  # On the physical cabinet:
  #   1. Partition: ESP (vfat, label BOOT), /nix (ext4, label nix),
  #      /persist (ext4, label persist). Root is tmpfs.
  #   2. Run `nixos-generate-config --root /mnt` and paste the generated
  #      hardware section (bus IDs, initrd kernel modules, cpu microcode)
  #      here or into a hardware-configuration.nix imported here.
  #   3. Keep root on tmpfs and keep neededForBoot on /persist, otherwise
  #      impermanence cannot bind things in before services start.
  # ===========================================================================
  fileSystems."/" = {
    device = "none";
    fsType = "tmpfs";
    options = [
      "defaults"
      "size=2G"
      "mode=755"
    ];
  };
  fileSystems."/nix" = {
    device = "/dev/disk/by-label/nix"; # PLACEHOLDER
    fsType = "ext4";
    neededForBoot = true;
  };
  fileSystems."/persist" = {
    device = "/dev/disk/by-label/persist"; # PLACEHOLDER
    fsType = "ext4";
    neededForBoot = true;
  };
  fileSystems."/boot" = {
    device = "/dev/disk/by-label/BOOT"; # PLACEHOLDER
    fsType = "vfat";
  };
  boot.loader.systemd-boot.enable = true;
  boot.loader.efi.canTouchEfiVariables = true;
  # ===========================================================================

  system.stateVersion = "26.05";
}
