# Builds the Linux binaries against Debian 12's glibc (2.36), so the tarball runs on common
# distributions. `cargo xtask bundle-linux` builds this image and runs it offline against a
# vendored source tree.
FROM rust:1.95-bookworm
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      pkg-config libjack-jackd2-dev libasound2-dev \
      libxcb1-dev libxcb-shm0-dev libxcb-composite0-dev libxcb-damage0-dev libxcb-xfixes0-dev \
      libxcb-xtest0-dev libxcb-render0-dev libxcb-shape0-dev libx11-dev libxkbcommon-dev \
      libwayland-dev libgl-dev \
 && rm -rf /var/lib/apt/lists/*
