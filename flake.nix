{
  description = "Superposition development environment";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    { self, nixpkgs, rust-overlay, ... }:
    let
      systems = [
        "aarch64-darwin"
        "x86_64-linux"
      ];
      forAllSystems =
        f:
        nixpkgs.lib.genAttrs systems (
          system:
          f (
            import nixpkgs {
              inherit system;
              overlays = [ rust-overlay.overlays.default ];
            }
          )
        );
    in
    {
      devShells = forAllSystems (
        pkgs:
        let
          inherit (pkgs) lib;
          linux = pkgs.stdenv.hostPlatform.isLinux;
          # The one pinned Wine build. Licences in the shared prefix and the
          # sp-wine-bridge unixlib ABI both depend on it; change it deliberately.
          wine = pkgs.wineWow64Packages.staging;
          mingw = pkgs.pkgsCross.mingwW64;
          # Libraries that eframe, the X11 editor host and the streamer open at run time.
          runtimeLibs = with pkgs; [
            libxkbcommon
            wayland
            libGL
            libx11
            libxcursor
            libxrandr
            libxi
            libxcb
            jack2
            alsa-lib
          ];
        in
        {
          default = pkgs.mkShell {
            packages =
              [
                (pkgs.rust-bin.stable."1.95.0".default.override {
                  extensions = [
                    "clippy"
                    "rustfmt"
                    "rust-src"
                    "llvm-tools-preview"
                  ];
                  targets =
                    [ "aarch64-apple-darwin" ]
                    ++ lib.optionals linux [ "x86_64-pc-windows-gnu" ];
                })
                pkgs.pkg-config
              ]
              ++ lib.optionals linux (
                [
                  pkgs.alsa-lib # MIDI (midir)
                  pkgs.jack2 # JACK library, and jackd for dummy-driver tests
                  pkgs.pipewire.jack # pw-jack on a PipeWire desktop
                  pkgs.libxcb
                  pkgs.xorg-server # Xvfb, the engine-owned editor display
                  pkgs.xdotool
                  wine
                  mingw.stdenv.cc # x86_64-w64-mingw32-gcc links the Windows worker
                ]
                ++ runtimeLibs
              );

            env = lib.optionalAttrs linux {
              LD_LIBRARY_PATH = lib.makeLibraryPath runtimeLibs;
              SUPERPOSITION_WINE = "${wine}/bin/wine";
              SUPERPOSITION_WINE_PREFIX_DIR = "${wine}";
              CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER = "${mingw.stdenv.cc}/bin/x86_64-w64-mingw32-gcc";
              CARGO_TARGET_X86_64_PC_WINDOWS_GNU_RUSTFLAGS = "-L native=${mingw.windows.pthreads}/lib";
            };
          };
        }
      );
    };
}
