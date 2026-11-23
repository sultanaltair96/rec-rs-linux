#!/bin/sh
# The in-container half of packaging/appimage/build-appimage.sh.
#
# Runs in an Ubuntu 24.04 container (glibc 2.39: Ubuntu 24.04+, Debian 13+,
# Fedora 40+; and PipeWire 1.0, which pipewire-rs 0.10 needs to compile) as
# root. Not meant to be run on the host.
#
# Env: REPO (checkout), OUT (/out, shared with the host), JOBS (default 4),
#      VERSION (Rec version from Cargo.toml).
set -eu

REPO=${REPO:?}
OUT=${OUT:-/out}
JOBS=${JOBS:-4}
VERSION=${VERSION:?}

export DEBIAN_FRONTEND=noninteractive
export PATH=/root/.cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin

echo "== packages"
apt-get update -qq
apt-get install -y -qq --no-install-recommends \
    build-essential pkg-config git curl ca-certificates file wget xz-utils \
    libclang-dev clang \
    libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev \
    libpipewire-0.3-dev libx11-dev libxcb1-dev libdbus-1-dev libv4l-dev \
    libssl-dev libwayland-dev libxkbcommon-dev libsecret-1-dev libglib2.0-dev \
    libudev-dev zlib1g-dev \
    gstreamer1.0-tools gstreamer1.0-plugins-base gstreamer1.0-plugins-good \
    gstreamer1.0-plugins-bad gstreamer1.0-plugins-ugly gstreamer1.0-libav \
    >/dev/null

echo "== rust"
if [ ! -x /root/.cargo/bin/cargo ]; then
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs |
        sh -s -- -y --no-modify-path --profile minimal --default-toolchain stable
fi
cargo --version
rustc --version

echo "== build (release, locked)"
cd "$REPO"
CARGO_TARGET_DIR=/root/target cargo build --release --locked --jobs "$JOBS"

echo "== GStreamer this build sees"
gst-inspect-1.0 --version | head -2

APPIMAGE_BUILD=/out/appimage
APPDIR=$APPIMAGE_BUILD/AppDir
rm -rf "$APPIMAGE_BUILD"
mkdir -p "$APPDIR/usr/bin" "$APPDIR/usr/share/applications" "$APPDIR/usr/share/metainfo"

install -Dm755 /root/target/release/rec-rs "$APPDIR/usr/bin/rec-rs"
ln -sf rec-rs "$APPDIR/usr/bin/rec"
install -Dm644 packaging/desktop/io.github.sultanaltair96.Rec.metainfo.xml \
    "$APPDIR/usr/share/metainfo/io.github.sultanaltair96.Rec.metainfo.xml"
# The AppImage desktop file is named after the binary, and its icon too, so
# desktop environments group the window and the launcher together. The actions
# are the same as in the Flatpak (upstream's Dock menu).
sed -e 's/^Icon=.*/Icon=rec-rs/' \
    packaging/desktop/io.github.sultanaltair96.Rec.desktop \
    >"$APPDIR/usr/share/applications/rec-rs.desktop"
for size in 128 256 512; do
    install -Dm644 "packaging/desktop/icons/hicolor/${size}x${size}/apps/io.github.sultanaltair96.Rec.png" \
        "$APPDIR/usr/share/icons/hicolor/${size}x${size}/apps/rec-rs.png"
done
install -Dm644 LICENSE-MIT "$APPDIR/usr/share/licenses/rec-rs/LICENSE-MIT"
install -Dm644 LICENSE-APACHE "$APPDIR/usr/share/licenses/rec-rs/LICENSE-APACHE"
cp "$APPDIR/usr/share/icons/hicolor/256x256/apps/rec-rs.png" "$APPDIR/.DirIcon"

echo "== tools"
TOOLS=/root/tools
mkdir -p "$TOOLS"
# Pinned releases (not 'continuous') so a rebuild uses the same linuxdeploy and
# appimagetool; the sha256s are checked before running them.
fetch() { # url sha256 output
    if [ ! -x "$3" ]; then
        curl -sSL -o "$3" "$1"
        echo "$2  $3" | sha256sum -c -
        chmod +x "$3"
    fi
}
export APPIMAGE_EXTRACT_AND_RUN=1
fetch "https://github.com/linuxdeploy/linuxdeploy/releases/download/1-alpha-20251107-1/linuxdeploy-x86_64.AppImage" \
      "c20cd71e3a4e3b80c3483cef793cda3f4e990aca14014d23c544ca3ce1270b4d" "$TOOLS/linuxdeploy"
