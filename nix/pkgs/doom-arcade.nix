# The Rust workspace: arcade-supervisor, arcade-attract, arcade-leaderboard.
{
  lib,
  rustPlatform,
  pkg-config,
  makeWrapper,
  wayland,
  libxkbcommon,
  libGL,
  fontconfig,
}:

let
  # winit/glow dlopen these at runtime; they are not linked, so they must be
  # injected via LD_LIBRARY_PATH in postFixup below.
  guiLibs = [
    wayland
    libxkbcommon
    libGL
  ];
in
rustPlatform.buildRustPackage {
  pname = "doom-arcade";
  version = "0.1.0";

  src = lib.cleanSource ../..;

  cargoLock.lockFile = ../../Cargo.lock;

  nativeBuildInputs = [
    pkg-config
    makeWrapper
  ];

  buildInputs = guiLibs ++ [ fontconfig ];

  # Tests run in CI via `cargo test` / `cargo clippy` (SPEC §12); the package
  # build stays lean and avoids test-only runtime requirements.
  doCheck = false;

  postInstall = ''
    install -Dm444 assets/config/gzdoom.ini $out/share/doom-arcade/gzdoom.ini
  '';

  postFixup = ''
    for bin in arcade-attract arcade-supervisor; do
      wrapProgram "$out/bin/$bin" \
        --prefix LD_LIBRARY_PATH : ${lib.makeLibraryPath guiLibs}
    done
  '';

  meta = {
    description = "DOOM arcade cabinet: supervisor, attract UI, and leaderboard service";
    license = lib.licenses.mit;
    platforms = lib.platforms.linux;
    mainProgram = "arcade-supervisor";
  };
}
