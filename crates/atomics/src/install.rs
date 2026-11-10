//! Putting the running program where it belongs. A downloaded ATOMICS runs from wherever it was unpacked: this gives it a
//! home, with what makes it open from the applications menu, through the same steps an update takes. Config and state
//! are no part of it. A program a build or a package manager put somewhere stays there.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, anyhow, bail};

use crate::update::{InstallTarget, STAGING};

const ICON_SVG: &[u8] = include_bytes!("../../../assets/icon.svg");
const ICON_PNG: &[u8] = include_bytes!("../../../assets/icon.png");
/// What builds from before the rename to ATOMICS installed, which an install clears away.
const OLD_NAME: &str = "imacs";
/// Folders that belong to the system or a package manager.
const MANAGED: [&str; 7] = ["/usr/", "/nix/", "/opt/", "/snap/", "/bin/", "/sbin/", "/var/lib/flatpak/"];

/// Where a program runs from, as far as installing goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Placement {
    /// Where it is installed.
    Installed,
    /// Somewhere like a downloads folder, and `to` is where it would go.
    Elsewhere { to: PathBuf },
    /// A build, or what a package manager put there: it stays.
    Managed,
}

pub(crate) fn placement(executable: &Path, home: &Path, macos: bool) -> Placement {
    let resolved = executable.canonicalize().unwrap_or_else(|_| executable.to_path_buf());
    placement_with_system_bundle(&resolved, home, macos, Path::new("/Applications/ATOMICS.app").exists())
}

fn placement_with_system_bundle(resolved: &Path, home: &Path, macos: bool, system_bundle: bool) -> Placement {
    if macos {
        let Some(bundle) = crate::restart::bundle_of(resolved) else { return Placement::Managed };
        let folder = bundle.parent();
        if folder == Some(&home.join("Applications")) || folder == Some(Path::new("/Applications")) {
            return Placement::Installed;
        }
        return Placement::Elsewhere { to: bundle_destination(home, system_bundle) };
    }
    let bin = home.join(".local/bin");
    if resolved.parent().is_some_and(|folder| folder == bin || bin.canonicalize().is_ok_and(|bin| bin == folder)) {
        return Placement::Installed;
    }
    let built = resolved.ancestors().any(|dir| dir.file_name().is_some_and(|name| name == "target"))
        && resolved.components().any(|part| part.as_os_str() == "debug" || part.as_os_str() == "release");
    let text = resolved.to_string_lossy();
    if built || resolved.starts_with(home.join(".cargo")) || MANAGED.iter().any(|folder| text.starts_with(folder)) {
        return Placement::Managed;
    }
    Placement::Elsewhere { to: bin.join("atomics") }
}

/// A person's own Applications folder, needing no administrator, unless ATOMICS already lives in the system's.
fn bundle_home(home: &Path) -> PathBuf {
    bundle_destination(home, Path::new("/Applications/ATOMICS.app").exists())
}

fn bundle_destination(home: &Path, system_bundle: bool) -> PathBuf {
    if system_bundle { "/Applications/ATOMICS.app".into() } else { home.join("Applications/ATOMICS.app") }
}

/// The program inside what `to` names.
pub(crate) fn program_at(to: &Path, macos: bool) -> PathBuf {
    if macos { to.join("Contents/MacOS/atomics") } else { to.to_path_buf() }
}

/// What installing did, for whoever asked.
pub(crate) struct Installed {
    pub(crate) at: PathBuf,
    /// The line that puts `atomics` on PATH for a terminal, if it is not there yet.
    pub(crate) path_line: Option<String>,
}

/// Where this running program would go, if anywhere.
pub(crate) fn offer() -> Option<(PathBuf, PathBuf)> {
    let executable = crate::setup::executable_path().ok()?;
    match placement(&executable, &atomics_core::paths::home(), cfg!(target_os = "macos")) {
        Placement::Elsewhere { to } => Some((executable, to)),
        _ => None,
    }
}

/// Installs the running program for this user.
pub(crate) fn install_self() -> Result<Installed> {
    let executable = crate::setup::executable_path().context("locating this program")?;
    let home = atomics_core::paths::home();
    let path = std::env::var("PATH").unwrap_or_default();
    if cfg!(target_os = "macos") {
        let resolved = executable.canonicalize().unwrap_or(executable);
        let bundle = crate::restart::bundle_of(&resolved)
            .ok_or_else(|| anyhow!("ATOMICS is not running from an app bundle, so there is nothing to install."))?;
        return install_bundle(bundle, &bundle_home(&home), &home, &path);
    }
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| home.join(".local/share"));
    install_binary(&executable, &home, &data, &path)
}

