# Bootable installer ISO for the physical cabinet (`nix build .#iso`).
#
# dd the ISO onto a flash drive, boot the target machine from it (UEFI),
# and the console autologs in as root — then run `doom-cade-install`.
#
# The ISO is deliberately fat (several GB): it carries the complete cabinet
# system closure, this repo's source, and every transitive flake input, so
# the install needs zero network. disko-install re-evaluates the flake to
# apply `--disk main <device>`; everything that evaluation wants is already
# in the ISO's store:
#   - the flake inputs resolve by narHash from the local store,
#   - the toplevel it builds is bit-identical to the baked cabinet toplevel
#     (--write-efi-boot-entries makes the install-time overrides match the
#     cabinet's own config values),
#   - the closureInfo derivation is pre-built below,
#   - the device-overridden disko script differs from the baked one only in
#     the device string, and its dependency closure (sgdisk, mkfs.*, …) is
#     baked via the cabinet's own diskoScript.
{
  config,
  lib,
  pkgs,
  modulesPath,
  self,
  disko,
  ...
}:

let
  cabinet = self.nixosConfigurations.cabinet;
  cabinetToplevel = cabinet.config.system.build.toplevel;

  # Exact derivation disko-install builds at install time — pre-realized here
  # so the offline install never has to compute it from scratch.
  cabinetClosureInfo = cabinet.pkgs.closureInfo { rootPaths = [ cabinetToplevel ]; };

  # This repo, as the flake `doom-cade-install` installs from.
  bakedRepo = self.outPath;

  # Every transitive flake input, so `builtins.getFlake` on the baked repo
  # resolves all locked inputs by narHash from the local store (this is what
  # `nix flake archive` would copy).
  collectFlakeInputs =
    input: [ input ] ++ lib.concatMap collectFlakeInputs (lib.attrValues (input.inputs or { }));
  bakedSources = lib.unique (map (i: i.outPath) (collectFlakeInputs self));

  diskoPkg = disko.packages.${pkgs.stdenv.hostPlatform.system}.disko;

  sshKeyConfigured = cabinet.config.users.users.doom.openssh.authorizedKeys.keys != [ ];

  installScript = pkgs.writeShellApplication {
    name = "doom-cade-install";
    runtimeInputs = [
      diskoPkg
      pkgs.util-linux
    ];
    text = ''
      red=$'\e[1;31m'
      bold=$'\e[1m'
      reset=$'\e[0m'

      # Test affordance: point DOOM_CADE_FLAKE at another flake#attr to
      # install a variant through the exact same machinery. The shipped
      # default is the pure cabinet config baked into this ISO.
      repo=${bakedRepo}
      flake="''${DOOM_CADE_FLAKE:-$repo#cabinet}"
      ssh_key_configured=${if sshKeyConfigured then "1" else "0"}

      if [ "$(id -u)" -ne 0 ]; then
        echo "doom-cade-install must run as root." >&2
        exit 1
      fi

      cat <<BANNER
      $bold=====================================================
        doom-cade cabinet installer
      =====================================================$reset

      This wipes ONE disk and installs the doom-cade arcade
      cabinet system onto it, entirely from this stick — no
      network needed. Config: $flake

      No ethernet? Run \`nmtui\` (ctrl-c out of here first) to
      join Wi-Fi. Optional — the install itself is fully
      offline — but any Wi-Fi connection you set up here is
      carried over to the installed cabinet.

      BANNER

      if [ "$ssh_key_configured" = 0 ]; then
        cat <<WARN
      ''${red}''${bold}WARNING: users.users.doom.openssh.authorizedKeys.keys is EMPTY
      in nix/hosts/cabinet.nix. The installed cabinet allows key-only
      SSH and has no console escape — with no key baked in it will be
      completely unreachable except by reinstalling. Strongly consider
      aborting (ctrl-c), setting your public key, rebuilding the ISO,
      and reflashing.''${reset}

      WARN
      fi

      echo "''${bold}Disks on this machine''${reset} (the installer stick is the one"
      echo "with /iso mounted under it):"
      echo
      lsblk -po NAME,SIZE,MODEL,TYPE,MOUNTPOINTS
      echo

      read -rp "Install target (device path, e.g. /dev/sda or /dev/nvme0n1): " device
      if [ ! -b "$device" ]; then
        echo "$device is not a block device; aborting." >&2
        exit 1
      fi

      echo
      echo "''${red}''${bold}ALL DATA ON $device WILL BE DESTROYED.''${reset}"
      read -rp "Re-type the device path to confirm: " device_again
      if [ "$device_again" != "$device" ]; then
        echo "Device paths do not match; aborting." >&2
        exit 1
      fi
      read -rp "Type YES (all caps) to wipe $device and install: " really
      if [ "$really" != "YES" ]; then
        echo "Aborting." >&2
        exit 1
      fi

      # Wi-Fi carry-over: NetworkManager profiles created in this live
      # environment (nmtui) are copied into the installed system's /persist,
      # where the cabinet's impermanence bind puts them back under
      # /etc/NetworkManager/system-connections on every boot.
      nm_dir=/etc/NetworkManager/system-connections
      extra_files_args=()
      if [ -d "$nm_dir" ] && [ -n "$(find "$nm_dir" -maxdepth 1 -name '*.nmconnection' -print -quit)" ]; then
        chmod 600 "$nm_dir"/*.nmconnection
        extra_files_args+=(--extra-files "$nm_dir" persist/etc/NetworkManager/system-connections)
        while IFS= read -r conn; do
          echo "Wi-Fi connection '$(basename "$conn" .nmconnection)' will carry over to the cabinet."
        done < <(find "$nm_dir" -maxdepth 1 -name '*.nmconnection' | sort)
      fi

      echo
      echo "Partitioning $device (disko) and installing $flake ..."
      echo "(The flake re-evaluation takes a few minutes; the store copy a few more.)"
      echo

      disko-install \
        --flake "$flake" \
        --disk main "$device" \
        --mode format \
        --write-efi-boot-entries \
        "''${extra_files_args[@]}"

      cat <<DONE

      ''${bold}Install complete.''${reset} Next steps:

       1. Remove this installer stick, then reboot.
       2. First boot goes straight into the kiosk: attract screen,
          leaderboards, PRESS START. It runs the bundled Freedoom with an
          UNVERIFIED IWAD banner until you provide a real doom2.wad.
       3. Plug in a USB stick containing doom2.wad — it auto-imports
          within a few seconds.
       4. To pin the IWAD hash: ssh in and run
            journalctl -u 'doom-arcade-wad-import@*'
          then set the printed sha256 as services.doom-arcade.iwadSha256
          in nix/hosts/cabinet.nix and push — the cabinet self-updates
          from the repo (comin).

      Remember: SSH into the cabinet only works with the public key(s)
      baked into nix/hosts/cabinet.nix at ISO build time.
      DONE
    '';
  };
in
{
  imports = [ "${modulesPath}/installer/cd-dvd/installation-cd-minimal.nix" ];

  networking.hostName = "doom-cade-installer";

  isoImage = {
    # Everything an offline install needs (see header comment).
    storeContents = [
      cabinetToplevel
      cabinetClosureInfo
      cabinet.config.system.build.diskoScript
      # The device-overridden disko script is the one derivation disko-install
      # must BUILD at install time (the device string is spliced into its
      # text). It is produced by nixpkgs' makeScriptWriter, whose builder is
      # runCommandLocal with makeBinaryWrapper in nativeBuildInputs — so the
      # writer's build environment must be in the store too, or an offline
      # install bottoms out trying to compile bash from source (empirically:
      # with these two present, exactly the two script derivations get built
      # and nothing else).
      cabinet.pkgs.stdenvNoCC
      cabinet.pkgs.makeBinaryWrapper
    ]
    ++ bakedSources;
    # Level 19 takes far too long on a multi-GB store for little gain.
    squashfsCompression = "zstd -Xcompression-level 6";
  };
  # Yields result/iso/doom-cade-installer.iso (the iso module derives the
  # artifact name from image.baseName, which it sets itself — hence mkForce).
  image.baseName = lib.mkForce "doom-cade-installer";

  environment.systemPackages = [
    installScript
    diskoPkg
  ];

  # Land on a root console; the getty help line and motd say what to run.
  services.getty.autologinUser = lib.mkForce "root";
  services.getty.helpLine = lib.mkAfter ''

    ** doom-cade cabinet installer **
    Run `doom-cade-install` to wipe a disk and install the cabinet.
    No ethernet? Run `nmtui` to join Wi-Fi (optional — the install is
    fully offline); the connection carries over to the cabinet.
  '';
  users.motd = ''

    ** doom-cade cabinet installer **
    Run `doom-cade-install` to wipe a disk and install the cabinet.
    (Offline install: everything needed is on this stick.)
    No ethernet? Run `nmtui` to join Wi-Fi (optional); the connection
    carries over to the installed cabinet.
  '';

  # Wi-Fi in the live installer. The installation-device profile on this
  # nixpkgs already enables NetworkManager (which ships nmtui/nmcli in PATH);
  # made explicit so a base-profile change can't silently regress it. Note:
  # the NM module itself sets networking.wireless.enable = true in
  # dbus-controlled mode (NM's wpa_supplicant backend) — do not force it off.
  networking.networkmanager.enable = true;

  # Serial console so headless/scripted installs (and the QEMU install test)
  # can drive the installer; tty0 last keeps the physical screen primary.
  boot.kernelParams = [
    "console=ttyS0,115200n8"
    "console=tty0"
  ];
  systemd.services."serial-getty@ttyS0" = {
    enable = true;
    wantedBy = [ "getty.target" ];
  };

  # The install is offline by design; never stall trying to substitute.
  nix.settings.substituters = lib.mkForce [ ];
  # For manual poking at the baked flake from the live console.
  nix.settings.experimental-features = [
    "nix-command"
    "flakes"
  ];

  # The flake re-evaluation inside disko-install is memory-hungry; zram keeps
  # it viable on small-RAM cabinet hardware.
  zramSwap.enable = true;

  system.stateVersion = "26.05";
}
