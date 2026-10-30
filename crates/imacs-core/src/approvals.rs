use std::path::PathBuf;
#[cfg(feature = "native")]
use std::path::{Component, Path};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::hook::HookEvent;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Allow,
    AllowAlways,
    Deny,
}

impl Decision {
    pub fn output(self, message: Option<&str>) -> String {
        let mut decision = json!({"behavior": if self == Self::Deny { "deny" } else { "allow" }});
        if let Some(message) = message {
            decision["message"] = json!(message);
        }
        json!({"hookSpecificOutput": {"hookEventName": "PermissionRequest", "decision": decision}}).to_string()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalRequest {
    pub tool: String,
    pub command: Option<String>,
    pub cwd: PathBuf,
    pub reason: Option<String>,
    pub input: Value,
}

impl ApprovalRequest {
    pub fn from_event(event: &HookEvent) -> Self {
        let text = |key: &str| event.tool_input.get(key).and_then(Value::as_str).map(str::to_string);
        Self {
            tool: event.tool_name.clone().unwrap_or_default(),
            command: text("command"),
            cwd: text("cwd")
                .or_else(|| text("workdir"))
                .map(PathBuf::from)
                .or_else(|| event.cwd.clone())
                .unwrap_or_default(),
            reason: text("description").or_else(|| text("justification")),
            input: event.tool_input.clone(),
        }
    }

    pub fn same_command(&self, other: &Self) -> bool {
        self.tool == other.tool
            && self.cwd == other.cwd
            && self.command == other.command
            && (self.command.is_some() || self.input == other.input)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ApprovalPolicy {
    pub allow: Vec<String>,
    /// Exact NAME=value assignments, including an empty value after `=`.
    pub env: Vec<String>,
}

impl Default for ApprovalPolicy {
    fn default() -> Self {
        Self {
            allow: ["ls", "cat", "rg", "git status", "git diff", "git log"].into_iter().map(str::to_string).collect(),
            env: Vec::new(),
        }
    }
}

#[cfg(feature = "native")]
impl ApprovalPolicy {
    pub fn allows(&self, request: &ApprovalRequest, root: &Path) -> bool {
        if request.tool != "Bash" || !inside(&request.cwd, root) {
            return false;
        }
        let Some(command) = &request.command else { return false };
        let Some(commands) = shell_commands(command) else { return false };
        commands.iter().all(|words| self.allows_command(words, &request.cwd, root))
    }

    fn allows_command(&self, raw: &[&str], cwd: &Path, root: &Path) -> bool {
        let Some(words): Option<Vec<String>> = raw.iter().map(|word| shlex::split(word)?.pop()).collect() else {
            return false;
        };
        // Only an unquoted NAME= prefix is a shell assignment. `env` accepts quoted ones too.
        let assignments = raw.iter().take_while(|word| assignment(word)).count();
        if !words[..assignments].iter().all(|word| self.env.contains(word)) {
            return false;
        }
        let mut words = &words[assignments..];
        loop {
            if words.first().map(String::as_str) == Some("env") {
                words = &words[1..];
                let assignments = words.iter().take_while(|word| assignment(word)).count();
                if assignments == 0 || !words[..assignments].iter().all(|word| self.env.contains(word)) {
                    return false;
                }
                words = &words[assignments..];
            } else if let Some(inner) = crate::governor::queued_command(words) {
                // Queue executes argv directly: a bare assignment here is not a shell prefix.
                words = inner;
            } else {
                break;
            }
        }
        let Some(words) = self.cargo_command(words) else { return false };
        if words.is_empty()
            || !self.allow.iter().any(|prefix| {
                shlex::split(prefix)
                    .and_then(|prefix| self.cargo_command(&prefix))
                    .is_some_and(|prefix| !prefix.is_empty() && words.starts_with(&prefix))
            })
        {
            return false;
        }
        for word in &words[1..] {
            // These read-tool flags can execute programs or write files.
            if [
                "--pre",
                "--hostname-bin",
                "--ext-diff",
                "--textconv",
                "--output",
                "--config",
                "--config-path",
                "--exec-path",
                "--git-dir",
                "--work-tree",
                "--directory",
                "--out-dir",
                "--manifest-path",
                "--target-dir",
                "--follow",
                "--dereference",
                "--dereference-command-line",
                "--dereference-command-line-symlink-to-dir",
            ]
            .iter()
            .any(|flag| word == flag || word.starts_with(&format!("{flag}=")))
                || ["-c", "-C", "-o", "-O", "-f"]
                    .iter()
                    .any(|flag| !word.starts_with("--") && word.starts_with(flag) && word.len() > flag.len())
                || matches!(word.as_str(), "-c" | "-C" | "-o")
                || (words[0] == "cargo"
                    && word.starts_with('-')
                    && !word.starts_with("--")
                    && word.contains(['Z', 'C']))
                || (words[0] == "ls" && word.starts_with('-') && !word.starts_with("--") && word.contains('L'))
                || (words[0] == "rg" && word.starts_with('-') && !word.starts_with("--") && word.contains('L'))
            {
                return false;
            }
            let value = word.split_once('=').map_or(word.as_str(), |(_, value)| value);
            if (value.contains('/') || value == "..") && !inside(&cwd.join(value), root) {
                return false;
            }
            if !word.starts_with('-') && cwd.join(word).exists() && !inside(&cwd.join(word), root) {
                return false;
            }
        }
        // Non-cargo programs still have to stay within the worktree themselves.
        if words[0].contains('/') && !inside(&cwd.join(&words[0]), root) {
            return false;
        }
        true
    }

    fn cargo_command(&self, words: &[String]) -> Option<Vec<String>> {
        let mut words = words;
        if words.first().map(String::as_str) == Some("rustup") {
            if words.get(1).map(String::as_str) != Some("run") || !toolchain(words.get(2)?) {
                return None;
            }
            words = &words[3..];
            if !self.is_cargo(words.first()?) {
                return None;
            }
        }
        let mut normalized = words.to_vec();
        if let Some(program) = words.first()
            && self.is_cargo(program)
        {
            normalized[0] = "cargo".into();
            if let Some(version) = words.get(1).and_then(|word| word.strip_prefix('+')) {
                if !toolchain(version) {
                    return None;
                }
                normalized.remove(1);
            }
        }
        Some(normalized)
    }

    fn is_cargo(&self, program: &str) -> bool {
        use std::os::unix::fs::PermissionsExt;

        if program == "cargo" {
            return true;
        }
        let path = Path::new(program);
        // Rustup proxies share a real path but dispatch by basename (e.g. rustc vs cargo).
        if !path.is_absolute() || path.file_name().is_none_or(|name| name != "cargo") {
            return false;
        }
        let Ok(real) = path.canonicalize() else { return false };
        let configured = self.allow.iter().filter_map(|prefix| shlex::split(prefix)?.into_iter().next());
        if configured.filter(|program| Path::new(program).is_absolute()).any(|program| {
            let path = Path::new(&program);
            path.file_name().is_some_and(|name| name == "cargo") && path.canonicalize().is_ok_and(|p| p == real)
        }) {
            return true;
        }
        let Some(path) = std::env::var_os("PATH") else { return false };
        // Relative and empty entries would resolve inside the agent's own directory.
        std::env::split_paths(&path)
            .filter(|dir| dir.is_absolute())
            .map(|dir| dir.join("cargo"))
            .find(|path| path.metadata().is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0))
            .and_then(|path| path.canonicalize().ok())
            .is_some_and(|path| path == real)
    }
}

#[cfg(feature = "native")]
fn assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

#[cfg(feature = "native")]
fn toolchain(word: &str) -> bool {
    word.starts_with(|c: char| c.is_ascii_alphanumeric())
        && word.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// A deliberately small shell grammar: words with quotes, joined only by plain `&&` or `;`.
/// Keep raw words so quoted assignment names cannot become shell environment prefixes.
#[cfg(feature = "native")]
fn shell_commands(command: &str) -> Option<Vec<Vec<&str>>> {
    // Keep refusing expansions and redirections even inside quotes. Reject controls (including
    // newlines) before parsing, so heredocs and line continuations cannot hide commands.
    if command.chars().any(|c| {
        (c.is_control() && c != '\t')
            || matches!(c, '|' | '<' | '>' | '`' | '$' | '*' | '?' | '[' | ']' | '{' | '}' | '~' | '(' | ')' | '\\')
    }) {
        return None;
    }
    let mut commands = Vec::new();
    let mut words = Vec::new();
    let mut start = None;
    let mut quote = None;
    let mut chars = command.char_indices();
    while let Some((index, c)) = chars.next() {
        if let Some(q) = quote {
            if c == q {
                quote = None;
            }
            continue;
        }
        if matches!(c, '\'' | '"') {
            start.get_or_insert(index);
            quote = Some(c);
        } else if matches!(c, ' ' | '\t' | ';' | '&') {
            if let Some(start) = start.take() {
                words.push(&command[start..index]);
            }
            if matches!(c, ';' | '&') {
                if c == '&' && chars.next().is_none_or(|(_, c)| c != '&') {
                    return None;
                }
                if words.is_empty() {
                    return None;
                }
                commands.push(std::mem::take(&mut words));
            }
        } else if c == '#' {
            return None;
        } else {
            start.get_or_insert(index);
        }
    }
    if quote.is_some() {
        return None;
    }
    if let Some(start) = start {
        words.push(&command[start..]);
    }
    if words.is_empty() {
        return None;
    }
    commands.push(words);
    Some(commands)
}

#[cfg(feature = "native")]
fn inside(path: &Path, root: &Path) -> bool {
    fn resolve(path: &Path) -> Option<PathBuf> {
        if !path.is_absolute() {
            return None;
        }
        let mut resolved = PathBuf::new();
        for component in path.components() {
            match component {
                Component::ParentDir => {
                    resolved.pop();
                }
                Component::CurDir => {}
                other => resolved.push(other.as_os_str()),
            }
            if resolved.exists() {
                resolved = resolved.canonicalize().ok()?;
            }
        }
        Some(resolved)
    }
    matches!((resolve(path), resolve(root)), (Some(path), Some(root)) if path.starts_with(&root))
}

/// Codex also prompts for edits and terminal input; their choices must never receive steering text.
pub fn menu_visible(screen: &str) -> bool {
    if let Some(prompt) = screen.lines().rev().find_map(|line| line.trim().strip_prefix(['›', '❯', '>']))
        && !prompt
            .trim()
            .split_once('.')
            .is_some_and(|(number, _)| !number.is_empty() && number.chars().all(|c| c.is_ascii_digit()))
    {
        return false;
    }
    let compact: String = screen.chars().filter(|c| !c.is_whitespace()).flat_map(char::to_lowercase).collect();
    compact.contains("pressentertoconfirmoresctocancel")
        || screen.lines().any(|line| {
            let line = line.trim();
            let Some(choice) = line.strip_prefix(['›', '❯', '>']) else { return false };
            let Some((number, _)) = choice.trim().split_once('.') else { return false };
            !number.is_empty() && number.chars().all(|c| c.is_ascii_digit())
        })
}

/// A question an agent asks before its first turn and waits on: Codex's "Trust this folder?",
/// its update and sign-in screens, Claude's folder trust, or any choice menu.
pub fn first_run_prompt(screen: &str) -> bool {
    const PROMPTS: &[&str] = &[
        "trustthisfolder?",
        "trustandcontinue",
        "doyoutrustthefilesinthisfolder",
        "skipuntilnextversion",
        "signinwithchatgpt",
        "pressentertocontinue",
    ];
    let compact: String = screen.chars().filter(|c| !c.is_whitespace()).flat_map(char::to_lowercase).collect();
    menu_visible(screen) || PROMPTS.iter().any(|prompt| compact.contains(prompt))
}

/// Match a complete native menu command, expanding hidden or truncated commands first.
pub fn menu_key(screen: &str, command: Option<&str>, decision: Decision) -> Option<String> {
    let command = command?;
    let (_, menu) = screen.rsplit_once("Would you like to run the following command?")?;
    if !menu_visible(menu) {
        return None;
    }
    let Some(line) = menu.lines().find(|line| line.trim_start().starts_with('$')) else {
        return (menu.contains("Reason:") && menu.contains("⌃a view all") && menu_visible(menu)).then(|| "\x01".into());
    };
    let shown = menu[menu.find(line)?..].trim_start().strip_prefix('$')?;
    let shown = shown.split("\n\n").next()?;
    // Codex 0.165 lists the choices right below the command, without a blank line.
    let choice = |line: &str| {
        let line = line.trim().trim_start_matches(['›', '❯', '>']).trim_start();
        line.split_once(". ")
            .is_some_and(|(number, _)| !number.is_empty() && number.chars().all(|c| c.is_ascii_digit()))
    };
    let shown = shown.lines().take_while(|line| !choice(line)).collect::<Vec<_>>().join("\n");
    let compact = |text: &str| text.chars().filter(|c| !c.is_whitespace()).collect::<String>();
    let (shown, command) = (compact(&shown), compact(command));
    let truncated_match = shown.rsplit_once("[…").is_some_and(|(prefix, marker)| {
        let lines = marker.strip_suffix("lines]⌃aviewall").or_else(|| marker.strip_suffix("lines]"));
        !prefix.is_empty()
            && command.starts_with(prefix)
            && lines.is_some_and(|lines| lines.parse::<usize>().is_ok_and(|lines| lines > 0))
    });
    if truncated_match {
        return menu.contains("⌃a view all").then(|| "\x01".into());
    }
    if shown != command {
        return None;
    }
    if decision == Decision::Deny && menu.contains("esc to cancel") {
        return Some("\x1b".into());
    }
    let needle = match decision {
        Decision::Allow | Decision::AllowAlways => "yes, proceed",
        Decision::Deny => "no, and tell codex",
    };
    menu.lines().find_map(|line| {
        let line = line.trim().trim_start_matches(['›', '❯', ' ']);
        let (number, label) = line.split_once('.')?;
        (number.len() == 1
            && number.chars().all(|c| c.is_ascii_digit())
            && label.trim().to_lowercase().starts_with(needle))
        .then(|| format!("{number}\r"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[cfg(feature = "native")]
    fn request(command: &str, cwd: &Path) -> ApprovalRequest {
        ApprovalRequest {
            tool: "Bash".into(),
            command: Some(command.into()),
            cwd: cwd.into(),
            reason: None,
            input: Value::Null,
        }
    }

    #[test]
    #[cfg(feature = "native")]
    fn default_policy_allows_only_worktree_reads() {
        let dir = crate::test_support::tempdir().unwrap();
        for command in ["ls -la", "cat Cargo.toml", "rg hook src", "git status --short", "git diff", "git log -5"] {
            assert!(ApprovalPolicy::default().allows(&request(command, dir.path()), dir.path()), "{command}");
        }
    }

    #[test]
    #[cfg(feature = "native")]
    fn cargo_requires_an_explicit_policy_entry() {
        let dir = crate::test_support::tempdir().unwrap();
        for command in [
            "cargo build",
            "cargo test",
            "cargo check",
            "cargo clippy",
            "cargo fmt",
            "cargo +stable test",
            "rustup run stable cargo test",
            "RUSTC_WRAPPER= cargo test",
            "env RUSTC_WRAPPER= cargo test",
            "ls && cargo test",
        ] {
            assert!(!ApprovalPolicy::default().allows(&request(command, dir.path()), dir.path()));
        }
        assert!(
            ApprovalPolicy { allow: vec!["cargo test".into()], ..ApprovalPolicy::default() }
                .allows(&request("cargo test -p imacs-core", dir.path()), dir.path())
        );
    }

    #[test]
    #[cfg(feature = "native")]
    fn policy_judges_chains_env_and_toolchains_by_their_parts() {
        let dir = crate::test_support::tempdir().unwrap();
        let mut policy = ApprovalPolicy {
            env: vec!["RUSTC_WRAPPER=".into(), "CARGO_TERM_COLOR=always".into()],
            ..ApprovalPolicy::default()
        };
        policy.allow.extend(["cargo check", "cargo test", "cargo clippy", "cargo fmt"].map(str::to_string));
        for (command, expected) in [
            ("ls&&git status; cargo test -p imacs-core", true),
            ("cargo check && cargo test && cargo fmt --check", true),
            ("RUSTC_WRAPPER= cargo check", true),
            ("RUSTC_WRAPPER='' cargo check", true),
            ("RUSTC_WRAPPER= CARGO_TERM_COLOR=always cargo test", true),
            ("env RUSTC_WRAPPER= cargo test", true),
            ("env 'RUSTC_WRAPPER=' cargo test", true),
            ("RUSTC_WRAPPER= env CARGO_TERM_COLOR=always cargo test", true),
            ("cargo +stable check", true),
            ("cargo +nightly-2026-10-01 clippy -- -D warnings", true),
            ("rustup run stable cargo test -p imacs-core", true),
            ("env RUSTC_WRAPPER= rustup run stable cargo test", true),
            ("RUSTC_WRAPPER= cargo +stable test && env RUSTC_WRAPPER= cargo fmt --check", true),
            ("imacs queue --class test -- env RUSTC_WRAPPER= cargo +stable test", true),
            ("env RUSTC_WRAPPER= imacs queue --class=test -- rustup run stable cargo test", true),
            ("imacs queue -- cargo test; git status", true),
            ("rg 'a && b' src && git status", true),
            ("rg \"a; b\" src; ls", true),
            ("rg a'&&'b src", true),
            ("cat 'file with spaces'", true),
            ("\tgit status\t&&\tls", true),
            ("", false),
            (" ", false),
            ("cargo check && rm file", false),
            ("rm file; cargo check", false),
            ("cargo test && cargo publish", false),
            ("cargo test; unknown-program", false),
            ("cargo test && env RUSTC=evil cargo check", false),
            ("cargo test &&", false),
            ("; cargo test", false),
            ("cargo test;;ls", false),
            ("cargo test &&& ls", false),
            ("cargo test & ls", false),
            ("cargo test || ls", false),
            ("cargo test | cat", false),
            ("cargo test > output", false),
            ("cargo test 2>&1", false),
            ("cargo test < input", false),
            ("cargo test\nls", false),
            ("cargo test\rls", false),
            ("cargo test\0", false),
            ("cat <<'EOF'\nls && rm file\nEOF", false),
            ("cat <<'EOF' && ls", false),
            ("cargo test # && rm file", false),
            ("cargo test $(ls)", false),
            ("cargo test `ls`", false),
            ("cargo test $ARGS", false),
            ("cat *", false),
            ("cat file?", false),
            ("cat [ab]", false),
            ("cat {a,b}", false),
            ("cat ~/file", false),
            ("ls \\\n&& ls", false),
            ("rg 'unterminated && ls", false),
            ("rg \"unterminated; ls", false),
            ("rg '&&' /etc/passwd", false),
            ("rg '&&' src && rm file", false),
            ("cat ../secret && ls", false),
            ("ls; git diff --ext-diff", false),
            ("RUSTC_WRAPPER=evil cargo test", false),
            ("RUSTC_WRAPPER= evil", false),
            ("RUSTC_WRAPPER=", false),
            ("'RUSTC_WRAPPER=' cargo test", false),
            ("RUSTC_WRAPPER''= cargo test", false),
            ("RUSTC=arbitrary cargo test", false),
            ("PATH=bin cargo test", false),
            ("DYLD_INSERT_LIBRARIES=evil cargo test", false),
            ("LD_PRELOAD=evil cargo test", false),
            ("CARGO_TERM_COLOR=never cargo test", false),
            ("env RUSTC_WRAPPER=evil cargo test", false),
            ("env RUSTC_WRAPPER= LD_PRELOAD=evil cargo test", false),
            ("env PATH=bin cargo test", false),
            ("env -i RUSTC_WRAPPER= cargo test", false),
            ("env -- RUSTC_WRAPPER= cargo test", false),
            ("env RUSTC_WRAPPER=", false),
            ("imacs queue -- RUSTC_WRAPPER= cargo test", false),
            ("imacs queue --class qa -- env RUSTC_WRAPPER= sh -c cargo", false),
            ("cargo +stable publish", false),
            ("cargo + test", false),
            ("cargo +../evil test", false),
            ("rustup run ../evil cargo test", false),
            ("rustup run --install cargo test", false),
            ("rustup run stable rustc test", false),
            ("rustup run stable sh -c cargo", false),
            ("rustup run stable", false),
            ("rustup default stable", false),
            ("./cargo test", false),
            ("/does-not-exist/cargo test", false),
            ("cargo test --manifest-path ../Cargo.toml", false),
            ("cargo +stable test --manifest-path=../Cargo.toml", false),
            ("cargo test --manifest-path Cargo.toml", false),
            ("env RUSTC_WRAPPER= cargo test --config build.rustc=evil", false),
            ("cargo test --config=evil", false),
            ("cargo test --target-dir ../target", false),
            ("rustup run stable cargo test --target-dir=/tmp/target", false),
            ("cargo test -Z unstable-options", false),
            ("cargo test -Zunstable-options", false),
            ("cargo test -vZunstable-options", false),
            ("cargo test -C ../other", false),
            ("cargo test -C../other", false),
            ("cargo test -vCother", false),
        ] {
            assert_eq!(policy.allows(&request(command, dir.path()), dir.path()), expected, "{command:?}");
        }
    }

    #[test]
    #[cfg(feature = "native")]
    fn policy_checks_real_cargo_paths() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        use std::process::Command;

        // Isolate PATH in a child, without mutating the environment of parallel tests.
        let Some(fixture) = std::env::var_os("IMACS_APPROVAL_PATH_FIXTURE") else {
            let dir = crate::test_support::tempdir().unwrap();
            for folder in ["first", "second", "configured", "linked", "worktree"] {
                std::fs::create_dir(dir.path().join(folder)).unwrap();
            }
            for folder in ["first", "second", "configured"] {
                let path = dir.path().join(folder).join("cargo");
                std::fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
            symlink(dir.path().join("first/cargo"), dir.path().join("linked/cargo")).unwrap();
            symlink(dir.path().join("first/cargo"), dir.path().join("linked/rustc")).unwrap();
            let output = crate::test_support::output(
                Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", "approvals::tests::policy_checks_real_cargo_paths", "--nocapture"])
                    .env("IMACS_APPROVAL_PATH_FIXTURE", dir.path())
                    .env("PATH", std::env::join_paths([dir.path().join("first"), dir.path().join("second")]).unwrap()),
            );
            assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
            return;
        };
        let fixture = PathBuf::from(fixture);
        let root = fixture.join("worktree");
        let configured = format!("{} test", fixture.join("configured/cargo").display());
        let policy = ApprovalPolicy { allow: vec!["cargo test".into(), configured], ..ApprovalPolicy::default() };
        for (folder, expected) in [("first", true), ("linked", true), ("second", false), ("configured", true)] {
            let cargo = fixture.join(folder).join("cargo");
            for command in [
                format!("{} test", cargo.display()),
                format!("{} +stable test", cargo.display()),
                format!("rustup run stable {} test", cargo.display()),
                format!("imacs queue --class test -- {} test && {} test", cargo.display(), cargo.display()),
            ] {
                assert_eq!(policy.allows(&request(&command, &root), &root), expected, "{command}");
                assert!(!ApprovalPolicy::default().allows(&request(&command, &root), &root), "{command}");
            }
        }
        for command in [
            format!("{} test", fixture.join("linked/rustc").display()),
            format!("{} test --config=evil", fixture.join("first/cargo").display()),
            format!(
                "{} test && {} test",
                fixture.join("first/cargo").display(),
                fixture.join("second/cargo").display()
            ),
        ] {
            assert!(!policy.allows(&request(&command, &root), &root), "{command}");
        }
    }

    #[test]
    #[cfg(feature = "native")]
    fn relative_path_entries_never_make_an_agent_cargo_trusted() {
        use std::os::unix::fs::PermissionsExt;
        use std::process::Command;

        // Isolate PATH in a child, without mutating the environment of parallel tests.
        let Some(fixture) = std::env::var_os("IMACS_APPROVAL_RELATIVE_PATH_FIXTURE") else {
            let dir = crate::test_support::tempdir().unwrap();
            for folder in ["host", "worktree/bin"] {
                std::fs::create_dir_all(dir.path().join(folder)).unwrap();
                let path = dir.path().join(folder).join("cargo");
                std::fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
            std::fs::copy(dir.path().join("host/cargo"), dir.path().join("worktree/cargo")).unwrap();
            let path = std::env::join_paths([PathBuf::new(), PathBuf::from("bin"), dir.path().join("host")]).unwrap();
            let output = crate::test_support::output(
                Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", "approvals::tests::relative_path_entries_never_make_an_agent_cargo_trusted"])
                    .current_dir(dir.path().join("worktree"))
                    .env("IMACS_APPROVAL_RELATIVE_PATH_FIXTURE", dir.path())
                    .env("PATH", path),
            );
            assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
            return;
        };
        let fixture = PathBuf::from(fixture);
        let root = fixture.join("worktree");
        let policy = ApprovalPolicy { allow: vec!["cargo test".into()], ..ApprovalPolicy::default() };
        let allows = |cargo: PathBuf| policy.allows(&request(&format!("{} test", cargo.display()), &root), &root);
        assert!(!allows(root.join("cargo")), "an empty PATH entry must not resolve to the agent's directory");
        assert!(!allows(root.join("bin/cargo")), "a relative PATH entry must not resolve inside the agent's directory");
        assert!(allows(fixture.join("host/cargo")), "the first absolute PATH entry is the trusted cargo");
    }

    #[test]
    #[cfg(feature = "native")]
    fn queued_commands_are_judged_by_what_they_run() {
        let dir = crate::test_support::tempdir().unwrap();
        let policy = ApprovalPolicy { allow: vec!["cargo test".into(), "ls".into()], ..ApprovalPolicy::default() };
        let allows = |command: &str| policy.allows(&request(command, dir.path()), dir.path());
        assert!(allows("imacs queue --class test -- cargo test -p imacs-core"));
        assert!(allows("imacs queue -- cargo test"));
        // Only the `imacs` IMACS puts first on PATH: a binary of that name elsewhere could be anything.
        assert!(!allows("./imacs queue -- ls"));
        assert!(!allows("/tmp/evil/imacs queue -- git status"));
        assert!(!allows("imacs queue -- ls\nrm -rf src"));
        assert!(!allows("imacs queue -- ls\rrm -rf src"));
        assert!(!allows("imacs queue --class test -- cargo publish"));
        assert!(!allows("imacs queue -- rm -rf target"));
        assert!(!allows("imacs queue -- cargo test; touch pwned"));
        assert!(!allows("imacs queue --class test -- ls /etc"));
        assert!(!allows("imacs queue --class bogus -- cargo test"));
    }

    #[test]
    #[cfg(feature = "native")]
    fn policy_waits_for_writes_shell_syntax_external_paths_and_execution_flags() {
        let dir = crate::test_support::tempdir().unwrap();
        for command in [
            "cargo publish",
            "rm file",
            "git push",
            "git status; touch pwned",
            "cat $(pwd)",
            "ls | sh",
            "cat a > b",
            "rg --pre=sh x",
            "git diff --output=file",
            "git diff --ext-diff",
            "git log --textconv",
            "cat /etc/passwd",
            "cargo test --manifest-path=../Cargo.toml",
            "cat ../secret",
            "cargo test --target-dir=/tmp/elsewhere",
            "rg -f/etc/passwd pattern",
            "git diff -O/etc/passwd",
            "rg --follow pattern",
            "ls -RL",
        ] {
            assert!(!ApprovalPolicy::default().allows(&request(command, dir.path()), dir.path()), "{command}");
        }
        assert!(!ApprovalPolicy::default().allows(&request("ls", Path::new("/")), dir.path()));
        assert!(
            !ApprovalPolicy { allow: vec![], ..ApprovalPolicy::default() }
                .allows(&request("ls", dir.path()), dir.path())
        );
        assert!(
            !ApprovalPolicy::default()
                .allows(&ApprovalRequest { tool: "mcp__x__y".into(), ..request("ls", dir.path()) }, dir.path())
        );
    }

    #[test]
    #[cfg(feature = "native")]
    fn policy_resolves_symlinks_and_custom_prefixes() {
        let root = crate::test_support::tempdir().unwrap();
        let outside = crate::test_support::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("escape")).unwrap();
        for command in ["cat escape/secret", "ls escape", "cat escape/../secret"] {
            assert!(!ApprovalPolicy::default().allows(&request(command, root.path()), root.path()));
        }
        let policy = ApprovalPolicy { allow: vec!["echo".into()], ..ApprovalPolicy::default() };
        assert!(policy.allows(&request("echo hello", root.path()), root.path()));
        assert!(!policy.allows(&request("echolocation hello", root.path()), root.path()));
    }

    #[test]
    fn permission_payload_and_response_match_codex_contract() {
        let event = HookEvent::from_payload(&json!({"session_id":"s", "hook_event_name":"PermissionRequest", "cwd":"/repo", "tool_name":"Bash", "tool_input":{"command":"touch hello", "cwd":"/repo/sub", "description":"Create a test file"}})).unwrap();
        let request = ApprovalRequest::from_event(&event);
        assert_eq!(event.status(), Some(crate::session::AgentStatus::NeedsInput));
        assert_eq!(request.command.as_deref(), Some("touch hello"));
        assert_eq!(request.cwd, Path::new("/repo/sub"));
        assert_eq!(request.reason.as_deref(), Some("Create a test file"));
        for decision in [Decision::Allow, Decision::AllowAlways, Decision::Deny] {
            let output: Value = serde_json::from_str(&decision.output(Some("because"))).unwrap();
            assert_eq!(
                output["hookSpecificOutput"]["decision"]["behavior"],
                if decision == Decision::Deny { "deny" } else { "allow" }
            );
            assert_eq!(output["hookSpecificOutput"]["decision"]["message"], "because");
            assert!(output["hookSpecificOutput"]["decision"].get("updatedPermissions").is_none());
        }
    }

    #[test]
    fn recognizes_other_codex_menus_without_a_command_prompt() {
        for screen in [
            "Allow these edits?\n› 1. Yes, proceed\n  2. No",
            "Terminal input required\nPress enter to confirm or esc to cancel",
            "Permissions changed\nPress enter to confirm or\nesc to cancel",
            "Different display\n❯ 2. Allow once",
        ] {
            assert!(menu_visible(screen), "{screen}");
            assert!(menu_key(screen, Some("touch fixture"), Decision::Allow).is_none());
        }
        assert!(!menu_visible("1. A normal numbered response\n› Ask Codex to do anything"));
        assert!(!menu_visible(
            "› 1. Yes, proceed\nPress enter to confirm or esc to cancel\nWorking (esc to interrupt)\n›"
        ));
    }

    #[test]
    fn first_run_questions_are_prompts_and_an_idle_composer_is_not() {
        for screen in [
            "  Trust this folder? Codex can read, edit, and run files here, subject to your\n  permission settings.\n\n  Trust and continue\n  Open restricted",
            "> You are in /repos/app\n\n  Do you trust the files in this folder?\n\n❯ 1. Yes, proceed\n  2. No, exit",
            "✨ Update available! 0.165.0 -> 0.170.0\n\n› 1. Update now\n  2. Skip\n  3. Skip until next version",
            "  Sign in with ChatGPT to use Codex as part of your paid plan\n\n  Press Enter to continue",
        ] {
            assert!(first_run_prompt(screen), "{screen}");
        }
        for screen in ["", "› Ask Codex to do anything", "1. A numbered answer\n2. Another\n\n› Implement {feature}"]
        {
            assert!(!first_run_prompt(screen), "{screen}");
        }
    }

    #[test]
    fn fallback_expands_a_hidden_command_before_approving() {
        let screen = "Would you like to run the following command?\n\nReason: A long explanation of why the command needs access\n  [… 9 lines] ⌃a view all\n\n› 1. Yes, proceed (y)\n  2. No, and tell Codex what to do differently (esc)";
        for decision in [Decision::Allow, Decision::AllowAlways, Decision::Deny] {
            assert_eq!(menu_key(screen, Some("touch fixture"), decision).as_deref(), Some("\x01"));
        }
        assert_eq!(menu_key(&screen.replace(" ⌃a view all", ""), Some("touch fixture"), Decision::Allow), None);
        assert_eq!(menu_key(&format!("{screen}\nWorking\n›"), Some("touch fixture"), Decision::Allow), None);
    }

    #[test]
    fn fallback_only_answers_visible_matching_menu_choices() {
        let screen = "Would you like to run the following command?\n\n  $ touch fixture\n\n› 1. Yes, proceed (y)\n  2. Yes, and don't ask again for commands that start with `touch`\n  3. No, and tell Codex what to do differently (esc)";
        assert_eq!(menu_key(screen, Some("touch fixture"), Decision::Allow).as_deref(), Some("1\r"));
        assert_eq!(menu_key(screen, Some("touch fixture"), Decision::AllowAlways).as_deref(), Some("1\r"));
        assert_eq!(menu_key(screen, Some("touch fixture"), Decision::Deny).as_deref(), Some("3\r"));
        assert_eq!(menu_key("Agent is working", Some("touch fixture"), Decision::Allow), None);
        assert_eq!(menu_key(screen, Some("touch other"), Decision::Allow), None);
        assert_eq!(menu_key(&format!("{screen}\nWorking\n›"), Some("touch fixture"), Decision::Allow), None);
        let old = format!("9. Yes, proceed (y)\n{screen}");
        assert_eq!(menu_key(&old, Some("touch fixture"), Decision::Allow).as_deref(), Some("1\r"));
        let no_choice =
            "1. Yes, proceed (y)\nWould you like to run the following command?\n\n$ touch fixture\n\nRendering menu";
        assert_eq!(menu_key(no_choice, Some("touch fixture"), Decision::Allow), None);
        let clipped = "Would you like to run the following command?\n\n  $ touch fixture\n\n› 1. Yes, proceed (y)\nPress enter to confirm or esc to cancel";
        assert_eq!(menu_key(clipped, Some("touch fixture"), Decision::Deny).as_deref(), Some("\x1b"));
    }

    #[test]
    fn fallback_answers_choices_listed_right_below_the_command() {
        // Codex 0.165, as seen in the e2e run: no blank line between the command and its choices.
        let screen = "  Would you like to run the following command?\n\n  Environment: local\n\n  Reason: Allow the required check to run outside the sandbox so it can\n  access the IMACS test queue socket?\n\n  $ imacs queue --class test -- ./check.sh\n› 1. Yes, proceed (y)\n  2. Yes, and don't ask again for commands that start with `imacs queue\n     --class test -- ./check.sh` (p)\n  Press enter to confirm or esc to cancel";
        let command = Some("imacs queue --class test -- ./check.sh");
        assert_eq!(menu_key(screen, command, Decision::Allow).as_deref(), Some("1\r"));
        assert_eq!(menu_key(screen, command, Decision::AllowAlways).as_deref(), Some("1\r"));
        assert_eq!(menu_key(screen, command, Decision::Deny).as_deref(), Some("\x1b"));
        assert_eq!(menu_key(screen, Some("imacs queue --class test -- ./other.sh"), Decision::Allow), None);
        let wrapped = screen.replace("./check.sh\n›", "./check.sh --all\n  --verbose\n›");
        assert_eq!(
            menu_key(&wrapped, Some("imacs queue --class test -- ./check.sh --all --verbose"), Decision::Allow)
                .as_deref(),
            Some("1\r")
        );
        assert_eq!(menu_key(&wrapped, command, Decision::Allow), None);
    }

    #[test]
    fn fallback_expands_truncated_heredocs_and_requires_the_full_command() {
        let command = format!("python3 - <<'PY'\nfrom pathlib import Path\n{}PY", "print('fixture')\n".repeat(46));
        let screen = "Would you like to run the following command?\n\n  $ python3 - <<'PY'\n  from pathlib import Path\n  [… 46 lines] ⌃a view all\n\n› 1. Yes, proceed (y)\n  2. No, and tell Codex what to do differently (esc)\nPress enter to confirm or esc to cancel";
        for decision in [Decision::Allow, Decision::AllowAlways, Decision::Deny] {
            assert_eq!(menu_key(screen, Some(&command), decision).as_deref(), Some("\x01"));
        }
        let different = command.replace("print('fixture')", "print('different command')");
        assert_eq!(menu_key(screen, Some(&different), Decision::Allow).as_deref(), Some("\x01"));
        let expanded =
            screen.replace("python3 - <<'PY'\n  from pathlib import Path\n  [… 46 lines] ⌃a view all", &command);
        assert_eq!(menu_key(&expanded, Some(&command), Decision::Allow).as_deref(), Some("1\r"));
        assert_eq!(menu_key(&expanded, Some(&different), Decision::Allow), None);
        assert_eq!(menu_key(screen, Some("python3 - <<'PY'\nimport os\nPY"), Decision::Allow), None);
        let no_hint = screen.replace(" ⌃a view all", "");
        assert_eq!(menu_key(&no_hint, Some(&command), Decision::Allow), None);
        for marker in ["[… lines]", "[… many lines]", "[… 46 lines] unrelated text"] {
            assert_eq!(
                menu_key(&screen.replace("[… 46 lines] ⌃a view all", marker), Some(&command), Decision::Allow),
                None
            );
        }
        let no_prefix = screen.replace("python3 - <<'PY'\n  from pathlib import Path\n  ", "");
        assert_eq!(menu_key(&no_prefix, Some(&command), Decision::Allow), None);
    }
}