fn install_binary(source: &Path, home: &Path, data: &Path, path: &str) -> Result<Installed> {
    let bin = home.join(".local/bin");
    let target = bin.join("atomics");
    std::fs::create_dir_all(&bin).with_context(|| format!("Could not write to {}", bin.display()))?;
    // Running from the installed file itself only repairs what goes with it.
    let same = source.canonicalize().ok().is_some_and(|source| target.canonicalize().is_ok_and(|to| to == source));
    if !same {
        let staging = tempfile::Builder::new()
            .prefix(STAGING)
            .tempdir_in(&bin)
            .with_context(|| format!("Could not write to {}", bin.display()))?;
        let new = staging.path().join("atomics");
        std::fs::copy(source, &new).with_context(|| format!("Could not copy {}", source.display()))?;
        crate::update::place(&InstallTarget::Binary(target.clone()), &new, staging.path())?;
    }
    desktop_integration(&target, data)?;
    Ok(Installed { at: target, path_line: path_line(&bin, path) })
}

fn install_bundle(source: &Path, to: &Path, home: &Path, path: &str) -> Result<Installed> {
    let folder = to.parent().context("the install has no folder")?;
    std::fs::create_dir_all(folder).with_context(|| format!("Could not write to {}", folder.display()))?;
    if source.canonicalize().ok() != to.canonicalize().ok() {
        let staging = tempfile::Builder::new()
            .prefix(STAGING)
            .tempdir_in(folder)
            .with_context(|| format!("Could not write to {}", folder.display()))?;
        let new = staging.path().join("ATOMICS.app");
        // `-R` keeps what a bundle is made of, its signature and the links inside it among them.
        let status = Command::new("cp")
            .arg("-R")
            .arg(source)
            .arg(&new)
            .stdin(Stdio::null())
            .status()
            .context("Could not run cp")?;
        if !status.success() {
            bail!("Could not copy {}", source.display());
        }
        crate::update::place(&InstallTarget::Bundle(to.to_path_buf()), &new, staging.path())?;
    }
    let link = link_on_path(&program_at(to, true), home, path)?;
    Ok(Installed { at: to.to_path_buf(), path_line: path_line(link.parent().context("the link has no folder")?, path) })
}

/// `atomics` for a terminal: in a folder on PATH that this user can write to, else their own, which then needs a line.
fn link_on_path(program: &Path, home: &Path, path: &str) -> Result<PathBuf> {
    let shared = ["/opt/homebrew/bin", "/usr/local/bin"].map(PathBuf::from);
    let own = home.join(".local/bin");
    let candidates = shared.iter().filter(|folder| on_path(folder, path)).chain(std::iter::once(&own));
    let mut last = None;
    for folder in candidates {
        let link = folder.join("atomics");
        let made = std::fs::create_dir_all(folder).and_then(|()| {
            let staged = folder.join(format!("{STAGING}link-{}", std::process::id()));
            let _ = std::fs::remove_file(&staged);
            std::os::unix::fs::symlink(program, &staged)?;
            std::fs::rename(&staged, &link)
        });
        match made {
            Ok(()) => return Ok(link),
            Err(error) => last = Some(error),
        }
    }
    Err(last.map_or_else(|| anyhow!("no folder to put atomics in"), anyhow::Error::from))
        .context("Could not put atomics on PATH")
}

fn on_path(folder: &Path, path: &str) -> bool {
    std::env::split_paths(path).any(|known| known == folder)
}

fn path_line(folder: &Path, path: &str) -> Option<String> {
    (!on_path(folder, path)).then(|| format!("export PATH=\"{}:$PATH\"", folder.display()))
}

/// A desktop entry's `Exec` has its own quoting rules, including `%%` for a literal `%`.
fn exec_quoted(program: &Path) -> String {
    let text = program.to_string_lossy();
    let inner = text.replace('\\', "\\\\").replace('"', "\\\"").replace('$', "\\$").replace('`', "\\`");
    inner.replace('%', "%%").replace('\\', "\\\\")
}

/// The menu entry `scripts/linux-install.sh` writes too; a test holds the two together.
fn desktop_entry(program: &Path) -> String {
    format!(
        "[Desktop Entry]\nType=Application\nName=ATOMICS\nComment=Review-first workspace for coding agents\nExec=\"{}\" \
         %u\nIcon=atomics\nTerminal=false\nCategories=Development;IDE;\nStartupNotify=true\nStartupWMClass=atomics\n\
         MimeType=x-scheme-handler/atomics;\n",
        exec_quoted(program)
    )
}

