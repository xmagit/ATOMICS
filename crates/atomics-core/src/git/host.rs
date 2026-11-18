//! Process policy shared by repository operations and configuration discovery.
use super::*;
use std::io::Write;
use std::sync::OnceLock;

/// Never inherit an index, config injection, alternate object store or a helper chosen by
/// the invoking environment. Identity and deterministic dates remain the caller's.
pub(super) fn base() -> Command {
    process(false)
}

pub(super) fn process(registered: bool) -> Command {
    let mut cmd = Command::new("git");
    for (key, _) in std::env::vars_os() {
        let name = key.to_string_lossy();
        if name.starts_with("GIT_") && !name.starts_with("GIT_AUTHOR_") && !name.starts_with("GIT_COMMITTER_") {
            cmd.env_remove(key);
        }
    }
    cmd.current_dir("/")
        .stdin(Stdio::null())
        .arg("--no-pager")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_EDITOR", "false")
        .env("GIT_SEQUENCE_EDITOR", "false");
    for setting in [
        "core.quotepath=off",
        "core.hooksPath=/dev/null",
        "core.fsmonitor=false",
        "submodule.recurse=false",
        "fetch.recurseSubmodules=false",
        "status.submoduleSummary=false",
        "diff.submodule=short",
        "gc.auto=0",
        "maintenance.auto=false",
    ] {
        cmd.args(["-c", setting]);
    }
    if !registered {
        cmd.env("GIT_ALLOW_PROTOCOL", "https:http:ssh:git:file").env("GIT_ATTR_NOSYSTEM", "1");
        for setting in [
            "core.attributesFile=/dev/null",
            "core.untrackedCache=false",
            "core.splitIndex=false",
            "commit.gpgSign=false",
            "tag.gpgSign=false",
        ] {
            cmd.args(["-c", setting]);
        }
    }
    cmd
}

type Settings = Vec<(String, String)>;

fn settings(raw: &str) -> Settings {
    raw.split_terminator('\0')
        .map(|entry| {
            let (key, value) = entry.split_once('\n').unwrap_or((entry, "true"));
            (key.to_string(), value.to_string())
        })
        .collect()
}

/// Parse the exact regular-file bytes with Git's parser in a neutral directory. In
/// particular, never let a config include select another file during parsing.
fn read_settings(path: &Path) -> Result<Arc<Settings>> {
    parse_settings(read_regular_file(path, 256 * 1024)?)
}

fn parse_settings(bytes: String) -> Result<Arc<Settings>> {
    type Cache = Vec<(String, Arc<Settings>)>;
    static CACHE: Mutex<Cache> = Mutex::new(Vec::new());
    if let Some((_, settings)) = CACHE.lock().unwrap().iter().find(|(old, _)| old == &bytes) {
        return Ok(settings.clone());
    }
    let mut cmd = base();
    let args = ["config", "--no-includes", "--file", "-", "--null", "--list"];
    cmd.args(args);
    let parsed = Arc::new(settings(&run(cmd, &args, Some(bytes.as_bytes()), quick())?));
    let mut cache = CACHE.lock().unwrap();
    if cache.len() >= 16 {
        cache.remove(0);
    }
    cache.push((bytes, parsed.clone()));
    Ok(parsed)
}

/// Preserve host identity, default branch and ignore file at global precedence. Taking a
/// snapshot outside the checkout keeps conditional includes and executable drivers out.
fn global_config() -> Result<&'static [u8]> {
    static GLOBAL: OnceLock<Vec<u8>> = OnceLock::new();
    if let Some(file) = GLOBAL.get() {
        return Ok(file);
    }
    let mut cmd = base();
    cmd.env_remove("GIT_CONFIG_GLOBAL");
    if let Some(file) = std::env::var_os("GIT_CONFIG_GLOBAL") {
        cmd.env("GIT_CONFIG_GLOBAL", file);
    }
    let args = [
        "config",
        "--global",
        "--includes",
        "--null",
        "--get-regexp",
        r"^(user\.(name|email)|init\.defaultbranch|core\.excludesfile)$",
    ];
    cmd.args(args);
    let result = output(cmd, &args, None, quick())?;
    ensure!(matches!(result.status.code(), Some(0 | 1)), "cannot read global Git identity");
    let mut file = Vec::new();
    for (key, value) in settings(std::str::from_utf8(&result.stdout)?) {
        let (section, name) = key.split_once('.').context("invalid global key")?;
        let value = value.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n").replace('\t', "\\t");
        writeln!(file, "[{section}]\n{name} = \"{value}\"")?;
    }
    Ok(GLOBAL.get_or_init(|| file))
}

