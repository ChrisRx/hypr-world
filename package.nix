# The hypr-world app. plugin/default.nix, given this as `app`, builds a
# plugin that launches it.
{
  lib,
  rustPlatform,
  makeWrapper,
  pkg-config,
  alsa-lib,
  pipewire,
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

  # The shader tests (src/materials.rs) compile against bevy_pbr's shaders,
  # which a build here has among the vendored crates, not in
  # ~/.cargo/registry.
  preCheck = ''
    export HYPR_WORLD_CRATES="$cargoDeps"
  '';

  # The patchelf has to come first: after wrapProgram, bin/hypr-world is
  # the shell wrapper and the binary is .hypr-world-wrapped.
  #
  # The drivers the app loads are the ones in /run/opengl-driver alone. On
  # NixOS that is all the loader finds anyway; elsewhere it would also try
  # the distro's, built against a different glibc.
  #
  # The sound plays through ALSA's "default", which on a PipeWire system is
  # the pipewire ALSA plugin (/etc/alsa/conf.d/99-pipewire-default.conf).
  # alsa-lib here looks for plugins in its own store path, where there are
  # none, so without ALSA_PLUGIN_DIR "default" fails to open and cpal falls
  # back to a raw hw device (an HDMI output, say). Holding that directly,
  # past PipeWire, the stream sticks in XRUN once the sink goes away (a
  # monitor sleeping or unplugged) and cpal's output thread spins a core on
  # it. NixOS's own ALSA config names the plugin by store path, so there
  # this changes nothing. --set-default keeps a value already set.
  postFixup = ''
    patchelf --add-rpath ${lib.makeLibraryPath runtimeLibs} $out/bin/hypr-world
    wrapProgram $out/bin/hypr-world \
      --set-default VK_DRIVER_FILES /run/opengl-driver/share/vulkan/icd.d \
      --set-default ALSA_PLUGIN_DIR ${pipewire}/lib/alsa-lib
  '';

  meta = {
    description = "Hyprland workspaces on the walls of a 3D house";
    license = lib.licenses.mit;
    mainProgram = "hypr-world";
    platforms = lib.platforms.linux;
  };
}
