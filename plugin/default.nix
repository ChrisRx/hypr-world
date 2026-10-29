# The hypr-world glue plugin, built against `hyprland`. That has to be the
# exact compositor build the session runs: PLUGIN_INIT refuses any other.
# `app`, a package with bin/hypr-world, is what the world key launches; with
# none it is target/release/hypr-world beside the source the plugin was built
# from, which only makes sense for a build in the tree (dev.sh).
{
  lib,
  cmake,
  pkg-config,
  hyprland,
  app ? null,
}:

# hyprland.stdenv: the plugin is loaded into the compositor's address space,
# so it is built with the same toolchain.
hyprland.stdenv.mkDerivation {
  pname = "hypr-world-plugin";
  version = "0.1.0";

  src = ./.;

  nativeBuildInputs = [
    cmake
    pkg-config
  ];

  # hyprland's dev output carries hyprland.pc, and its own buildInputs are
  # everything those headers include.
  buildInputs = [ hyprland ] ++ hyprland.buildInputs;

  cmakeFlags = lib.optional (app != null) "-DHYPR_WORLD_APP=${app}/bin/hypr-world";

  installPhase = ''
    runHook preInstall
    install -Dm444 libhypr-world.so $out/lib/libhypr-world.so
    runHook postInstall
  '';
}