fn write_settings(mut config: impl Write, settings: &[(String, String)]) -> Result<()> {
    let quote = |s: &str| {
        s.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n").replace('\t', "\\t").replace('\r', "")
    };
    for (key, value) in settings {
        let (section, rest) = key.split_once('.').context("invalid Git setting")?;
        if let Some((subsection, name)) = rest.rsplit_once('.') {
            writeln!(config, "[{section} \"{}\"]\n{name} = \"{}\"", quote(subsection), quote(value))?;
        } else {
            writeln!(config, "[{section}]\n{rest} = \"{}\"", quote(value))?;
        }
    }
    Ok(())
}

pub(super) struct Config {
    raw: String,
    effective: Settings,
    native: bool,
}

fn is_include(key: &str) -> bool {
    key == "include.path" || (key.starts_with("includeif.") && key.ends_with(".path"))
}

/// Check both the named path and its target. Includes cannot transfer authority to
/// a checkout, an object store, or a task's writable worktree administration.
fn protected(path: &Path, common: &Path, worktree: &Path) -> bool {
    let checked = |p: &Path| {
        let in_common = crate::paths::at_or_below(p, common);
        let in_checkout = crate::paths::at_or_below(p, worktree)
            || common.parent().is_some_and(|root| crate::paths::at_or_below(p, root));
        (!in_checkout || in_common)
            && !in_writable_git_folder(p)
            && !sandbox_writable(p)
            && crate::workspace::untrusted(p).is_none()
    };
    checked(path) && crate::paths::resolved_path(path).is_ok_and(|real| checked(&real))
}

fn include_file(source: &Path, value: &str, common: &Path, worktree: &Path, depth: usize) -> Option<PathBuf> {
    use std::os::unix::fs::MetadataExt;
    let expanded = crate::paths::expand(value);
    if depth >= 10 || value.starts_with('%') || (value.starts_with('~') && !expanded.is_absolute()) {
        return None;
    }
    let path = source.parent()?.join(expanded);
    if !protected(&path, common, worktree) {
        return None;
    }
    let metadata = path.symlink_metadata().ok()?;
    if !metadata.is_file() || metadata.nlink() != 1 {
        return None;
    }
    let nested = read_settings(&path).ok()?;
    nested
        .iter()
        .all(|(key, value)| {
            !matches!(key.as_str(), "extensions.worktreeconfig" | "core.worktree")
                && (!is_include(key) || include_file(&path, value, common, worktree, depth + 1).is_some())
        })
        .then_some(path)
}

