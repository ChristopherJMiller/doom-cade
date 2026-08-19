# The physical cabinet.
#
# `comin`, `impermanence`, and `disko` arrive via specialArgs from flake.nix.
{
  config,
  lib,
  pkgs,
  comin,
  impermanence,
  disko,
  ...
}:

{
  imports = [
    ../module.nix
    ../kiosk.nix
    ../wad-import.nix
    ../wifi-import.nix
    ../disk-layout.nix
    impermanence.nixosModules.impermanence
    comin.nixosModules.comin
    disko.nixosModules.disko
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
    # Local leaderboard, browsable from the office subnet: the attract
    # screen shows the URL (LAN IP, auto-detected). GET is open; POST is
    # protected by a locally-minted token, satisfying the module's
    # openFirewall assertions. Point .url at another host instead if the
    # leaderboard ever moves off-cabinet.
    leaderboard = {
      enable = true;
      listen = "0.0.0.0:8080";
      url = "http://127.0.0.1:8080"; # what supervisor/attract talk to
      openFirewall = true;
      generateToken = true;
    };
  };

  # mDNS convenience alias: http://doom-cab.local:8080 where the office
  # network allows multicast DNS (the numeric IP on the attract screen is
  # the universal fallback).
  services.avahi = {
    enable = true;
    publish = {
      enable = true;
      addresses = true;
    };
  };

  # TODO: replace with your actual public key(s). PasswordAuthentication is
  # off (kiosk.nix) — without a key here the machine is SSH-inaccessible.
  users.users.doom.openssh.authorizedKeys.keys = [
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAICHR4q3amhKDhCF6+xa3oTXJX2ycN503+cEo/gpnOkFt git@chrismiller.xyz"
  ];

  # ---------------------------------------------------------------------------
  # Networking: NetworkManager, so Wi-Fi profiles created on the installer
  # stick (nmtui) carry over — doom-cade-install copies them into
  # /persist/etc/NetworkManager/system-connections, and the persistence bind
  # below puts them back under /etc every boot. Wired DHCP still works under
  # NM. This lives here rather than kiosk.nix because it is physical-cabinet
  # plumbing; the dev VM keeps its default qemu networking.
  networking.networkmanager.enable = true;
  # Powersave trades Wi-Fi latency/reliability for milliwatts — the wrong
  # trade for a plugged-in cabinet.
  networking.networkmanager.wifi.powersave = false;
  # Lets an SSH'd `doom` session run nmcli/nmtui to change networks later.
  users.users.doom.extraGroups = [ "networkmanager" ];

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
      "/var/lib/comin"       # comin's deployment state (state.json etc.)
      "/var/lib/nixos"       # uid/gid maps — keeps doom's uid stable
      "/etc/NetworkManager/system-connections" # Wi-Fi profiles (installer carry-over + nmtui edits)
    ];
    files = [
      "/etc/machine-id"
    ];
  };

  # SSH host keys live in /persist directly (sshd_config points there) rather
  # than behind an impermanence bind. A directory bind on /etc/ssh would
  # shadow the store-symlinked sshd_config — sshd then fails to start with
  # "No such file or directory" (found by the QEMU install test) — and
  # file binds race ssh-keygen with empty placeholder files.
  services.openssh.hostKeys = [
    {
      path = "/persist/etc/ssh/ssh_host_ed25519_key";
      type = "ed25519";
    }
    {
      path = "/persist/etc/ssh/ssh_host_rsa_key";
      type = "rsa";
      bits = 4096;
    }
  ];

  # Disk layout is declarative via disko (../disk-layout.nix): tmpfs root,
  # 1G ESP, btrfs `nix`/`persist` subvolumes. Override the device at install
  # time with `disko-install --disk main /dev/X` (the installer ISO's
  # `doom-cade-install` prompts for it) — nothing hardware-specific here.
  boot.loader.systemd-boot.enable = true;
  boot.loader.efi.canTouchEfiVariables = true;

  system.stateVersion = "26.05";
}
