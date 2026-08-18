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

      # Take effect immediately. A direct `systemctl restart` would be
      # denied by polkit for the unprivileged doom SSH user, so the restart
      # lives in a root oneshot unit (doom-arcade-apply-wad.service) that a
      # polkit rule below lets doom start. Never silently swallowed: if the
      # restart cannot happen, say so instead of claiming success.
      if systemctl start doom-arcade-apply-wad.service; then
        echo "doom-arcade-import-wad: kiosk restarted; the new IWAD is live"
      else
        echo "doom-arcade-import-wad: WARNING: could not restart the kiosk" >&2
        echo "doom-arcade-import-wad: the new IWAD takes effect on the next reboot" >&2
      fi
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

    # Privileged apply step for a freshly imported IWAD. The import script
    # runs unprivileged over SSH (user doom), which polkit rightly stops
    # from restarting arbitrary units — so the exact restart it needs is
    # packaged as a root oneshot that doom is allowed to start (rule below).
    systemd.services.doom-arcade-apply-wad = {
      description = "Apply a newly imported IWAD (re-verify and restart the kiosk)";
      path = [ pkgs.systemd ];
      serviceConfig.Type = "oneshot";
      script = ''
        systemctl restart doom-arcade-preflight.service
        # Hosts without the kiosk just refresh the preflight env file.
        if systemctl cat cage-tty1.service >/dev/null 2>&1; then
          systemctl restart cage-tty1.service
        fi
      '';
    };

    security.polkit.enable = true;
    security.polkit.extraConfig = ''
      // Allow the doom user (the only SSH identity on the cabinet) to start
      // exactly the IWAD apply unit — nothing else.
      polkit.addRule(function(action, subject) {
        if (action.id == "org.freedesktop.systemd1.manage-units" &&
            subject.user == "doom" &&
            action.lookup("unit") == "doom-arcade-apply-wad.service" &&
            action.lookup("verb") == "start") {
          return polkit.Result.YES;
        }
      });
    '';

    environment.systemPackages = [ importWad ];
  };
}