pub(super) fn configure(common: &Path, git_dir: &Path, worktree: &Path, registered: bool) -> Result<Config> {
    let source = common.join("config");
    let raw = read_regular_file(&source, 256 * 1024)?;
    let mut local = Vec::new();
    let mut native = registered;
    for (key, value) in parse_settings(raw.clone())?.iter() {
        if key == "core.worktree" {
            native = false;
            continue;
        }
        if key == "extensions.worktreeconfig" {
            // Git reads this option during repository discovery, before -c overrides.
            // A private common directory keeps config.worktree out of operational Git.
            if !matches!(value.to_ascii_lowercase().as_str(), "false" | "no" | "off" | "0") {
                native = false;
            }
            continue;
        }
        if is_include(key) {
            if registered && let Some(path) = include_file(&source, value, common, worktree, 0) {
                local.push((key.clone(), path.to_string_lossy().into_owned()));
                continue;
            }
            native = false;
            // In the restricted view, consume only the two data fields from the
            // managed identity file. Never let Git follow a foreign include.
            let path = Path::new(value);
            let identities = common.join("atomics-identities");
            let pattern = crate::identity::worktree_include_key(git_dir).replacen("includeIf", "includeif", 1);
            if *key == pattern
                && path.parent() == Some(identities.as_path())
                && identities.symlink_metadata().is_ok_and(|m| m.is_dir())
            {
                local.extend(
                    read_settings(path)?
                        .iter()
                        .filter(|(k, _)| matches!(k.as_str(), "user.name" | "user.email"))
                        .cloned(),
                );
            }
            continue;
        }
        // Unknown options are data in foreign folders. Pass only settings needed
        // for the repository's format, identity, refs and remotes to the private view.
        let keep = registered
            || matches!(
                key.as_str(),
                "core.repositoryformatversion"
                    | "core.filemode"
                    | "core.bare"
                    | "core.logallrefupdates"
                    | "core.ignorecase"
                    | "core.precomposeunicode"
                    | "core.symlinks"
                    | "core.autocrlf"
                    | "core.eol"
                    | "core.safecrlf"
                    | "user.name"
                    | "user.email"
                    | "branch.autosetupmerge"
                    | "extensions.objectformat"
                    | "extensions.refstorage"
                    | "init.defaultbranch"
            )
            || (key.starts_with("branch.")
                && key
                    .rsplit_once('.')
                    .is_some_and(|(_, k)| matches!(k, "remote" | "merge" | "description" | "pushremote")))
            || (key.starts_with("remote.")
                && key.rsplit_once('.').is_some_and(|(_, k)| matches!(k, "url" | "fetch" | "pushurl" | "tagopt")));
        if keep {
            local.push((key.clone(), value.clone()));
        }
    }
    Ok(Config { raw, effective: local, native })
}

/// System and global settings can provide installed filters and transport helpers.
/// Inspect the include graph before exposing those scopes to registered projects.
/// The inspection command runs outside all repositories and does not follow includes.
fn user_scopes(cmd: &mut Command) {
    for key in ["GIT_CONFIG_NOSYSTEM", "GIT_CONFIG_SYSTEM", "GIT_CONFIG_GLOBAL"] {
        cmd.env_remove(key);
        if let Some(value) = std::env::var_os(key) {
            cmd.env(key, value);
        }
    }
}

fn user_settings(common: &Path, worktree: &Path) -> Result<Settings> {
    let mut cmd = base();
    user_scopes(&mut cmd);
    let args = ["config", "--no-includes", "--show-origin", "--null", "--list"];
    cmd.args(args);
    let output = run(cmd, &args, None, quick())?;
    let mut origins = HashMap::new();
    let mut fields = output.split_terminator('\0');
    let mut result = Vec::new();
    while let Some(origin) = fields.next() {
        let entry = fields.next().context("missing Git setting")?;
        let Some(path) = origin.strip_prefix("file:").map(Path::new) else { continue };
        if !*origins.entry(path).or_insert_with(|| protected(path, common, worktree)) {
            continue;
        }
        let (key, value) = entry.split_once('\n').unwrap_or((entry, "true"));
        if is_include(key) {
            if let Some(included) = include_file(path, value, common, worktree, 0) {
                result.push((include_key(path, key), included.to_string_lossy().into_owned()));
            }
        } else {
            result.push((key.into(), value.into()));
        }
    }
    Ok(result)
}

