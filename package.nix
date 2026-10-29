# The hypr-world app. plugin/default.nix, given this as `app`, builds a
# plugin that launches it.
{
  lib,
  rustPlatform,
  makeWrapper,
  pkg-config,
  alsa-lib,
  libgbm,
  libxkbcommon,
  vulkan-loader,
  wayland,
}:

let
  # winit and wgpu dlopen these at runtime rather than linking them, so they
  # go into the binary's RUNPATH (postFixup, below).
  runtimeLibs = [
    vulkan-loader
    wayland
    libxkbcommon
  ];
in
rustPlatform.buildRustPackage {
  pname = "hypr-world";
  version = "0.1.0";

  src = lib.fileset.toSource {
    root = ./.;
    fileset = lib.fileset.unions [
      ./Cargo.toml
      ./Cargo.lock
      ./src
      ./examples
      ./protocols
    ];
  };

  cargoLock.lockFile = ./Cargo.lock;

  nativeBuildInputs = [
    pkg-config
    makeWrapper
  ];

  # libgbm allocates the dmabufs window frames are captured into, and
  # alsa-lib is what the sound plays through (bevy_audio).
  buildInputs = runtimeLibs ++ [
    libgbm
    alsa-lib
  ];

  # It reads bevy_pbr's shaders out of ~/.cargo/registry, which a build here
  # has no copy of: the crates are vendored.
  checkFlags = [ "--skip=materials::tests::yuv_shader_compiles" ];

  # The patchelf has to come first: after wrapProgram, bin/hypr-world is
  # the shell wrapper and the binary is .hypr-world-wrapped.
  #
  # The drivers the app loads are the ones in /run/opengl-driver alone. On
  # NixOS that is all the loader finds anyway; elsewhere it would also try
  # the distro's, built against a different glibc.
  postFixup = ''
    patchelf --add-rpath ${lib.makeLibraryPath runtimeLibs} $out/bin/hypr-world
    wrapProgram $out/bin/hypr-world \
      --set-default VK_DRIVER_FILES /run/opengl-driver/share/vulkan/icd.d
  '';

  meta = {
    description = "Hyprland workspaces on the walls of a 3D house";
    license = lib.licenses.mit;
    mainProgram = "hypr-world";
    platforms = lib.platforms.linux;
  };
}