fetch "https://github.com/AppImage/appimagetool/releases/download/1.9.1/appimagetool-x86_64.AppImage" \
      "ed4ce84f0d9caff66f50bcca6ff6f35aae54ce8135408b3fa33abfc3cb384eb0" "$TOOLS/appimagetool"

# --- the GStreamer half of the bundle -------------------------------------
# Every element the recorder or the probe can ask for, mapped to its plugin
# file. Missing ones are skipped (the decoder falls back to another encoder;
# openh264enc, fdkaacenc and vaapih264enc are not in Ubuntu's repositories).
ELEMENTS="appsrc appsink filesrc fakesink filesink queue capsfilter identity
 tee valve videoconvert videoscale videorate videotestsrc audioconvert
 audioresample audiorate volume level audioamplify audiomixer videocrop
 mp4mux qtmux qtdemux matroskamux h264parse h265parse aacparse opusparse
 avdec_h264 avdec_h265 avdec_aac avdec_prores avdec_vp9
 avenc_aac avenc_prores_ks avenc_h264 avenc_libx264 avenc_hevc avenc_libx265
 x264enc x265enc openh264enc fdkaacenc voaacenc opusenc opusdec
 vah264enc vah265enc vaapih264enc vaapih265enc
 vorbisenc vorbisdec theoraenc theoradec typefindfunctions uridecodebin
 decodebin parsebin multiqueue"
PLUGINS=$APPIMAGE_BUILD/plugins
mkdir -p "$PLUGINS"
for el in $ELEMENTS; do
    file=$(gst-inspect-1.0 "$el" 2>/dev/null | sed -n 's/^  Filename *//p' | head -1)
    [ -n "$file" ] && [ -f "$file" ] && cp -n "$file" "$PLUGINS/" || true
done
# Core plugins that are not named after an element but are needed:
# coreelements (queue/tee/filesink/capsfilter are there), playback, typefind.
for f in /usr/lib/x86_64-linux-gnu/gstreamer-1.0/libgstcoreelements.so \
         /usr/lib/x86_64-linux-gnu/gstreamer-1.0/libgstplayback.so \
         /usr/lib/x86_64-linux-gnu/gstreamer-1.0/libgsttypefindfunctions.so \
         /usr/lib/x86_64-linux-gnu/gstreamer-1.0/libgstvolume.so; do
    [ -f "$f" ] && cp -n "$f" "$PLUGINS/" || true
done
echo "  plugins: $(ls "$PLUGINS" | wc -l)"