/// Resolve before entering the checkout. Check both sides of symlinks and pin
/// the resulting absolute filename so Git never repeats a PATH lookup there.
pub(super) fn installed_program(name: &str) -> Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let path = Path::new(name);
    let candidates = if path.is_absolute() {
        vec![path.to_owned()]
    } else {
        ensure!(path.components().count() == 1 && !name.contains('/'), "relative driver path");
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .filter(|dir| dir.is_absolute())
            .map(|dir| dir.join(path))
            .collect()
    };
    let roots = registered_common_dirs();
    let outside = |path: &Path, real: &Path| {
        !sandbox_writable(path)
            && crate::workspace::untrusted(path).is_none()
            && !roots.iter().any(|common| common.parent().is_some_and(|repo| crate::paths::at_or_below(path, repo)))
            && !path.ancestors().any(|p| {
                // Homebrew stores installed packages alongside its own Git checkout.
                // Only the package manager's prefix can contain that enclosing .git.
                let cellar = matches!(p.to_str(), Some("/opt/homebrew" | "/usr/local"))
                    && real.starts_with(p.join("Cellar"))
                    && p.join("Library/Homebrew").is_dir();
                !cellar
                    && (p.join(".git").symlink_metadata().is_ok()
                        || (p.join("HEAD").is_file() && p.join("objects").is_dir()))
            })
    };
    for candidate in candidates {
        let Ok(real) = candidate.canonicalize() else { continue };
        if real.metadata().is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0) {
            ensure!(
                outside(&candidate, &real) && outside(&real, &real),
                "driver executable is inside a repository or worktree"
            );
            return Ok(real);
        }
    }
    bail!("driver executable is not installed on the host")
}

pub(super) fn shell_path(path: &Path) -> Result<String> {
    let text = path.to_str().context("non-UTF-8 driver path")?;
    ensure!(!text.contains(['%', '\n', '\r']), "invalid driver path");
    Ok(crate::hook::shell_quote(text))
}

fn driver_command(raw: &str) -> Result<String> {
    let words = shlex::split(raw).context("invalid driver command")?;
    let (name, args) = words.split_first().context("empty driver command")?;
    let program = installed_program(name)?;
    // With no arguments an interpreter can execute the filtered bytes, and
    // build tools select a program from their current working directory.
    let executable = program.file_name().and_then(|n| n.to_str()).context("invalid driver name")?;
    let family = executable.trim_end_matches(|c: char| c.is_ascii_digit() || c == '.');
    ensure!(
        !matches!(
            family,
            "sh" | "bash"
                | "dash"
                | "zsh"
                | "ksh"
                | "csh"
                | "tcsh"
                | "fish"
                | "python"
                | "perl"
                | "ruby"
                | "node"
                | "php"
                | "lua"
                | "tclsh"
                | "osascript"
                | "make"
                | "gmake"
                | "just"
                | "ninja"
                | "rake"
                | "xargs"
                | "parallel"
                | "source"
        ),
        "a command interpreter or build tool cannot be a Git driver"
    );
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    // tr reads literal character sets. A single sed substitution with no
    // command flags is also a stream transform. Re-quote the parsed literals.
    if executable == "tr"
        || (executable == "sed"
            && matches!(args.as_slice(), [expression]
        if matches!(expression.split('/').collect::<Vec<_>>().as_slice(), ["s", _, _, "" | "g"])))
    {
        ensure!(
            args.iter().all(|arg| !arg.is_empty() && !arg.contains(['%', '\n', '\r', '\\'])),
            "invalid stream filter argument"
        );
        let mut command = shell_path(&program)?;
        for arg in args {
            command.push(' ');
            command.push_str(&crate::hook::shell_quote(arg));
        }
        return Ok(command);
    }
    // Git substitutes these specific LFS arguments itself. Arbitrary interpreter
    // arguments and shell commands can select a second program in the checkout.
    let lfs = Path::new(name).file_name().is_some_and(|n| n == "git-lfs");
    let allowed = args.is_empty()
        || (lfs
            && matches!(
                args.as_slice(),
                ["filter-process"]
                    | ["clean" | "smudge", "--", "%f"]
                    | ["clean" | "smudge", "%f"]
                    | [
                        "merge-driver",
                        "--ancestor",
                        "%O",
                        "--current",
                        "%A",
                        "--other",
                        "%B",
                        "--marker-size",
                        "%L",
                        "--output",
                        "%A"
                    ]
            ));
    ensure!(allowed, "use a standalone host-installed driver for a command with arguments");
    let mut command = shell_path(&program)?;
    for arg in args {
        command.push(' ');
        command.push_str(arg);
    }
    Ok(command)
}

