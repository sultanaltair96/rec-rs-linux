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
# until phase 1 capture lands. The image does carry the *software* versions of
# those pieces — a headless sway + portal session, llvmpipe Vulkan, pipewire,
# a keyring and V4L2 tools — so phase 1 workers can test against them, and it
# enables RPM Fusion so the encoder story matches upstream (H.264 High with
# bitrate control, HEVC). Packaging (Flatpak/AppImage) is a separate decision.
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
# - wayland/xkb/x11/xcb: portals and the app window (phase 1A/1E); winit
#   dlopens libxkbcommon-x11.so for the X11 SIGTERM regression self-test
# - dbus-devel: xdg-desktop-portal through zbus/ashpd (phase 1A)
# - libv4l-devel: V4L2 camera (phase 1B)
# - openssl-devel: rustls/openssl for streaming (phase 1D)
RUN dnf -y install --setopt=install_weak_deps=False \
        clang gcc gcc-c++ make pkgconf-pkg-config \
        git tar gzip xz curl which findutils procps-ng binutils diffutils \
        gstreamer1-devel gstreamer1-plugins-base-devel \
        pipewire-devel wayland-devel libxkbcommon-devel libxkbcommon-x11-devel \
        libX11-devel libxcb-devel dbus-devel libv4l-devel openssl-devel \
    && dnf -y clean all

# --- RPM Fusion -----------------------------------------------------------
# Fedora's free repositories have no x264enc, no x265enc, no VA-API encoder
# and a gst-libav built from ffmpeg-free, so an unmodified image can only
# record H.264 Constrained Baseline at an unbounded bitrate and cannot record
# HEVC at all. A recorder's *dev* image may use RPM Fusion: it provides
# x264enc, x265enc, a full ffmpeg (and so avdec_h264/avdec_h265 for gst-libav)
# and the VA-API plugin packages. Whether the shipped Flatpak/AppImage may do
# the same is a packaging question for phase 1F.
RUN dnf -y install \
        https://mirrors.rpmfusion.org/free/fedora/rpmfusion-free-release-43.noarch.rpm \
        https://mirrors.rpmfusion.org/nonfree/fedora/rpmfusion-nonfree-release-43.noarch.rpm \
    && dnf -y makecache && dnf -y clean all

# --- runtime: GStreamer plugins, PipeWire, ffmpeg (ffprobe in tests) -------
# Encoders: x264enc (H.264, profile High, rate control honoured), x265enc
# (HEVC), openh264enc (fallback), avenc_prores_ks (ProRes 422), fdkaacenc and
# avenc_aac (AAC). The VA-API elements (vah264enc/vaapih265enc and the older
# vaapih264enc) come from gstreamer1-vaapi, but GStreamer registers no VA
# factory without a working VA display: with no GPU the elements are present in
# the packages and NOT in gst-inspect's list. dev/ELEMENTS.md records exactly
# what this image has, and the writer probes an encoder before using it, so a
# GPU-less host records with x264enc.
#
# Test harness for the phase 1 workers (one image, so nobody rebuilds later):
#   - sway + foot + xdg-desktop-portal(-wlr) + dbus + Xvfb: a headless desktop
#     for screen capture (1A) and the app (1E)
#   - mesa-vulkan-drivers (lavapipe) + vulkan-tools: a software Vulkan device
#     for wgpu (1C)
#   - pipewire-gstreamer + pipewire-pulseaudio + pulseaudio-utils: attempt to
#     check pipewiresrc/pipewiresink and pactl against a session
#   - gnome-keyring + libsecret: the Secret Service (1D)
#   - v4l-utils: camera device inspection (1B)
RUN dnf -y install --setopt=install_weak_deps=False \
        gstreamer1 gstreamer1-plugins-base \
        gstreamer1-plugins-good gstreamer1-plugins-bad-free \
        gstreamer1-plugins-bad-free-extras \
        gstreamer1-plugins-ugly gstreamer1-plugins-bad-freeworld \
        gstreamer1-vaapi gstreamer1-plugin-openh264 gstreamer1-plugin-libav \
        pipewire pipewire-utils wireplumber \
        pipewire-gstreamer pipewire-pulseaudio pulseaudio-utils \
        ffmpeg python3 \
        sway foot xdg-desktop-portal xdg-desktop-portal-wlr \
        dbus-daemon dbus-tools dbus-broker xorg-x11-server-Xvfb \
        mesa-vulkan-drivers mesa-dri-drivers vulkan-loader vulkan-tools \
        gnome-keyring libsecret v4l-utils \
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
