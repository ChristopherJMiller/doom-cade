#!/usr/bin/env bash
# check-no-wad.sh — fail CI if a DOOM WAD has been committed (SPEC §3).
#
# The IWAD is copyrighted and must never enter the repo or the Nix store.
# This scans every git-tracked file and flags any file larger than 1 MB
# whose first four bytes are the WAD magic header ("IWAD" or "PWAD").
#
# Run from anywhere inside the repo: bash scripts/check-no-wad.sh
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

offenders=()
while IFS= read -r -d '' file; do
  # Skip anything that is not a plain readable file (submodule dirs, etc.).
  [[ -f "$file" && -r "$file" ]] || continue

  size=$(wc -c <"$file")
  ((size > 1048576)) || continue

  # tr strips NUL bytes so bash's command substitution never warns on
  # binary content; the WAD magic itself is pure ASCII.
  magic=$(LC_ALL=C head -c 4 -- "$file" | LC_ALL=C tr -d '\0')
  if [[ "$magic" == "IWAD" || "$magic" == "PWAD" ]]; then
    offenders+=("$file")
  fi
done < <(git ls-files -z)

if ((${#offenders[@]} > 0)); then
  echo "ERROR: WAD file(s) committed to the repo — remove them and scrub history:" >&2
  printf '  %s\n' "${offenders[@]}" >&2
  exit 1
fi

echo "OK: no WAD files in the tree."