/// Git evaluates ./ in a gitdir condition relative to the including file.
fn include_key(source: &Path, key: &str) -> String {
    for prefix in ["includeif.gitdir:", "includeif.gitdir/i:"] {
        if let Some(rest) = key.strip_prefix(prefix).and_then(|path| path.strip_prefix("./")) {
            return format!("{prefix}{}", source.parent().unwrap().join(rest).display());
        }
    }
    key.to_owned()
}

/// Pin commands in all reachable includes, even ones gated on the writable
/// worktree HEAD. A later branch switch still selects only captured drivers.
fn pin_drivers(
    settings: &mut Settings,
    source: &Path,
    common: &Path,
    worktree: &Path,
    files: &mut Vec<tempfile::NamedTempFile>,
    depth: usize,
) -> Result<bool> {
    let mut changed = false;
    for (key, value) in settings {
        if is_include(key) {
            let path = include_file(source, value, common, worktree, depth).context("Git config include changed")?;
            let mut nested = read_settings(&path)?.as_ref().clone();
            if pin_drivers(&mut nested, &path, common, worktree, files, depth + 1)? {
                let mut snapshot = tempfile::NamedTempFile::new()?;
                write_settings(&mut snapshot, &nested)?;
                *value = snapshot.path().to_string_lossy().into_owned();
                files.push(snapshot);
                changed = true;
            } else {
                // Preserve relative includes if a sibling driver relocates this file.
                *value = path.to_string_lossy().into_owned();
            }
            *key = include_key(source, key);
            continue;
        }
        let driver = key == "diff.external"
            || key.rsplit_once('.').is_some_and(|(group, field)| {
                (group.starts_with("filter.") && matches!(field, "process" | "clean" | "smudge"))
                    || (group.starts_with("merge.") && field == "driver")
                    || (group.starts_with("diff.") && matches!(field, "command" | "textconv"))
            });
        if driver && !value.is_empty() {
            *value = driver_command(value).with_context(|| format!("refusing Git driver {key}"))?;
            changed = true;
        }
    }
    Ok(changed)
}

pub(super) fn check_sandbox_config(common: &Path) -> Result<()> {
    for (key, value) in read_settings(&common.join("config"))?.iter() {
        ensure!(
            key != "extensions.worktreeconfig"
                || matches!(value.to_ascii_lowercase().as_str(), "false" | "no" | "off" | "0"),
            "extensions.worktreeConfig permits worktree-local Git settings"
        );
    }
    Ok(())
}

/// Keep the approved configuration alive through Git and its subprocesses, then unlink it.
pub(super) struct Invocation {
    cmd: Command,
    _global: Option<tempfile::NamedTempFile>,
    includes: Vec<tempfile::NamedTempFile>,
    view: Option<RepositoryView>,
}

impl std::ops::Deref for Invocation {
    type Target = Command;
    fn deref(&self) -> &Command {
        &self.cmd
    }
}

impl std::ops::DerefMut for Invocation {
    fn deref_mut(&mut self) -> &mut Command {
        &mut self.cmd
    }
}

impl From<Command> for Invocation {
    fn from(cmd: Command) -> Self {
        Self { cmd, _global: None, includes: Vec::new(), view: None }
    }
}

