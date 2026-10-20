#!/bin/sh
# CI only: Bionic's dummy 0.3.x lacks multiple RandR outputs. Build against its Xorg ABI.
set -eu
archive=/tmp/uu-xf86-video-dummy-0.4.0.tar.xz
curl --fail --location --proto '=https' --tlsv1.2 \
  https://xorg.freedesktop.org/archive/individual/driver/xf86-video-dummy-0.4.0.tar.xz -o "$archive"
printf '%s  %s\n' e78ceae5c8c0588c7cb658f2afc3a9fac9ef665b52a75b01f8e9c5449a4e1e5a "$archive" | sha256sum -c -
tar -xJf "$archive" -C /tmp
cd /tmp/xf86-video-dummy-0.4.0
./configure --prefix=/usr --with-xorg-module-dir="$(pkg-config --variable=moduledir xorg-server)"
make -j2
make install