/// The menu entry, the icons, and `atomics://` links. What a desktop lacks the tools for does without: none of it is
/// worth failing an install over.
fn desktop_integration(program: &Path, data: &Path) -> Result<()> {
    let applications = data.join("applications");
    let scalable = data.join("icons/hicolor/scalable/apps");
    let sized = data.join("icons/hicolor/512x512/apps");
    let write = |folder: &Path, name: String, bytes: &[u8]| -> Result<()> {
        std::fs::create_dir_all(folder).with_context(|| format!("Could not write to {}", folder.display()))?;
        std::fs::write(folder.join(name), bytes).with_context(|| format!("Could not write to {}", folder.display()))
    };
    write(&applications, "atomics.desktop".into(), desktop_entry(program).as_bytes())?;
    write(&scalable, "atomics.svg".into(), ICON_SVG)?;
    write(&sized, "atomics.png".into(), ICON_PNG)?;
    let bin = program.parent().unwrap_or(Path::new("."));
    let old = [
        bin.join(OLD_NAME),
        applications.join(format!("{OLD_NAME}.desktop")),
        scalable.join(format!("{OLD_NAME}.svg")),
        sized.join(format!("{OLD_NAME}.png")),
    ];
    for file in old {
        let _ = std::fs::remove_file(file);
    }
    let run = |program: &str, args: &[&std::ffi::OsStr]| {
        let _ =
            Command::new(program).args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status();
    };
    run("xdg-mime", &["default".as_ref(), "atomics.desktop".as_ref(), "x-scheme-handler/atomics".as_ref()]);
    run("update-desktop-database", &[applications.as_os_str()]);
    let icons = data.join("icons/hicolor");
    if icons.join("index.theme").exists() {
        run("gtk-update-icon-cache", &["-f".as_ref(), "-t".as_ref(), icons.as_os_str()]);
    }
    Ok(())
}