impl Invocation {
    /// Git has no option to replace just its local config. For content and index
    /// operations, supply a private common directory with the captured settings and
    /// links to repository data. Per-worktree state (HEAD and index) remains in GIT_DIR.
    pub(super) fn content(
        mut self,
        common: &Path,
        worktree: &Path,
        mut settings: Config,
        config_writer: bool,
        registered: bool,
        drivers: bool,
    ) -> Result<Self> {
        use std::os::unix::fs::symlink;
        if drivers
            && pin_drivers(&mut settings.effective, &common.join("config"), common, worktree, &mut self.includes, 0)?
        {
            settings.native = false;
        }
        if settings.native {
            return Ok(self);
        }
        let view = tempfile::tempdir()?;
        for name in ["HEAD", "refs", "objects", "logs", "packed-refs", "reftable", "shallow", "info", "worktrees"] {
            symlink(common.join(name), view.path().join(name))?;
        }
        let mut config = std::fs::File::create(view.path().join("config"))?;
        write_settings(&mut config, &settings.effective)?;
        self.cmd.env("GIT_COMMON_DIR", view.path());
        if registered {
            // A driver may use Git's common directory for storage. Git LFS accepts an
            // absolute storage path; resolve its effective setting in this same view.
            let mut probe = base();
            for (key, value) in self.cmd.get_envs() {
                if matches!(key.to_str(), Some("GIT_DIR" | "GIT_COMMON_DIR" | "GIT_WORK_TREE" | "GIT_CONFIG_GLOBAL"))
                    && let Some(value) = value
                {
                    probe.env(key, value);
                }
            }
            let args = ["config", "--path", "--get", "lfs.storage"];
            probe.args(args);
            let output = output(probe, &args, None, quick())?;
            ensure!(matches!(output.status.code(), Some(0 | 1)), "cannot determine Git LFS storage");
            let storage = std::str::from_utf8(&output.stdout)?.trim_end_matches('\n');
            let storage = common.join(if storage.is_empty() { "lfs" } else { storage });
            let storage = ("lfs.storage".to_string(), storage.to_string_lossy().into_owned());
            write_settings(&mut config, std::slice::from_ref(&storage))?;
            settings.effective.push(storage);
        }
        self.view =
            Some(RepositoryView { folder: view, common: common.to_owned(), config: config_writer.then_some(settings) });
        Ok(self)
    }

    pub(super) fn for_project(
        mut cmd: Command,
        common: &Path,
        worktree: &Path,
        registered: bool,
        drivers: bool,
    ) -> Result<Self> {
        if !registered {
            return Self::repository(cmd);
        }
        let mut file = tempfile::NamedTempFile::new()?;
        let mut includes = Vec::new();
        let mut settings = user_settings(common, worktree)?;
        // Top-level user include paths were resolved against their own origins.
        if drivers {
            pin_drivers(&mut settings, &common.join("config"), common, worktree, &mut includes, 0)?;
        }
        write_settings(&mut file, &settings)?;
        cmd.env("GIT_CONFIG_GLOBAL", file.path());
        Ok(Self { cmd, _global: Some(file), includes, view: None })
    }

    pub(super) fn repository(mut cmd: Command) -> Result<Self> {
        let settings = global_config()?;
        let mut global = None;
        if !settings.is_empty() {
            let mut file = tempfile::NamedTempFile::new()?;
            file.write_all(settings)?;
            cmd.env("GIT_CONFIG_GLOBAL", file.path());
            global = Some(file);
        }
        Ok(Self { cmd, _global: global, includes: Vec::new(), view: None })
    }
}

/// Keep Git's administrative path output in the real repository's coordinates. Only
/// fields defined as paths by the requested command undergo this translation.
struct RepositoryView {
    folder: tempfile::TempDir,
    common: PathBuf,
    config: Option<Config>,
}

