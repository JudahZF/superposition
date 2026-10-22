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
                ]
                ++ runtimeLibs
              );

            # Prefixed mingw tools (x86_64-w64-mingw32-dlltool for windows-sys) without the
            # cross wrapper's setup hooks.
            shellHook = lib.optionalString linux ''
              export PATH="${mingw.buildPackages.binutils-unwrapped}/bin:$PATH"
            '';

            env = lib.optionalAttrs linux {
              # Some hosts export `-L/usr/lib` here from /etc/profile; it would link against the
              # system glibc instead of the shell's.
              NIX_CFLAGS_LINK = "";
              LD_LIBRARY_PATH = lib.makeLibraryPath runtimeLibs;
              SUPERPOSITION_WINE = "${wine}/bin/wine";
              SUPERPOSITION_WINE_PREFIX_DIR = "${wine}";
              # The mingw wrapper is referenced, not added to `packages`: its setup hook would
              # replace the host linker flags and link Linux binaries against the wrong glibc.
              SUPERPOSITION_MINGW_CC = "${mingw.stdenv.cc}/bin/x86_64-w64-mingw32-gcc";
              CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER = "${mingw.stdenv.cc}/bin/x86_64-w64-mingw32-gcc";
              CARGO_TARGET_X86_64_PC_WINDOWS_GNU_RUSTFLAGS = "-L native=${mingw.windows.pthreads}/lib -L native=${mingw.windows.mcfgthreads}/lib";
            };
          };
        }
      );
    };
}
