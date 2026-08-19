# Thumb-drive Wi-Fi import — the no-network recovery path.
#
# The cabinet has no console and key-only SSH, so if its Wi-Fi ever stops
# working (password rotated, network renamed) there would be no way back in.
# Mirror of the WAD import pattern: plug in a USB stick carrying either
#
#   - one or more NetworkManager keyfiles (*.nmconnection, top two directory
#     levels) — copied verbatim into /etc/NetworkManager/system-connections/
#     (root:root, 0600) and reloaded; covers enterprise/EAP setups; or
#   - a doom-cade-wifi.txt with lines:
#         ssid=YourNetwork
#         psk=YourPassword       (omit for an open network)
#         hidden=1               (optional)
#     turned into a profile idempotently via nmcli (a same-named profile is
#     replaced).
#
# Feedback channel is the journal, like the WAD import:
#   journalctl -u 'doom-cade-wifi-import@*'
#
# Threat model note: anyone with physical USB access can repoint the
# cabinet's network. That is consistent with the rest of the machine —
# physical access already means power, disk, and the WAD slot.
{
  config,
  lib,
  pkgs,
  ...
}:

let
  cfg = config.services.doom-arcade;

  # udev-triggered: mount the stick read-only, import what's there, always
  # clean up, ALWAYS exit 0 (never wedge udev). Degrades gracefully when
  # NetworkManager is not running or no Wi-Fi radio exists (the dev VM).
  wifiImportUdev = pkgs.writeShellApplication {
    name = "doom-cade-wifi-import";
    runtimeInputs = [
      pkgs.coreutils
      pkgs.util-linux
      pkgs.findutils
      pkgs.gnused
      pkgs.networkmanager
    ];
    text = ''
      kdev="''${1:?usage: doom-cade-wifi-import <kernel-block-device>}"
      dev="/dev/$kdev"
      # Distinct mount point from the WAD import's so one stick carrying
      # both doom2.wad and Wi-Fi config imports both without collision.
      mnt="/run/doom-cade/wifi-import/$kdev"

      log() { echo "doom-cade-wifi-import($kdev): $*"; }

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

      destdir=/etc/NetworkManager/system-connections
      mkdir -p "$destdir"

      # (a) Full keyfiles win: copy verbatim, then reload.
      found_keyfile=0
      # -size in bytes ('c'): find's default units round file sizes UP, so
      # "-size -1M" would reject every non-empty file smaller than 1M.
      while IFS= read -r -d "" f; do
        found_keyfile=1
        base=$(basename "$f")
        log "importing NetworkManager profile '$base'"
        install -o root -g root -m 600 "$f" "$destdir/$base"
      done < <(find "$mnt" -maxdepth 2 -type f -name '*.nmconnection' \
                    -size -1048576c -print0 2>/dev/null)

      if [ "$found_keyfile" = 1 ]; then
        if nmcli connection reload 2>/dev/null; then
          log "profiles reloaded; NetworkManager connects when the network is in range"
        else
          log "profiles installed; NetworkManager not reachable — they load on next boot"
        fi
        exit 0
      fi

      # (b) Simple ssid=/psk= file.
      conf=$(find "$mnt" -maxdepth 2 -type f -name doom-cade-wifi.txt \
                  -size -1048576c -print -quit 2>/dev/null || true)
      if [ -z "$conf" ]; then
        log "no *.nmconnection or doom-cade-wifi.txt on $dev; nothing to import"
        exit 0
      fi

      ssid=$(tr -d '\r' < "$conf" | sed -n 's/^ssid=//p' | head -n1)
      psk=$(tr -d '\r' < "$conf" | sed -n 's/^psk=//p' | head -n1)
      hidden=$(tr -d '\r' < "$conf" | grep -c '^hidden=1$' || true)

      if [ -z "$ssid" ]; then
        log "doom-cade-wifi.txt found but it has no ssid= line; nothing imported"
        exit 0
      fi
      if [ -n "$psk" ] && { [ "''${#psk}" -lt 8 ] || [ "''${#psk}" -gt 63 ]; }; then
        log "psk for '$ssid' must be 8..63 characters (got ''${#psk}); nothing imported"
        exit 0
      fi

      if [ -n "$psk" ]; then
        log "found doom-cade-wifi.txt: ssid '$ssid' (wpa-psk)"
      else
        log "found doom-cade-wifi.txt: ssid '$ssid' (open network)"
      fi

      # Idempotent: replace any same-named profile.
      nmcli connection delete "$ssid" >/dev/null 2>&1 || true

      args=(connection add type wifi con-name "$ssid" ssid "$ssid"
            connection.autoconnect yes)
      if [ -n "$psk" ]; then
        args+=(wifi-sec.key-mgmt wpa-psk wifi-sec.psk "$psk")
      fi
      if [ "$hidden" != 0 ]; then
        args+=(wifi.hidden yes)
      fi

      if nmcli "''${args[@]}" >/dev/null 2>&1; then
        log "created Wi-Fi profile '$ssid'"
        if nmcli connection up "$ssid" >/dev/null 2>&1; then
          log "connected to '$ssid'"
        else
          log "profile saved but not connected yet (radio absent or network out of range) — NetworkManager retries automatically"
        fi
      else
        log "nmcli could not create the profile (NetworkManager not running on this host?); nothing imported"
      fi
      exit 0
    '';
  };
in
{
  config = lib.mkIf cfg.enable {
    services.udev.extraRules = ''
      ACTION=="add", SUBSYSTEM=="block", ENV{ID_FS_USAGE}=="filesystem", ENV{ID_BUS}=="usb", TAG+="systemd", ENV{SYSTEMD_WANTS}+="doom-cade-wifi-import@%k.service"
    '';

    systemd.services."doom-cade-wifi-import@" = {
      description = "Import Wi-Fi config from USB block device /dev/%i";
      serviceConfig = {
        Type = "oneshot";
        ExecStart = "${wifiImportUdev}/bin/doom-cade-wifi-import %i";
      };
    };
  };
}