impl Invocation {
    pub(super) fn finish(&self, args: &[&str], output: &mut Vec<u8>) -> Result<()> {
        let Some(view) = &self.view else { return Ok(()) };
        if let Some(original) = &view.config {
            view.save_config(original)?;
        }
        let mut sub = args;
        while let ["-c", _, rest @ ..] = sub {
            sub = rest;
        }
        if matches!(sub, ["worktree", "list", ..]) {
            let end = output.iter().position(|b| matches!(b, 0 | b'\n')).unwrap_or(output.len());
            if output.starts_with(b"worktree ") {
                let main = if view.common.file_name().is_some_and(|n| n == ".git") {
                    view.common.parent().unwrap()
                } else {
                    &view.common
                };
                output.splice(9..end, main.as_os_str().as_encoded_bytes().iter().copied());
            }
            return Ok(());
        }
        match sub {
            ["rev-parse", args @ ..] if args.iter().any(|a| matches!(*a, "--git-path" | "--git-common-dir")) => {
                view.translate_path_fields(output, b'\n', 1)
            }
            // Verbose NUL records contain the rule source, line number, pattern and queried path.
            ["check-ignore", "-v", "--no-index", "-z", "--stdin"] => view.translate_path_fields(output, 0, 4),
            _ => Ok(()),
        }
    }
}

impl RepositoryView {
    /// Translate the leading path in each record when it is at or below the private common directory.
    fn translate_path_fields(&self, output: &mut Vec<u8>, separator: u8, fields_per_record: usize) -> Result<()> {
        use std::os::unix::ffi::OsStrExt;

        let canonical = self.folder.path().canonicalize()?;
        let mut translated = Vec::with_capacity(output.len());
        for (i, field) in output.split_inclusive(|&b| b == separator).enumerate() {
            let field = field.strip_suffix(&[separator]).context("unterminated Git path record")?;
            let path = Path::new(std::ffi::OsStr::from_bytes(field));
            if i.is_multiple_of(fields_per_record)
                && let Ok(tail) = path.strip_prefix(self.folder.path()).or_else(|_| path.strip_prefix(&canonical))
            {
                let real = if tail.as_os_str().is_empty() { self.common.clone() } else { self.common.join(tail) };
                translated.extend_from_slice(real.as_os_str().as_bytes());
            } else {
                translated.extend_from_slice(field);
            }
            translated.push(separator);
        }
        *output = translated;
        Ok(())
    }

    /// Git can update branch tracking or remote settings. Apply just those changes to
    /// a copy of the original config so comments, includes and unrelated keys survive.
    /// Git's normal lock and a byte comparison protect concurrent configuration edits.
    fn save_config(&self, before: &Config) -> Result<()> {
        let after = read_settings(&self.folder.path().join("config"))?;
        if *after == before.effective {
            return Ok(());
        }
        let keys: std::collections::BTreeSet<_> = before.effective.iter().chain(after.iter()).map(|(k, _)| k).collect();
        let copy = tempfile::NamedTempFile::new()?;
        std::fs::write(copy.path(), &before.raw)?;
        let edit = |args: &[&str]| -> Result<()> {
            let mut cmd = base();
            cmd.args(["config", "--no-includes", "--file"]).arg(copy.path()).args(args);
            run(cmd, &["config", "update"], None, quick()).map(drop)
        };
        for key in keys {
            let values =
                |list: &Settings| list.iter().filter(|(k, _)| k == key).map(|(_, v)| v.clone()).collect::<Vec<_>>();
            let (old, new) = (values(&before.effective), values(&after));
            if old == new {
                continue;
            }
            ensure!(key.starts_with("remote.") || key.starts_with("branch."), "unexpected Git configuration change");
            if !old.is_empty() {
                edit(&["--unset-all", key])?;
            }
            for value in new {
                edit(&["--add", key, &value])?;
            }
        }
        let destination = self.common.join("config");
        let lock = self.common.join("config.lock");
        let mut fd = std::fs::OpenOptions::new().write(true).create_new(true).open(&lock)?;
        let lock = tempfile::TempPath::try_from_path(lock)?;
        ensure!(
            read_regular_file(&destination, 256 * 1024)? == before.raw,
            "repository config changed during Git operation"
        );
        fd.set_permissions(std::fs::symlink_metadata(&destination)?.permissions())?;
        fd.write_all(&std::fs::read(copy.path())?)?;
        lock.persist(destination)?;
        Ok(())
    }
}
