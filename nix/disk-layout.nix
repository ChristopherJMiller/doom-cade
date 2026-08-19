# Declarative disk layout (disko) for the physical cabinet.
#
# Single GPT disk: a 1G ESP and one big btrfs partition holding `nix` and
# `persist` subvolumes. The root filesystem is tmpfs (disko `nodev`) — the
# machine is impermanent (SPEC §9): everything that must survive a reboot
# lives under /persist (see the environment.persistence list in
# hosts/cabinet.nix) or in /nix.
#
# The device path is a mkDefault: `disko-install --disk main /dev/X`
# (what `doom-cade-install` on the installer ISO runs) overrides it at
# install time, so no hardware-specific path is baked into the repo.
{ lib, ... }:

{
  disko.devices = {
    # tmpfs root: nothing on disk backs "/", state cannot accumulate.
    nodev."/" = {
      fsType = "tmpfs";
      mountOptions = [
        "defaults"
        "size=2G"
        "mode=755"
      ];
    };

    disk.main = {
      type = "disk";
      device = lib.mkDefault "/dev/nvme0n1";
      content = {
        type = "gpt";
        partitions = {
          ESP = {
            priority = 1;
            size = "1G";
            type = "EF00";
            content = {
              type = "filesystem";
              format = "vfat";
              mountpoint = "/boot";
              mountOptions = [ "umask=0077" ];
            };
          };
          system = {
            priority = 2;
            size = "100%";
            content = {
              type = "btrfs";
              extraArgs = [
                "-f"
                "-L"
                "doom-cab"
              ];
              subvolumes = {
                "nix" = {
                  mountpoint = "/nix";
                  mountOptions = [
                    "compress=zstd"
                    "noatime"
                  ];
                };
                "persist" = {
                  mountpoint = "/persist";
                  mountOptions = [
                    "compress=zstd"
                    "noatime"
                  ];
                };
              };
            };
          };
        };
      };
    };
  };

  # Impermanence bind-mounts out of /persist before services start; disko
  # only emits the fileSystems entry, the boot ordering flag is ours to set.
  # (/nix is neededForBoot automatically.)
  fileSystems."/persist".neededForBoot = true;

  # There is no nixos-generate-config hardware file: the initrd must find the
  # disk on its own. NixOS's default module set already covers SATA/NVMe/
  # MMC/USB-input; add virtio (so the exact installed image boots under QEMU
  # — this is how the installer is integration-tested) and USB storage (in
  # case the cabinet's disk ever hangs off USB). Modules only load when the
  # matching hardware exists.
  boot.initrd.availableKernelModules = [
    "virtio_pci"
    "virtio_blk"
    "virtio_scsi"
    "usb_storage"
    "uas"
  ];
}
