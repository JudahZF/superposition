# Superposition for Linux (x86_64)

Run `bin/superposition` from the tarball, or run the AppImage
(`Superposition-<version>-x86_64.AppImage`). The AppImage needs FUSE; without FUSE, add
`--appimage-extract-and-run`. Both hold the same files.

Run `superposition --doctor` to check the system settings below. The engine also prints any
unmet setting when it starts. Plug-in editors, Windows plug-ins, and remote serving need nothing
else from this package; the system must provide:

- **Audio:** a JACK server (jackd, or PipeWire with its JACK library, `pipewire-jack`). The
  server's rate and quantum must match the session, for example
  `pw-metadata -n settings 0 clock.force-quantum 128`.
- **Editors:** `Xvfb` (`xvfb` or `xorg-x11-server-Xvfb`), plus libxcb, libX11, libxkbcommon and an
  OpenGL driver (Mesa) for the app window.
- **MIDI:** the ALSA sequencer (`libasound2`).
- **glibc 2.36 or newer.**

## Real-time scheduling

Grant real-time priority, or plug-in racks will miss deadlines on a busy system: add
`@audio - rtprio 95` and `@audio - memlock unlimited` to `/etc/security/limits.d/audio.conf` and
add your user to `audio` (on NixOS: the flake's module). Without the limit, Superposition asks
rtkit, if it runs, for its JACK callback thread and every plug-in worker, Windows ones included,
at rtkit's lower priority (usually 20). Superposition warns
once when it runs without real-time priority; `SUPERPOSITION_REQUIRE_REALTIME=1` makes that an
error.

## Windows plug-ins

`lib/superposition/wine` is the pinned Wine build (11.18 staging). Windows plug-ins and their
licences live in one prefix, `~/.local/share/superposition/wine`. Install them with
Setup → "Run Windows program…". Keep this Wine build: licences such as iLok can break when Wine
changes. For faster Wine synchronisation, load the kernel's `ntsync` module (`modprobe ntsync`).

## Remote

`bin/superposition --serve` runs the engine without a window and prints a pairing code;
`bin/superposition --connect HOST --pair CODE` shows and drives it from another machine.
