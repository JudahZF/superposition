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
      pinnedWine = system: nixpkgs.legacyPackages.${system}.wineWow64Packages.staging;
    in
    {
      nixosModules.default = import ./nix/nixos-module.nix { wineFor = pinnedWine; };

      # Evaluates a NixOS system with the module enabled and checks the settings it must apply.
      checks.x86_64-linux.nixos-module =
        let
          pkgs = nixpkgs.legacyPackages.x86_64-linux;
          inherit (pkgs) lib;
          host = nixpkgs.lib.nixosSystem {
            system = "x86_64-linux";
            modules = [
              self.nixosModules.default
              {
                programs.superposition.enable = true;
                boot.loader.grub.enable = false;
                fileSystems."/" = {
                  device = "none";
                  fsType = "tmpfs";
                };
                system.stateVersion = "26.05";
              }
            ];
          };
          inherit (host) config;
          ok =
            lib.elem "ntsync" config.boot.kernelModules
            && lib.any (
              limit: limit.domain == "@audio" && limit.item == "rtprio" && limit.value == "95"
            ) config.security.pam.loginLimits
            && config.security.rtkit.enable
            && config.environment.sessionVariables.SUPERPOSITION_WINE
              == "${pinnedWine "x86_64-linux"}/bin/wine";
        in
        assert ok;
        pkgs.runCommand "superposition-nixos-module" { } "touch $out";

      devShells = forAllSystems (
        pkgs:
        let
          inherit (pkgs) lib;
          linux = pkgs.stdenv.hostPlatform.isLinux;
          # The one pinned Wine build. Licences in the shared prefix and the
          # sp-wine-bridge unixlib ABI both depend on it; change it deliberately.
          wine = pinnedWine pkgs.stdenv.hostPlatform.system;
          mingw = pkgs.pkgsCross.mingwW64;
          # zig's bundled macOS libc headers let C build scripts (zstd-sys, ring) run for
          # `--target aarch64-apple-darwin`, so Linux can type-check the macOS code.
          zigCcMacos = pkgs.writeShellScript "zig-cc-aarch64-macos" ''
            args=()
            skip=0
            for arg in "$@"; do
              if [ "$skip" = 1 ]; then skip=0; continue; fi
              case "$arg" in
                -arch) skip=1 ;;
                --target=* | -mmacosx-version-min=*) ;;
                *) args+=("$arg") ;;
              esac
            done
            exec ${pkgs.zig}/bin/zig cc -target aarch64-macos "''${args[@]}"
          '';
          zigAr = pkgs.writeShellScript "zig-ar" ''exec ${pkgs.zig}/bin/zig ar "$@"'';
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
                # Verification tools `cargo xtask doctor` requires.
                pkgs.cargo-nextest
                pkgs.cargo-deny
                pkgs.cargo-audit
                pkgs.cargo-llvm-cov
              ]
              ++ lib.optionals linux (
                [
                  pkgs.alsa-lib # MIDI (midir)
                  pkgs.jack2 # JACK library, and jackd for dummy-driver tests
                  pkgs.pipewire.jack # pw-jack on a PipeWire desktop
                  pkgs.libxcb
                  pkgs.xorg-server # Xvfb, the engine-owned editor display
                  pkgs.xdotool
                  pkgs.squashfsTools # mksquashfs for the AppImage
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
              SUPERPOSITION_WINE_ROOT = "${wine}";
              # The mingw wrapper is referenced, not added to `packages`: its setup hook would
              # replace the host linker flags and link Linux binaries against the wrong glibc.
              SUPERPOSITION_MINGW_CC = "${mingw.stdenv.cc}/bin/x86_64-w64-mingw32-gcc";
              SUPERPOSITION_MINGW_LIB_DIRS = "${mingw.windows.pthreads}/lib:${mingw.windows.mcfgthreads}/lib";
              CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER = "${mingw.stdenv.cc}/bin/x86_64-w64-mingw32-gcc";
              CC_aarch64_apple_darwin = "${zigCcMacos}";
              AR_aarch64_apple_darwin = "${zigAr}";
              CARGO_TARGET_X86_64_PC_WINDOWS_GNU_RUSTFLAGS = "-L native=${mingw.windows.pthreads}/lib -L native=${mingw.windows.mcfgthreads}/lib";
            };
          };
        }
      );
    };
}
