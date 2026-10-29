{
  description = "hypr-world: Hyprland workspaces on the walls of a 3D house, in Bevy";

  inputs.nixpkgs.url = "github:nixos/nixpkgs/nixos-unstable";

  outputs =
    { nixpkgs, ... }:
    let
      system = "x86_64-linux";
      pkgs = nixpkgs.legacyPackages.${system};

      # winit and wgpu dlopen these at runtime rather than linking them, so
      # the app finds them through its RUNPATH (RUSTFLAGS, below).
      runtimeLibs = with pkgs; [
        vulkan-loader
        wayland
        libxkbcommon
      ];
    in
    {
      packages.${system}.default = pkgs.callPackage ./package.nix { };

      # Native libraries only. The Rust toolchain is rustup's, selected by
      # ./rust-toolchain.toml; .envrc loads this shell on cd.
      devShells.${system}.default = pkgs.mkShell {
        # wayland-sys links libwayland-client and finds it with pkg-config.
        packages = [ pkgs.pkg-config ];

        # libgbm allocates the dmabufs window frames are captured into.
        # alsa-lib is what the sound plays through (bevy_audio).
        buildInputs = runtimeLibs ++ [ pkgs.libgbm pkgs.alsa-lib ];

        # Their store paths in the binary's RUNPATH, so target/release/hypr-world
        # runs from anywhere (the plugin launches it) without this shell, and
        # nothing else started from the shell gets them on LD_LIBRARY_PATH.
        RUSTFLAGS = "-C link-arg=-Wl,-rpath,${pkgs.lib.makeLibraryPath runtimeLibs}";
      };
    };
}
