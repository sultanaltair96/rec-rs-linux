//! Where the app runs from, and who updates it.
//!
//! Upstream moves itself into Applications when macOS translocates it and lets
//! Sparkle replace the bundle. Linux has neither: a Flatpak is updated by
//! Flatpak (so the app hides its own update check there), an AppImage is a
//! single self-contained file that can check GitHub Releases itself, and a
//! distro or `cargo` build just points at the release page. An AppImage also
//! offers to integrate itself (a `.desktop` entry and an icon) so it shows up
//! in the activities/menu like an installed app.
//!
//! Adapted from rec-rs src/app/install.rs (MIT OR Apache-2.0); the macOS
//! move/translocation/Sparkle machinery is replaced by Flatpak/AppImage
//! detection and desktop-entry integration. The tests that only meant
//! something for an .app bundle are marked not applicable in PARITY.md.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

/// Running inside a Flatpak sandbox (Flatpak updates the app itself).
pub fn is_flatpak() -> bool {
    flatpak_from(Path::new("/.flatpak-info").exists())
}

/// Pure half of [`is_flatpak`] (the marker file exists).
pub fn flatpak_from(marker_exists: bool) -> bool {
    marker_exists
}

/// `$APPIMAGE`, when it names the running file: then the app can integrate and
/// check for updates itself.
pub fn appimage_path() -> Option<PathBuf> {
    appimage_from(std::env::var_os("APPIMAGE").map(PathBuf::from))
}

fn appimage_from(v: Option<PathBuf>) -> Option<PathBuf> {
    v.filter(|p| p.is_file())
}

/// Whether "Check for Updates…" makes sense: Flatpak owns the update, so the
/// app hides it there and points at the software centre instead.
pub fn updates_self() -> bool {
    !is_flatpak()
}

/// The three files self-integration writes, all under `$HOME`.
#[derive(Clone, Debug, PartialEq)]
pub struct Integration {
    /// `~/.local/share/applications/rec-rs.desktop`
    pub desktop: PathBuf,
    /// `~/.local/share/icons/hicolor/scalable/apps/rec-rs.svg`
    pub icon: PathBuf,
    /// The desktop entry's text.
    pub entry: String,
    /// The icon's SVG.
    pub svg: String,
}

/// Where integration goes and what it contains for this AppImage.
pub fn integration(appimage: &Path, name: &str) -> Integration {
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::util::home_dir().join(".local/share"));
    integration_in(&data, appimage, name)
}

/// [`integration`] with the data home given (tests).
pub fn integration_in(data: &Path, appimage: &Path, name: &str) -> Integration {
    Integration {
        desktop: data.join("applications/rec-rs.desktop"),
        icon: data.join("icons/hicolor/scalable/apps/rec-rs.svg"),
        entry: desktop_entry(appimage, name),
        svg: icon_svg(),
    }
}

/// A freedesktop desktop entry that starts this AppImage with the app's
/// command line. `Exec` quotes the path the way the spec wants, so a path with
/// spaces or quotes still works.
pub fn desktop_entry(appimage: &Path, name: &str) -> String {
    let exec = exec_quote(&appimage.to_string_lossy());
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name={name}\n\
         GenericName=Screen Recorder\n\
         Comment=Record the screen, camera and audio\n\
         Exec={exec} gui\n\
         TryExec={path}\n\
         Icon=rec-rs\n\
         Terminal=false\n\
         Categories=AudioVideo;Recorder;\n\
         Keywords=record;screen;capture;camera;screencast;\n\
         StartupNotify=true\n\
         StartupWMClass=rec-rs\n",
        name = name,
        exec = exec,
        path = appimage.display(),
    )
}

