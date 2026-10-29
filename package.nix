# The hypr-world app. plugin/default.nix, given this as `app`, builds a
# plugin that launches it.
{
  lib,
  rustPlatform,
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

  nativeBuildInputs = [ pkg-config ];

  # libgbm allocates the dmabufs window frames are captured into, and
  # alsa-lib is what the sound plays through (bevy_audio).
  buildInputs = runtimeLibs ++ [
    libgbm
    alsa-lib
  ];

  # It reads bevy_pbr's shaders out of ~/.cargo/registry, which a build here
  # has no copy of: the crates are vendored.
  checkFlags = [ "--skip=materials::tests::yuv_shader_compiles" ];

  postFixup = ''
    patchelf --add-rpath ${lib.makeLibraryPath runtimeLibs} $out/bin/hypr-world
  '';

  meta = {
    description = "Hyprland workspaces on the walls of a 3D house";
    license = lib.licenses.mit;
    mainProgram = "hypr-world";
    platforms = lib.platforms.linux;
  };
}