echo "== linuxdeploy"
# --library for every plugin: linuxdeploy copies it and resolves the library
# dependencies it dlopens (libgst*, libx264, libx265, libavcodec, libva, ...).
# The host must provide libGL/libEGL/Vulkan drivers, libdbus and libpipewire,
# so those are excluded from the bundle; the closure pass below catches what
# linuxdeploy leaves out.
set --
for f in "$PLUGINS"/*.so; do
    set -- "$@" --library "$f"
done

# Libraries the app loads with dlopen, so they are not DT_NEEDED of anything
# and linuxdeploy cannot discover them: winit's X11 backend (x11-dl) opens the
# Xlib set by soname, xkbcommon-dl opens libxkbcommon, and the Wayland client
# stack is opened by smithay-client-toolkit. Without them the GUI exits with
# "Failed to load one of xlib's shared libraries".
for name in libX11.so.6 libX11-xcb.so.1 libXcursor.so.1 libXrandr.so.2 \
            libXi.so.6 libXinerama.so.1 libXxf86vm.so.1 libXfixes.so.3 \
            libxkbcommon.so.0 libxkbcommon-x11.so.0 \
            libwayland-client.so.0 libwayland-cursor.so.0 libwayland-egl.so.1; do
    lib=$(ls /usr/lib/x86_64-linux-gnu/"$name" 2>/dev/null | head -1)
    [ -n "$lib" ] && set -- "$@" --library "$lib"
done

"$TOOLS/linuxdeploy" --appdir "$APPDIR" \
    --executable "$APPDIR/usr/bin/rec-rs" \
    --desktop-file "$APPDIR/usr/share/applications/rec-rs.desktop" \
    --icon-file "$APPDIR/usr/share/icons/hicolor/256x256/apps/rec-rs.png" \
    --exclude-library 'libpipewire*' \
    --exclude-library 'libspa*' \
    --exclude-library 'libGL*' \
    --exclude-library 'libEGL*' \
    --exclude-library 'libdbus-1*' \
    "$@"

# linuxdeploy drops the plugins into usr/lib; GStreamer loads them from
# usr/lib/gstreamer-1.0 (GST_PLUGIN_SYSTEM_PATH_1_0 in AppRun), so move the
# plugin files there and leave their libraries in usr/lib.
mkdir -p "$APPDIR/usr/lib/gstreamer-1.0" "$APPDIR/usr/lib/gstreamer1.0"
for f in "$PLUGINS"/*.so; do
    name=$(basename "$f")
    if [ -f "$APPDIR/usr/lib/$name" ]; then
        mv "$APPDIR/usr/lib/$name" "$APPDIR/usr/lib/gstreamer-1.0/$name"
    fi
done
# linuxdeploy does not descend into every dlopened plugin and its excludelist
# assumes a desktop host has some libraries that a minimal host lacks
# (libharfbuzz for libass in gst-libav, libfontconfig/libfreetype for theora).
# A missing one silently disables a whole plugin — no HEVC decoder, no AAC, no
# ProRes — so close the dependency graph explicitly: every library the bundled
# binaries and plugins resolve on the build system is copied in unless it is on
# the documented host-provided list.
echo "== dependency closure"
# libva is *bundled* on purpose: libavcodec (and therefore gst-libav) links it
# directly, so without it a host that has no libva would lose AAC, ProRes and
# the HEVC decoder entirely. libva is a thin loader; the driver it dlopens comes
# from the host's /usr/lib/*/dri, which is the part that must match the GPU.
# The Vulkan *loader* is bundled (ffmpeg's libavcodec links it); the ICDs and
# drivers behind it still come from the host. Same for libva above.
HOST_PROVIDED='^(linux-vdso|ld-linux|libc\.so|libm\.so|libpthread|libdl\.so|librt\.so|libresolv|libnss_|libpipewire|libspa-|libGL|libGLX|libOpenGL|libEGL|libdbus-1)'
for pass in 1 2 3 4 5; do
    added=0
    for f in "$APPDIR/usr/bin/rec-rs" "$APPDIR"/usr/lib/*.so* \
             "$APPDIR"/usr/lib/gstreamer-1.0/*.so* "$APPDIR"/usr/libexec/gstreamer-1.0/*; do
        [ -f "$f" ] || continue
        for lib in $(ldd "$f" 2>/dev/null | awk '/=> \//{print $3}'); do
            case "$lib" in "$APPDIR"/*) continue ;; esac
            base=$(basename "$lib")
            if echo "$base" | grep -Eq "$HOST_PROVIDED"; then continue; fi
            [ -f "$APPDIR/usr/lib/$base" ] && continue
            cp -L "$lib" "$APPDIR/usr/lib/$base"
            added=$((added + 1))
        done
    done
    echo "  pass $pass: $added libraries"
    [ "$added" -eq 0 ] && break
done

# GStreamer's out-of-process plugin scanner; its location moved between
# distributions/GStreamer versions, so find it instead of hard-coding one path.
SCANNER=$(find /usr/lib /usr/libexec -type f -name 'gst-plugin-scanner*' 2>/dev/null | head -1)
[ -n "$SCANNER" ] || {
    echo "build-inside.sh: no gst-plugin-scanner found" >&2
    exit 1
}
echo "  plugin scanner: $SCANNER"
install -Dm755 "$SCANNER" "$APPDIR/usr/libexec/gstreamer-1.0/gst-plugin-scanner"

# linuxdeploy leaves AppRun and the root icon as symlinks to the executable and
# the hicolor icon. Our AppRun replaces theirs (the GStreamer environment above
# is the whole reason this AppImage exists); the icon stays a real file next to
# .DirIcon so the image also works unpacked.
rm -f "$APPDIR/AppRun" "$APPDIR/rec-rs.png" "$APPDIR/.DirIcon"
install -Dm755 packaging/appimage/AppRun "$APPDIR/AppRun"
install -Dm644 "$APPDIR/usr/share/icons/hicolor/256x256/apps/rec-rs.png" "$APPDIR/rec-rs.png"
ln -sf rec-rs.png "$APPDIR/.DirIcon"

echo "== appimagetool"
export ARCH=x86_64
UPDATE_INFO="gh-releases-zsync|sultanaltair96|rec-rs-linux|latest|Rec-*x86_64.AppImage.zsync"
# appimagetool writes the .zsync next to its *working directory*, not next to
# the output path, so run it in /out (never the repository).
cd /out
"$TOOLS/appimagetool" --updateinformation "$UPDATE_INFO" \
    "$APPDIR" "/out/Rec-${VERSION}-x86_64.AppImage"

echo "== bundle"
ls -l "/out/Rec-${VERSION}-x86_64.AppImage" "/out/Rec-${VERSION}-x86_64.AppImage.zsync"
du -h "/out/Rec-${VERSION}-x86_64.AppImage" | awk '{print "  size " $1}'