/// Quote one `Exec` argument per the Desktop Entry spec: wrap in double
/// quotes, and escape `"`, `` ` ``, `$` and `\` inside.
pub fn exec_quote(arg: &str) -> String {
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('"');
    for c in arg.chars() {
        match c {
            '"' | '`' | '$' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            '%' => out.push_str("%%"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A small scalable icon: Rec's rounded square with a red record dot, in the
/// paper-rs greys. Written as text so integration needs no image assets.
pub fn icon_svg() -> String {
    "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"256\" height=\"256\" viewBox=\"0 0 256 256\">\
     <rect x=\"16\" y=\"16\" width=\"224\" height=\"224\" rx=\"52\" fill=\"#2b2b2d\"/>\
     <circle cx=\"128\" cy=\"128\" r=\"62\" fill=\"#ff3b30\"/>\
     </svg>\n"
        .to_string()
}

/// Write the desktop entry and icon, atomically, creating the folders.
///
/// Each file is written to a temporary name next to its destination and then
/// renamed over it, so a desktop reading the entry at the wrong moment never
/// finds a half-written or missing file (the same rule as upstream's bundle
/// install).
pub fn integrate(i: &Integration) -> Result<()> {
    write_atomic(&i.desktop, &i.entry)?;
    write_atomic(&i.icon, &i.svg)?;
    Ok(())
}

/// A temporary sibling of `dest`, unique to this process (two copies of Rec may
/// integrate at the same time), and the destination's directory.
fn staging(dest: &Path) -> Result<PathBuf> {
    let dir = dest
        .parent()
        .with_context(|| format!("{} has no folder", dest.display()))?;
    std::fs::create_dir_all(dir).with_context(|| format!("could not create {}", dir.display()))?;
    let name = dest
        .file_name()
        .with_context(|| format!("{} has no file name", dest.display()))?;
    let mut stage = std::ffi::OsString::from(".");
    stage.push(name);
    stage.push(format!(".rec-{}", std::process::id()));
    Ok(dir.join(stage))
}

/// Replace `dest` with `text` in one rename.
pub fn write_atomic(dest: &Path, text: &str) -> Result<()> {
    let stage = staging(dest)?;
    if let Err(e) = std::fs::write(&stage, text) {
        let _ = std::fs::remove_file(&stage);
        return Err(e).with_context(|| format!("could not write {}", stage.display()));
    }
    if let Err(e) = std::fs::rename(&stage, dest) {
        let _ = std::fs::remove_file(&stage);
        return Err(e).with_context(|| format!("could not move the file to {}", dest.display()));
    }
    Ok(())
}

/// Whether two paths name the same place, through symlinks or `..`
/// (file managers and shells report real paths).
#[cfg(test)]
pub fn same(a: &Path, b: &Path) -> bool {
    let real = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    real(a) == real(b)
}

/// Remove integration if it points at a different AppImage (an older download),
/// so the menu entry never starts a stale file. Returns whether it removed one.
pub fn remove_stale(i: &Integration, appimage: &Path) -> Result<bool> {
    let Ok(text) = std::fs::read_to_string(&i.desktop) else {
        return Ok(false);
    };
    if text.contains(&format!("TryExec={}\n", appimage.display())) {
        return Ok(false);
    }
    let _ = std::fs::remove_file(&i.desktop);
    let _ = std::fs::remove_file(&i.icon);
    Ok(true)
}

/// Ask for integration only when the AppImage isn't integrated yet (a menu
/// entry for this exact file). `REC_INTEGRATE=1` does it without asking,
/// `REC_INTEGRATE=0` never does (self-tests and packagers use it).
pub fn should_offer_integration(appimage: &Path, home_data: &Path) -> bool {
    match std::env::var("REC_INTEGRATE").ok().as_deref() {
        Some("1") => true,
        Some("0") => false,
        _ => {
            let i = integration_in(home_data, appimage, "Rec");
            std::fs::read_to_string(&i.desktop)
                .map(|t| !t.contains(&format!("TryExec={}\n", appimage.display())))
                .unwrap_or(true)
        }
    }
}

/// Check that an AppImage path is usable for integration (exists, is a file).
pub fn check_appimage(p: &Path) -> Result<()> {
    if !p.is_file() {
        bail!("{} is not a file", p.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rec-install-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn spots_flatpak_and_appimage() {
        // The marker file decides, so the test doesn't depend on the sandbox.
        assert!(flatpak_from(true));
        assert!(!flatpak_from(false));
        let d = temp_dir("appimage");
        let f = d.join("Rec.AppImage");
        std::fs::write(&f, "elf").unwrap();
        assert_eq!(appimage_from(Some(f.clone())), Some(f));
        assert_eq!(appimage_from(Some(d.join("gone"))), None, "must exist");
        assert_eq!(appimage_from(None), None);
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn updates_are_flatpak_managed() {
        // Flatpak updates the app through the store; AppImage and source builds
        // check GitHub Releases themselves.
        assert_eq!(updates_self(), !is_flatpak());
        assert!(flatpak_from(true));
    }

    #[test]
    fn desktop_entry_starts_this_appimage() {
        let e = desktop_entry(Path::new("/home/x/Apps/Rec-1.0.AppImage"), "Rec");
        assert!(e.starts_with("[Desktop Entry]\n"));
        assert!(
            e.contains("Exec=\"/home/x/Apps/Rec-1.0.AppImage\" gui\n"),
            "{e}"
        );
        assert!(e.contains("TryExec=/home/x/Apps/Rec-1.0.AppImage\n"), "{e}");
        assert!(e.contains("Icon=rec-rs\n"));
        assert!(e.contains("Categories=AudioVideo;Recorder;\n"));
        // Every line is a key or a section, never a stray blank inside.
        assert!(e.lines().all(|l| l.starts_with('[') || l.contains('=')));
    }

    #[test]
    fn exec_quoting_follows_the_spec() {
        assert_eq!(exec_quote("/plain/path"), "\"/plain/path\"");
        assert_eq!(
            exec_quote("/with space/Rec.AppImage"),
            "\"/with space/Rec.AppImage\""
        );
        // Quotes, backticks, dollars and backslashes are escaped; % is %%.
        assert_eq!(exec_quote("a\"b`c$d\\e%f"), "\"a\\\"b\\`c\\$d\\\\e%%f\"");
        let e = desktop_entry(Path::new("/tmp/Re c's App.AppImage"), "Rec");
        assert!(
            e.contains("Exec=\"/tmp/Re c's App.AppImage\" gui"),
            "single quotes need no escaping: {e}"
        );
    }

    #[test]
    fn installs_the_desktop_entry_and_icon() {
        let d = temp_dir("integrate");
        let app = d.join("Rec.AppImage");
        std::fs::write(&app, "elf").unwrap();
        let i = integration_in(&d.join("data"), &app, "Rec");
        integrate(&i).unwrap();
        assert!(i.desktop.is_file() && i.icon.is_file());
        assert_eq!(std::fs::read_to_string(&i.desktop).unwrap(), i.entry);
        assert!(i.svg.contains("<svg") && i.svg.contains("#ff3b30"));
        // No temporary files left behind.
        let leftovers: Vec<_> = std::fs::read_dir(i.desktop.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with('.'))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn replaces_an_older_entry() {
        let d = temp_dir("replace");
        let (old, new) = (d.join("Rec-1.AppImage"), d.join("Rec-2.AppImage"));
        std::fs::write(&old, "elf").unwrap();
        std::fs::write(&new, "elf").unwrap();
        let data = d.join("data");
        let a = integration_in(&data, &old, "Rec");
        integrate(&a).unwrap();
        // The new AppImage replaces the entry, atomically.
        let b = integration_in(&data, &new, "Rec");
        integrate(&b).unwrap();
        let text = std::fs::read_to_string(&b.desktop).unwrap();
        assert!(text.contains("Rec-2.AppImage"));
        assert!(!text.contains("Rec-1.AppImage"));
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn never_leaves_the_destination_missing() {
        // A desktop reading the entry while it is rewritten must always find a
        // complete one: written to a sibling, then renamed over it.
        let d = temp_dir("atomic");
        let dest = d.join("rec-rs.desktop");
        write_atomic(&dest, "one\n").unwrap();
        let seen = std::cell::RefCell::new(vec![]);
        // Read the file the way a desktop would, right before and after.
        seen.borrow_mut()
            .push(std::fs::read_to_string(&dest).unwrap());
        write_atomic(&dest, "two\n").unwrap();
        seen.borrow_mut()
            .push(std::fs::read_to_string(&dest).unwrap());
        assert_eq!(*seen.borrow(), ["one\n", "two\n"]);
        assert_eq!(std::fs::read_dir(&d).unwrap().count(), 1, "no staging left");
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn staging_is_apart_from_other_copies() {
        let d = temp_dir("staging");
        let foreign = d.join(".rec-rs.desktop.rec-99999");
        std::fs::write(&foreign, "another copy's staging").unwrap();
        write_atomic(&d.join("rec-rs.desktop"), "mine\n").unwrap();
        assert!(foreign.is_file(), "another copy's staging is left alone");
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn compares_real_paths() {
        let d = temp_dir("symlink");
        std::fs::create_dir_all(d.join("real")).unwrap();
        std::fs::write(d.join("real/rec-rs.desktop"), "x").unwrap();
        std::os::unix::fs::symlink(d.join("real"), d.join("link")).unwrap();
        assert!(same(
            &d.join("real/rec-rs.desktop"),
            &d.join("link/rec-rs.desktop")
        ));
        assert!(!same(
            &d.join("real/rec-rs.desktop"),
            &d.join("real/other.desktop")
        ));
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn removes_a_stale_entry_for_another_appimage() {
        let d = temp_dir("stale");
        let (old, new) = (d.join("Rec-1.AppImage"), d.join("Rec-2.AppImage"));
        std::fs::write(&old, "elf").unwrap();
        std::fs::write(&new, "elf").unwrap();
        let data = d.join("data");
        let a = integration_in(&data, &old, "Rec");
        integrate(&a).unwrap();
        assert!(!remove_stale(&a, &old).unwrap(), "this file is current");
        assert!(remove_stale(&a, &new).unwrap(), "an older entry goes");
        assert!(!a.desktop.exists() && !a.icon.exists());
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn offers_integration_once() {
        let d = temp_dir("offer");
        let app = d.join("Rec.AppImage");
        std::fs::write(&app, "elf").unwrap();
        let data = d.join("data");
        assert!(should_offer_integration(&app, &data), "not integrated yet");
        integrate(&integration_in(&data, &app, "Rec")).unwrap();
        assert!(!should_offer_integration(&app, &data), "already integrated");
        std::fs::remove_dir_all(&d).unwrap();
    }
}
