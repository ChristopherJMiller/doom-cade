# Deterministic zip of the pk3/ tree (ZScript telemetry handler + MAPINFO).
# A .pk3 is just a zip file; GZDoom loads it with -file.
{
  lib,
  stdenvNoCC,
  zip,
}:

stdenvNoCC.mkDerivation {
  pname = "arcade-telemetry-pk3";
  version = "0.1.0";

  src = ../../pk3;

  nativeBuildInputs = [ zip ];

  dontConfigure = true;

  buildPhase = ''
    runHook preBuild
    # Determinism: zip's DOS timestamps cannot represent the epoch mtimes the
    # Nix store gives us, so normalize everything to a fixed representable
    # date, feed zip a sorted file list, and strip extra attributes (-X).
    find . -exec touch -h -d '1980-01-01T00:00:00Z' {} +
    find . -type f | sort | TZ=UTC zip -X -q "$TMPDIR/arcade-telemetry.pk3" -@
    runHook postBuild
  '';

  installPhase = ''
    runHook preInstall
    install -Dm444 "$TMPDIR/arcade-telemetry.pk3" "$out/share/arcade-telemetry.pk3"
    runHook postInstall
  '';

  meta = {
    description = "GZDoom telemetry + arcade-rules pk3 for the DOOM arcade cabinet";
    license = lib.licenses.mit;
    platforms = lib.platforms.all;
  };
}
