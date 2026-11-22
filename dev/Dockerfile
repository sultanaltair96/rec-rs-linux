# syntax=docker/dockerfile:1
#
# Development and CI image for rec-rs-linux.
#
# The build host (WSL Ubuntu) has no Rust toolchain, no system development
# libraries and no sudo, so every build, test and self-test runs in this
# container. It is used through scripts/dev.sh:
#
#   scripts/dev.sh cargo build --release
#   scripts/dev.sh scripts/selftest.sh
#
# Everything is headless: no display server, no GPU, no audio hardware. The
# GStreamer test sources and the synthetic test pattern stand in for a desktop
# until phase 1 capture lands.
FROM fedora:43

ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin \
    CARGO_TERM_COLOR=always \
    LANG=C.UTF-8

# --- build toolchain and development headers ------------------------------
# - clang/pkg-config: -sys crates (gstreamer-sys, pipewire-sys, ...)
# - gstreamer*-devel + plugins-base-devel: gstreamer-rs
# - pipewire-devel: capture (phase 1A/1B)
# - wayland/xkb/x11/xcb: portals and the app window (phase 1A/1E)
# - dbus-devel: xdg-desktop-portal through zbus/ashpd (phase 1A)
# - libv4l-devel: V4L2 camera (phase 1B)
# - openssl-devel: rustls/openssl for streaming (phase 1D)
RUN dnf -y install --setopt=install_weak_deps=False \
        clang gcc gcc-c++ make pkgconf-pkg-config \
        git tar gzip xz curl which findutils procps-ng binutils diffutils \
        gstreamer1-devel gstreamer1-plugins-base-devel \
        pipewire-devel wayland-devel libxkbcommon-devel \
        libX11-devel libxcb-devel dbus-devel libv4l-devel openssl-devel \
    && dnf -y clean all

# --- runtime: GStreamer plugins, PipeWire, ffmpeg (ffprobe in tests) -------
# Fedora's free repositories have no x264enc, no x265enc and no VA-API H.264
# encoder, and gst-libav is built from ffmpeg-free (no libx264/libx265), so
# the only H.264 encoder here is openh264enc. AAC comes from fdkaacenc or
# avenc_aac, ProRes 422 from avenc_prores_ks. dev/ELEMENTS.md records what
# this image actually has (generated from gst-inspect-1.0).
RUN dnf -y install --setopt=install_weak_deps=False \
        gstreamer1 gstreamer1-plugins-base \
        gstreamer1-plugins-good gstreamer1-plugins-bad-free \
        gstreamer1-plugins-bad-free-extras gstreamer1-plugins-ugly-free \
        gstreamer1-plugin-openh264 gstreamer1-plugin-libav \
        pipewire pipewire-utils wireplumber \
        ffmpeg-free python3 \
    && dnf -y clean all

# --- Rust: rustup stable (>= 1.85 for edition 2024) + clippy + rustfmt -----
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
      | sh -s -- -y --no-modify-path --profile minimal \
          --default-toolchain stable --component clippy --component rustfmt \
    && rustup --version \
    && cargo --version \
    && rustc --version \
    && cargo clippy --version \
    && cargo fmt --version

# The repo is mounted at its host path and owned by the host user; running git
# in a volume with a different owner must not fail.
RUN git config --system --add safe.directory '*'

# The container runs as the host uid/gid (scripts/dev.sh), with HOME here.
# scripts/dev.sh mounts the named volume "rec-rs-linux-cache" at /home/dev, so
# the cargo registry cache (~/.cargo) and per-worktree target dirs
# (~/.cache/rec-rs-linux/target-<hash>) persist between runs without living in
# the repo or on the host.
RUN useradd --uid 1000 --user-group --create-home --home-dir /home/dev --shell /bin/bash dev \
    && mkdir -p /home/dev/.cargo /home/dev/.cache/rec-rs-linux \
    && chown -R dev:dev /home/dev \
    && chmod 0777 /home/dev

ENV HOME=/home/dev \
    CARGO_HOME=/home/dev/.cargo

WORKDIR /home/dev

CMD ["bash"]