/// `atomics install`.
pub fn run_cli() -> i32 {
    match install_self() {
        Ok(installed) => {
            println!("Installed ATOMICS {} at {}.", env!("CARGO_PKG_VERSION"), installed.at.display());
            println!("Open it from your applications menu. Its first run opens a setup screen.");
            if cfg!(target_os = "linux") && crate::secret_copy::unavailable().is_some() {
                println!("To enable authenticated secret Copy, run: sudo atomics install --secret-copy-policy");
            }
            if let Some(line) = installed.path_line {
                println!("To run atomics from a terminal too, add this to your shell's startup file: {line}");
            }
            0
        }
        Err(error) => {
            eprintln!("atomics install: {error:#}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_download_offers_to_install_and_a_build_or_a_package_does_not() {
        let home = Path::new("/home/someone");
        let linux = |path: &str| placement_with_system_bundle(Path::new(path), home, false, false);
        let to = home.join(".local/bin/atomics");
        assert_eq!(linux("/home/someone/Downloads/atomics-0.1.7/atomics"), Placement::Elsewhere { to: to.clone() });
        assert_eq!(linux("/tmp/x/atomics"), Placement::Elsewhere { to: to.clone() });
        assert_eq!(linux("/home/someone/.local/bin/atomics"), Placement::Installed);
        assert_eq!(linux("/home/someone/.local/bin/atomics.previous"), Placement::Installed);
        for managed in [
            "/home/someone/code/atomics/target/debug/atomics",
            "/target/release/atomics",
            "/home/someone/code/target/x86_64-unknown-linux-gnu/release/atomics",
            "/home/someone/.cargo/bin/atomics",
            "/usr/bin/atomics",
            "/nix/store/abc-atomics/bin/atomics",
            "/opt/atomics/atomics",
        ] {
            assert_eq!(linux(managed), Placement::Managed, "{managed}");
        }
        let mac = |path: &str| placement_with_system_bundle(Path::new(path), home, true, false);
        let to = home.join("Applications/ATOMICS.app");
        assert_eq!(mac("/home/someone/Downloads/ATOMICS.app/Contents/MacOS/atomics"), Placement::Elsewhere { to });
        assert_eq!(mac("/home/someone/Applications/ATOMICS.app/Contents/MacOS/atomics"), Placement::Installed);
        assert_eq!(mac("/Applications/ATOMICS.app/Contents/MacOS/atomics"), Placement::Installed);
        assert_eq!(mac("/home/someone/code/target/debug/atomics"), Placement::Managed, "not a bundle");
        assert_eq!(mac("/home/someone/work.app/target/debug/atomics"), Placement::Managed, "only its own program");
        assert_eq!(
            placement_with_system_bundle(Path::new("/tmp/ATOMICS.app/Contents/MacOS/atomics"), home, true, true),
            Placement::Elsewhere { to: "/Applications/ATOMICS.app".into() }
        );
    }

    #[test]
    fn placement_resolves_a_link_to_the_users_installed_program() {
        let dir = crate::test_support::tempdir().unwrap();
        let bin = dir.path().join(".local/bin");
        std::fs::create_dir_all(&bin).unwrap();
        crate::test_support::write_program(bin.join("atomics"), "#!/bin/sh\nexit 0\n");
        let link = dir.path().join("atomics-link");
        std::os::unix::fs::symlink(bin.join("atomics"), &link).unwrap();
        assert_eq!(placement(&link, dir.path(), false), Placement::Installed);
    }

    #[test]
    fn a_program_installs_itself_with_its_menu_entry_and_keeps_what_was_there() {
        let dir = crate::test_support::tempdir().unwrap();
        let home = dir.path().join("home");
        let data = home.join("data");
        let source = dir.path().join("Downloads/atomics");
        std::fs::create_dir_all(source.parent().unwrap()).unwrap();
        let new = "#!/bin/sh\necho 'atomics 2.0.0 (v2)'\n";
        crate::test_support::write_program(&source, new);
        let bin = home.join(".local/bin");
        std::fs::create_dir_all(&bin).unwrap();
        let old = "#!/bin/sh\necho 'atomics 1.0.0 (v1)'\n";
        crate::test_support::write_program(bin.join("atomics"), old);
        std::fs::write(bin.join(OLD_NAME), "from before the rename").unwrap();

        let installed = install_binary(&source, &home, &data, "/usr/bin").unwrap();
        assert_eq!(installed.at, bin.join("atomics"));
        assert_eq!(std::fs::read_to_string(bin.join("atomics")).unwrap(), new);
        assert_eq!(std::fs::read_to_string(bin.join("atomics.previous")).unwrap(), old, "the way back");
        assert!(!bin.join(OLD_NAME).exists());
        assert_eq!(installed.path_line, Some(format!("export PATH=\"{}:$PATH\"", bin.display())));
        assert!(std::fs::read_to_string(data.join("applications/atomics.desktop")).unwrap().contains("Icon=atomics"));
        assert!(data.join("icons/hicolor/scalable/apps/atomics.svg").is_file());
        assert!(data.join("icons/hicolor/512x512/apps/atomics.png").is_file());
        let names: Vec<_> = std::fs::read_dir(&bin).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names.len(), 2, "nothing else is left beside it: {names:?}");

        // Already on PATH, it says nothing of it; from the installed file it repairs the rest and changes no program.
        let on_path = format!("/usr/bin:{}", bin.display());
        std::fs::remove_file(data.join("applications/atomics.desktop")).unwrap();
        let again = install_binary(&bin.join("atomics"), &home, &data, &on_path).unwrap();
        assert_eq!(again.path_line, None);
        assert!(data.join("applications/atomics.desktop").is_file());
        assert_eq!(std::fs::read_to_string(bin.join("atomics.previous")).unwrap(), old);

        // A program that does not run here replaces nothing.
        let broken = dir.path().join("broken");
        crate::test_support::write_program(&broken, "#!/bin/sh\nexit 1\n");
        assert!(install_binary(&broken, &home, &data, "").is_err());
        assert_eq!(std::fs::read_to_string(bin.join("atomics")).unwrap(), new);
    }

    #[test]
    fn a_bundle_installs_whole_with_atomics_on_path_and_keeps_what_was_there() {
        let dir = crate::test_support::tempdir().unwrap();
        let home = dir.path().join("home");
        let bundle = |folder: &Path, text: &str| {
            let program = folder.join("ATOMICS.app/Contents/MacOS/atomics");
            std::fs::create_dir_all(program.parent().unwrap()).unwrap();
            crate::test_support::write_program(&program, text);
            folder.join("ATOMICS.app")
        };
        let new = "#!/bin/sh\necho 'atomics 2.0.0 (v2)'\n";
        let source = bundle(&dir.path().join("Downloads"), new);
        let installed_at = bundle(&home.join("Applications"), "#!/bin/sh\necho 'atomics 1.0.0 (v1)'\n");

        let installed = install_bundle(&source, &installed_at, &home, "/usr/bin").unwrap();
        assert_eq!(installed.at, installed_at);
        let program = program_at(&installed_at, true);
        assert_eq!(std::fs::read_to_string(&program).unwrap(), new);
        let previous = home.join("Applications/.atomics-previous/Contents/MacOS/atomics");
        assert!(std::fs::read_to_string(previous).unwrap().contains("1.0.0"), "the way back");
        let link = home.join(".local/bin/atomics");
        assert_eq!(std::fs::read_link(&link).unwrap(), program);
        assert!(installed.path_line.unwrap().contains(".local/bin"));
    }

    #[test]
    fn a_menu_entry_quotes_a_path_by_its_own_rules() {
        assert_eq!(exec_quoted(Path::new("/home/a b/atomics")), "/home/a b/atomics");
        assert_eq!(exec_quoted(Path::new("/h/50%/atomics")), "/h/50%%/atomics");
        assert_eq!(exec_quoted(Path::new("/h/$x`y\"z/a")), "/h/\\\\$x\\\\`y\\\\\"z/a");
    }
}
