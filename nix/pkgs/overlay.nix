# Overlay exposing the cabinet's packages. Applied by the flake's
# nixosConfigurations and exported as `overlays.default`.
final: prev: {
  doom-arcade = final.callPackage ./doom-arcade.nix { };
  arcade-telemetry-pk3 = final.callPackage ./telemetry-pk3.nix { };
}
