# Thumb-drive IWAD import.
#
# Plug in a USB stick with doom2.wad anywhere in its top two directory
# levels and it gets verified and installed automatically:
#   - iwadSha256 pinned + hash matches   -> installed, kiosk restarted
#   - iwadSha256 pinned + hash MISMATCH  -> refused; the found hash is logged
#     prominently (journalctl -u 'doom-arcade-wad-import@*' shows the hash
#     you may want to pin)
#   - iwadSha256 unset (null)            -> installed anyway, hash logged with
#     a "pin this" hint; preflight will still flag it UNVERIFIED until pinned
#
# The same verify+install flow is available over SSH as
# `doom-arcade-import-wad /path/to/doom2.wad`.
{
  config,
  lib,
  pkgs,
  ...
}:

let
  cfg = config.services.doom-arcade;

  expected = if cfg.iwadSha256 == null then "" else cfg.iwadSha256;

  # Verify + install + apply. Nonzero exit on refusal so SSH users get a
  # useful status; the udev path swallows it.
  importWad = pkgs.writeShellApplication {
    name = "doom-arcade-import-wad";
    runtimeInputs = [
      pkgs.coreutils
      pkgs.systemd
    ];
    text = ''
      src="''${1:?usage: doom-arcade-import-wad /path/to/doom2.wad}"
      expected=${lib.escapeShellArg expected}
      destdir=/var/lib/doom-arcade/iwad
      dest="$destdir/doom2.wad"

      actual=$(sha256sum -- "$src" | cut -c1-64)

      if [ -n "$expected" ] && [ "$actual" != "$expected" ]; then
        echo "doom-arcade-import-wad: ================ REFUSED ================" >&2
        echo "doom-arcade-import-wad: candidate sha256: $actual" >&2
        echo "doom-arcade-import-wad: pinned sha256:    $expected" >&2
        echo "doom-arcade-import-wad: hash mismatch; existing IWAD left untouched." >&2
        echo "doom-arcade-import-wad: if the candidate is the copy you actually want," >&2
        echo "doom-arcade-import-wad: set services.doom-arcade.iwadSha256 = \"$actual\";" >&2
        echo "doom-arcade-import-wad: =========================================" >&2
        exit 1
      fi

      mkdir -p "$destdir"
      # Copy off the (hostile, possibly flaky) source first, then re-hash the
      # copy before atomically activating it.
      cp -- "$src" "$dest.new"
      copied=$(sha256sum -- "$dest.new" | cut -c1-64)
      if [ "$copied" != "$actual" ]; then
        echo "doom-arcade-import-wad: copy verification failed ($copied != $actual); aborting." >&2
        rm -f -- "$dest.new"
        exit 1
      fi
      chmod 0444 "$dest.new"
      chown doom:doom "$dest.new"
      mv -f -- "$dest.new" "$dest"

      if [ -z "$expected" ]; then
        echo "doom-arcade-import-wad: installed UNPINNED IWAD, sha256 $actual"
        echo "doom-arcade-import-wad: pin this as services.doom-arcade.iwadSha256 = \"$actual\";"
        echo "doom-arcade-import-wad: (it runs with the UNVERIFIED banner until pinned)"
      else
        echo "doom-arcade-import-wad: verified and installed doom2.wad ($actual)"
      fi

      # Take effect immediately. Ignore failure on hosts without the kiosk.
      systemctl restart doom-arcade-preflight.service cage-tty1.service || true
    '';
  };

  # udev-triggered wrapper: mount the stick read-only, look for the WAD, hand
  # it to the installer, always clean up, ALWAYS exit 0 (never wedge udev).
  wadImportUdev = pkgs.writeShellApplication {
    name = "doom-arcade-wad-import";
    runtimeInputs = [
      pkgs.coreutils
      pkgs.util-linux
      pkgs.findutils
      importWad
    ];
    text = ''
      kdev="''${1:?usage: doom-arcade-wad-import <kernel-block-device>}"
      dev="/dev/$kdev"
      mnt="/run/doom-arcade/import/$kdev"

      log() { echo "doom-arcade-wad-import($kdev): $*"; }

      # shellcheck disable=SC2329 # invoked indirectly via trap
      cleanup() {
        umount "$mnt" 2>/dev/null || true
        rmdir "$mnt" 2>/dev/null || true
      }
      trap cleanup EXIT

      mkdir -p "$mnt"

      # The stick is untrusted: read-only, nosuid, nodev, noexec, and only
      # filesystems a thumb drive plausibly carries.
      if ! mount -t vfat,exfat,ntfs3,ext2,ext3,ext4,iso9660,udf \
                 -o ro,nosuid,nodev,noexec "$dev" "$mnt" 2>/dev/null; then
        log "cannot mount $dev; ignoring"
        exit 0
      fi

      # Regular files only, shallow, plausibly IWAD-sized (1M..64M).
      wad=$(find "$mnt" -maxdepth 2 -type f -iname doom2.wad \
                 -size +1M -size -64M -print -quit 2>/dev/null || true)

      if [ -z "$wad" ]; then
        log "no doom2.wad found on $dev"
        exit 0
      fi

      log "found $wad"
      if doom-arcade-import-wad "$wad"; then
        log "import complete"
      else
        log "import refused or failed (see log above); existing IWAD untouched"
      fi
      exit 0
    '';
  };
in
{
  config = lib.mkIf cfg.enable {
    services.udev.extraRules = ''
      ACTION=="add", SUBSYSTEM=="block", ENV{ID_FS_USAGE}=="filesystem", ENV{ID_BUS}=="usb", TAG+="systemd", ENV{SYSTEMD_WANTS}+="doom-arcade-wad-import@%k.service"
    '';

    systemd.services."doom-arcade-wad-import@" = {
      description = "Import doom2.wad from USB block device /dev/%i";
      serviceConfig = {
        Type = "oneshot";
        ExecStart = "${wadImportUdev}/bin/doom-arcade-wad-import %i";
      };
    };

    environment.systemPackages = [ importWad ];
  };
}
