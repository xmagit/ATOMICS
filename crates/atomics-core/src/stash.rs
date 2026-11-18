//! The stash every worktree of a repository shares: one `refs/stash` and its log, so `stash@{n}`
//! names whichever entry is n-th when git runs, whoever made it. ATOMICS finds the git commands
//! that drop entries, so they wait for the chief or the user, and says whose entries they would
//! drop when it can tell.

#[cfg(feature = "native")]
use std::path::Path;

/// A git command that drops entries of the shared stash.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Drop,
    Pop,
    /// `git stash branch`, which drops the entry once it applies.
    Branch,
    Clear,
}

impl Action {
    pub fn command(self) -> &'static str {
        match self {
            Self::Drop => "git stash drop",
            Self::Pop => "git stash pop",
            Self::Branch => "git stash branch",
            Self::Clear => "git stash clear",
        }
    }

    fn verb(self) -> &'static str {
        match self {
            Self::Drop => "drops",
            Self::Pop => "applies and drops",
            Self::Branch => "makes a branch from and drops",
            Self::Clear => "drops",
        }
    }
}

/// The entries a command drops.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// The entry at this place: `stash@{n}` or `n`, or the newest when the command names none.
    Index(usize),
    /// Every entry: `git stash clear`.
    All,
    /// Something else, such as `stash@{yesterday}`.
    Other(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StashDrop {
    /// The folders git runs in, in order, each relative to the one before: the script's `cd`s
    /// before the command, then the command's `git -C` folders. They may start with `~`.
    pub dirs: Vec<String>,
    /// The repository git is pointed at, relative to the last of `dirs`: the command's
    /// `--git-dir`, else a `GIT_DIR=` before it, else the script's `export GIT_DIR=`.
    pub git_dir: Option<String>,
    pub action: Action,
    pub target: Target,
}

/// Where a script has run git so far: the folders its `cd`s went to and the `GIT_DIR` it exported.
/// One script is read in order, as one shell; a subshell's `cd` is taken to last too.
#[derive(Debug, Clone, Default)]
struct Shell {
    dirs: Vec<String>,
    git_dir: Option<String>,
}

/// The git commands in the shell `command` that drop entries of the shared stash. It reads more
/// shell than the approval policy accepts (pipes, substitutions, `sh -c` scripts), because a
/// command it misses here would not wait for anyone.
pub fn drops(command: &str) -> Vec<StashDrop> {
    drops_in(command, &mut Shell::default())
}

fn drops_in(command: &str, shell: &mut Shell) -> Vec<StashDrop> {
    let mut found = Vec::new();
    for words in segments(command) {
        let (program, args) = match words.split_first() {
            Some((program, args)) => (program.as_str(), args),
            None => continue,
        };
        match program {
            "cd" | "pushd" => {
                let dir = args.iter().find(|arg| !arg.starts_with('-') || *arg == "-");
                shell.dirs.push(dir.map_or("~", String::as_str).to_string());
                continue;
            }
            "export" => {
                if let Some(dir) = args.iter().rev().find_map(|arg| arg.strip_prefix("GIT_DIR=")) {
                    shell.git_dir = Some(dir.to_string());
                }
            }
            "unset" if args.iter().any(|arg| arg == "GIT_DIR") => shell.git_dir = None,
            _ => {}
        }
        for (at, word) in words.iter().enumerate() {
            let program = word.rsplit('/').next().unwrap_or(word);
            if program == "git" {
                // `GIT_DIR=… git`, `env GIT_DIR=… git`: an assignment for this command alone.
                let git_dir = words[..at].iter().rev().find_map(|word| word.strip_prefix("GIT_DIR="));
                let git_dir = git_dir.map(str::to_string).or_else(|| shell.git_dir.clone());
                found.extend(git_drop(&words[at + 1..], shell, git_dir));
            } else if matches!(program, "sh" | "bash" | "zsh" | "dash" | "ksh")
                // `-c`, or with other options: `-lc`, `-ec`.
                && let Some(script) = words[at + 1..]
                    .iter()
                    .skip_while(|word| !(word.starts_with('-') && !word.starts_with("--") && word.contains('c')))
                    .nth(1)
            {
                // A shell of its own, started where this one is: its `cd`s end with it.
                found.extend(drops_in(script, &mut shell.clone()));
            } else if program == "eval" {
                // The rest of the segment is the command eval runs: read it once, as eval does,
                // in this shell.
                found.extend(drops_in(&words[at + 1..].join(" "), shell));
                break;
            }
        }
    }
    found
}

/// The drop `git <args>` makes, if any, run where `shell` is with `git_dir` in its environment.
fn git_drop(args: &[String], shell: &Shell, mut git_dir: Option<String>) -> Option<StashDrop> {
    let mut dirs = shell.dirs.clone();
    let mut rest = args;
    loop {
        match rest.first().map(String::as_str) {
            Some("-C") => {
                dirs.push(rest.get(1)?.clone());
                rest = &rest[2..];
            }
            Some("--git-dir") => {
                git_dir = Some(rest.get(1)?.clone());
                rest = &rest[2..];
            }
            Some(option) if option.starts_with("--git-dir=") => {
                git_dir = option.strip_prefix("--git-dir=").map(str::to_string);
                rest = &rest[1..];
            }
            Some("-c" | "--work-tree" | "--namespace" | "--config-env" | "--super-prefix") => {
                rest = rest.get(2..)?;
            }
            Some(option) if option.starts_with('-') => rest = &rest[1..],
            _ => break,
        }
    }
    let (subcommand, args) = rest.split_first()?;
    let (stash, args) = args.split_first()?;
    if subcommand != "stash" {
        return None;
    }
    let action = match stash.as_str() {
        "drop" => Action::Drop,
        "pop" => Action::Pop,
        "branch" => Action::Branch,
        "clear" => Action::Clear,
        _ => return None,
    };
    let mut named = args.iter().filter(|arg| !arg.starts_with('-'));
    let target = match action {
        Action::Clear => Target::All,
        // `git stash branch <branch> [<stash>]`.
        Action::Branch => named.nth(1).map_or(Target::Index(0), |name| target(name)),
        Action::Drop | Action::Pop => named.next().map_or(Target::Index(0), |name| target(name)),
    };
    Some(StashDrop { dirs, git_dir, action, target })
}

fn target(name: &str) -> Target {
    let index = name
        .strip_prefix("stash@{")
        .or_else(|| name.strip_prefix("refs/stash@{"))
        .and_then(|rest| rest.strip_suffix('}'))
        .unwrap_or(name);
    match index.parse() {
        // Only digits: `parse` takes a sign too.
        Ok(at) if index.bytes().all(|b| b.is_ascii_digit()) => Target::Index(at),
        _ => Target::Other(name.to_string()),
    }
}

/// The shell's simple commands in `command`, as unquoted words: split at `;`, `&`, `|`, newlines,
/// parentheses, backquotes and `$(`. A redirection is no word of its command: `2>&1`, `>/dev/null`,
/// `&> log` and `<<EOF` go, file descriptor and target alike.
fn segments(command: &str) -> Vec<Vec<String>> {
    // A word just ended: it is the command's, unless it is where a redirection goes.
    fn finish(words: &mut Vec<String>, word: &mut Option<String>, redirect: &mut bool) {
        if let Some(done) = word.take()
            && !std::mem::take(redirect)
        {
            words.push(done);
        }
    }
    let mut segments = Vec::new();
    let mut words = Vec::new();
    let mut word: Option<String> = None;
    let mut quote = None;
    // The next word is a redirection's target.
    let mut redirect = false;
    let mut chars = command.chars().peekable();
    while let Some(c) = chars.next() {
        if quote.is_none() && (matches!(c, '>' | '<') || (c == '&' && chars.peek() == Some(&'>'))) {
            // Digits just before `>` or `<` are the file descriptor it redirects: `2>`, `1>>`.
            if c != '&'
                && word.as_deref().is_some_and(|word| !word.is_empty() && word.bytes().all(|b| b.is_ascii_digit()))
            {
                word = None;
            }
            finish(&mut words, &mut word, &mut redirect);
            // The rest of the operator: `>>`, `>&`, `>|`, `<<`, `<<<`, `<&`, `<>`, `&>>`.
            while chars.next_if(|next| matches!(next, '>' | '<' | '&' | '|' | '-')).is_some() {}
            redirect = true;
            continue;
        }
        let boundary =
            matches!(c, ';' | '&' | '|' | '(' | ')' | '`' | '\n') || (c == '$' && chars.peek() == Some(&'('));
        match (quote, c) {
            (Some('\''), '\'') | (Some('"'), '"') => quote = None,
            (Some('"'), '\\') if chars.peek().is_some_and(|next| matches!(next, '"' | '\\' | '$' | '`')) => {
                word.get_or_insert_default().extend(chars.next());
            }
            (Some(_), c) => word.get_or_insert_default().push(c),
            (None, '\'' | '"') => {
                word.get_or_insert_default();
                quote = Some(c);
            }
            (None, '\\') => match chars.next() {
                Some('\n') | None => {}
                Some(next) => word.get_or_insert_default().push(next),
            },
            (None, _) if boundary => {
                finish(&mut words, &mut word, &mut redirect);
                redirect = false;
                if !words.is_empty() {
                    segments.push(std::mem::take(&mut words));
                }
            }
            (None, c) if c.is_whitespace() => finish(&mut words, &mut word, &mut redirect),
            (None, c) => word.get_or_insert_default().push(c),
        }
    }
    finish(&mut words, &mut word, &mut redirect);
    if !words.is_empty() {
        segments.push(words);
    }
    segments
}

/// An entry of the shared stash, as `git stash list` shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Its place now, `stash@{index}`: a push or drop in any checkout moves it.
    pub index: usize,
    pub commit: String,
    /// What git logged when it was made: `On <branch>: <message>`, or
    /// `WIP on <branch>: <commit> <subject>` when it was given no message.
    pub subject: String,
}

impl Entry {
    /// The branch it was made on, unless HEAD was detached.
    pub fn branch(&self) -> Option<&str> {
        let rest = self.subject.strip_prefix("On ").or_else(|| self.subject.strip_prefix("WIP on "))?;
        // A branch name never holds a colon.
        let (branch, _) = rest.split_once(": ")?;
        (branch != "(no branch)").then_some(branch)
    }

    /// The message `git stash push -m` gave it.
    pub fn message(&self) -> Option<&str> {
        self.subject.strip_prefix("On ")?.split_once(": ").map(|(_, message)| message)
    }
}

/// "1 entry", "2 entries".
pub fn entries(count: usize) -> String {
    if count == 1 { "1 entry".into() } else { format!("{count} entries") }
}

/// The format [`parse`] reads: `git stash list --format=` this.
pub const FORMAT: &str = "%H%x00%gs";

/// The entries in `git stash list --format=`[`FORMAT`]'s output, newest first.
pub fn parse(output: &str) -> Vec<Entry> {
    output
        .lines()
        .filter_map(|line| line.split_once('\0'))
        .enumerate()
        .map(|(index, (commit, subject))| Entry { index, commit: commit.into(), subject: subject.into() })
        .collect()
}

/// The shared stash of `dir`'s repository, newest first. Git logs one line per entry: it folds
/// the whitespace of a message, newlines included.
#[cfg(feature = "native")]
pub fn list(dir: &Path) -> anyhow::Result<Vec<Entry>> {
    list_in(dir, None)
}

/// [`list`] of the repository `git_dir` names from `dir`, as `git --git-dir` (or `GIT_DIR`) does.
#[cfg(feature = "native")]
pub fn list_in(dir: &Path, git_dir: Option<&Path>) -> anyhow::Result<Vec<Entry>> {
    let format = format!("--format={FORMAT}");
    let checkout;
    let dir = if let Some(location) = git_dir {
        let admin = dir.join(location).canonicalize()?;
        checkout = if admin.file_name().is_some_and(|name| name == ".git") {
            admin.parent().unwrap().to_owned()
        } else {
            let back = crate::git::read_regular_file(&admin.join("gitdir"), 4096)?;
            let dot = admin.join(back.trim_end()).canonicalize()?;
            anyhow::ensure!(dot.file_name().is_some_and(|name| name == ".git"), "invalid worktree gitdir");
            dot.parent().unwrap().to_owned()
        };
        let pins = crate::git::resolve_pins(&checkout)?;
        anyhow::ensure!(pins.git_dir.as_ref() == Some(&admin), "gitdir does not belong to this checkout");
        checkout.as_path()
    } else {
        dir
    };
    Ok(parse(&crate::git::git(dir, &["-c", "log.showSignature=false", "stash", "list", &format])?))
}

/// The commits of `entries` (the stash as listed now) that `drop` takes: the entry at its index (none
/// while there is no such entry), every entry for a clear, and none for a name ATOMICS cannot place.
/// What a warning named, by commit: once anyone stashes or drops, the same index names another entry.
pub fn taken(drop: &StashDrop, entries: &[Entry]) -> Vec<String> {
    match &drop.target {
        Target::Index(index) => entries.get(*index).map(|entry| entry.commit.clone()).into_iter().collect(),
        Target::All => entries.iter().map(|entry| entry.commit.clone()).collect(),
        Target::Other(_) => Vec::new(),
    }
}

/// Whose an entry is, as far as ATOMICS can tell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Owner {
    /// The agent whose command would drop it.
    Requester,
    /// Another one, named: "task t2 (session worker-two)".
    Known(String),
    Unknown,
}

impl Owner {
    fn whose(&self) -> String {
        match self {
            Self::Requester => "this agent's own".into(),
            Self::Known(owner) => format!("which belongs to {owner}, not to this agent"),
            Self::Unknown => "whose owner ATOMICS cannot tell".into(),
        }
    }
}

/// The first line of every [`warning`].
pub const SHARED: &str =
    "Every worktree of this repository shares one stash: an index names whichever entry is there when git runs.";

/// The most entries a warning about `git stash clear` names.
const SHOWN: usize = 10;

/// What an approval of a command that drops stash entries says: for each drop, the entries it
/// takes from the stash as listed now, or why that could not be read, and whose they are.
pub fn warning<'a>(
    drops: impl IntoIterator<Item = (&'a StashDrop, Result<&'a [Entry], &'a str>)>,
    owner: impl Fn(&Entry) -> Owner,
) -> String {
    let shown = |entry: &Entry| format!("stash@{{{}}} ({}), {}", entry.index, entry.subject, owner(entry).whose());
    let mut lines = vec![SHARED.to_string()];
    for (drop, listed) in drops {
        let (command, verb) = (drop.action.command(), drop.action.verb());
        lines.push(match (listed, &drop.target) {
            (Err(error), _) => format!("{command}: ATOMICS could not list the stash: {error}"),
            (Ok([]), Target::All) => format!("{command} {verb} every entry; there are none now."),
            (Ok(entries), Target::All) => {
                let mut named: Vec<_> = entries.iter().take(SHOWN).map(shown).collect();
                if entries.len() > SHOWN {
                    named.push(format!("and {} more", entries.len() - SHOWN));
                }
                format!("{command} {verb} all {}: {}.", self::entries(entries.len()), named.join("; "))
            }
            (Ok(entries), Target::Index(index)) => match entries.get(*index) {
                Some(entry) => format!("{command} {verb} {}.", shown(entry)),
                None => format!("{command} names stash@{{{index}}}, which does not exist now."),
            },
            (Ok(_), Target::Other(name)) => format!("{command} names {name}, which ATOMICS cannot place in the stash."),
        });
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn found(command: &str) -> Vec<(Action, Target)> {
        drops(command).into_iter().map(|drop| (drop.action, drop.target)).collect()
    }

    #[test]
    fn drops_pops_branches_and_clears_are_found_by_the_entry_they_name() {
        use Action::*;
        use Target::*;
        for (command, expected) in [
            ("git stash drop", vec![(Drop, Index(0))]),
            ("git stash drop stash@{2}", vec![(Drop, Index(2))]),
            ("git stash drop -q 'stash@{3}'", vec![(Drop, Index(3))]),
            ("git stash pop 1", vec![(Pop, Index(1))]),
            ("git stash pop --index refs/stash@{4}", vec![(Pop, Index(4))]),
            ("git stash branch rescue", vec![(Branch, Index(0))]),
            ("git stash branch rescue stash@{5}", vec![(Branch, Index(5))]),
            ("git stash clear", vec![(Clear, All)]),
            ("git stash drop stash@{yesterday}", vec![(Drop, Other("stash@{yesterday}".into()))]),
            ("git stash drop +1", vec![(Drop, Other("+1".into()))]),
            ("/usr/bin/git --no-pager -c color.ui=never stash pop", vec![(Pop, Index(0))]),
            ("cd sub && git stash list && git stash drop stash@{1}", vec![(Drop, Index(1))]),
            ("git stash list | head; git stash clear", vec![(Clear, All)]),
            ("echo $(git stash pop)", vec![(Pop, Index(0))]),
            ("bash -lc 'git stash drop 2'", vec![(Drop, Index(2))]),
            ("eval git stash clear", vec![(Clear, All)]),
            ("eval 'git stash pop'", vec![(Pop, Index(0))]),
            ("git stash drop 1 && git stash drop 0", vec![(Drop, Index(1)), (Drop, Index(0))]),
            // A redirection is neither the entry nor the end of the command.
            ("git stash drop 2>&1", vec![(Drop, Index(0))]),
            ("git stash drop stash@{1} 2>&1 | tail -1", vec![(Drop, Index(1))]),
            ("git stash pop >/dev/null 2>&1 && git stash drop 3", vec![(Pop, Index(0)), (Drop, Index(3))]),
            ("git stash drop > out.txt 2", vec![(Drop, Index(2))]),
            ("git stash drop 1>/dev/null", vec![(Drop, Index(0))]),
            ("git stash drop 1 >/dev/null", vec![(Drop, Index(1))]),
            ("git stash drop &>/dev/null", vec![(Drop, Index(0))]),
            ("git stash pop 2>> log 4", vec![(Pop, Index(4))]),
            ("git stash drop <<< 'y' 1", vec![(Drop, Index(1))]),
            ("git stash branch rescue 2>/dev/null stash@{1}", vec![(Branch, Index(1))]),
        ] {
            assert_eq!(found(command), expected, "{command}");
        }
        for command in [
            "git stash",
            "git stash push -m ENG-123",
            "git stash list",
            "git stash show -p stash@{0}",
            "git stash apply stash@{1}",
            "git status && git log",
            "echo 'git stash drop'",
            "git commit -m 'git stash drop later'",
            "gitk stash drop",
        ] {
            assert!(drops(command).is_empty(), "{command}");
        }
        let place =
            |command: &str| drops(command).into_iter().map(|drop| (drop.dirs, drop.git_dir)).collect::<Vec<_>>();
        let dirs = |dirs: &[&str]| dirs.iter().map(|dir| dir.to_string()).collect::<Vec<_>>();
        assert_eq!(place("git -C ../other --git-dir .git stash clear"), [(dirs(&["../other"]), Some(".git".into()))]);
        // Where a script's `cd`s and `GIT_DIR` put git, in order.
        assert_eq!(place("cd ../other && git stash drop"), [(dirs(&["../other"]), None)]);
        assert_eq!(place("cd a; cd -P b && git -C c stash pop"), [(dirs(&["a", "b", "c"]), None)]);
        assert_eq!(place("cd && git stash clear"), [(dirs(&["~"]), None)]);
        assert_eq!(place("GIT_DIR=/r/.git git stash drop"), [(vec![], Some("/r/.git".into()))]);
        assert_eq!(place("env GIT_DIR=x git --git-dir=y stash drop"), [(vec![], Some("y".into()))]);
        assert_eq!(
            place("export GIT_DIR=/r/.git; git stash drop; unset GIT_DIR; git stash drop"),
            [(vec![], Some("/r/.git".into())), (vec![], None)]
        );
        // `sh -c` starts where the script is, and its `cd`s end with it; `eval`'s do not.
        assert_eq!(
            place("cd a && bash -c 'cd b && git stash drop' && git stash pop"),
            [(dirs(&["a", "b"]), None), (dirs(&["a"]), None)]
        );
        assert_eq!(place("eval cd b; git stash drop"), [(dirs(&["b"]), None)]);
    }

    #[test]
    fn entries_name_their_branch_and_message() {
        let entries = parse(
            "aaa\0On fix/one: ENG-123 before rebase\nbbb\0WIP on main: 1234567 base\nccc\0On (no branch): detached\n",
        );
        assert_eq!(entries.len(), 3);
        assert_eq!((entries[0].index, entries[0].commit.as_str()), (0, "aaa"));
        assert_eq!(entries[0].branch(), Some("fix/one"));
        assert_eq!(entries[0].message(), Some("ENG-123 before rebase"));
        assert_eq!(entries[1].branch(), Some("main"));
        assert_eq!(entries[1].message(), None);
        assert_eq!(entries[2].branch(), None);
        assert!(parse("").is_empty());
    }

    #[test]
    fn a_warning_names_the_owner_of_each_entry_a_command_drops() {
        let entries = parse("aaa\0On atomics/t2: t2\nbbb\0On fix/mine: t1\nccc\0WIP on main: 1234567 base\n");
        let owner = |entry: &Entry| match entry.branch() {
            Some("fix/mine") => Owner::Requester,
            Some("atomics/t2") => Owner::Known("task t2 (session worker-two)".into()),
            _ => Owner::Unknown,
        };
        let warn = |command: &str| {
            let drops = drops(command);
            warning(drops.iter().map(|drop| (drop, Ok(&entries[..]))), owner)
        };
        let text = warn("git stash drop stash@{0}");
        assert!(text.starts_with(SHARED), "{text}");
        assert!(
            text.ends_with(
                "\ngit stash drop drops stash@{0} (On atomics/t2: t2), which belongs to task t2 (session worker-two), \
                 not to this agent."
            ),
            "{text}"
        );
        assert!(warn("git stash pop 1").ends_with("applies and drops stash@{1} (On fix/mine: t1), this agent's own."));
        assert!(warn("git stash drop 2").ends_with("(WIP on main: 1234567 base), whose owner ATOMICS cannot tell."));
        assert!(warn("git stash drop 7").ends_with("names stash@{7}, which does not exist now."));
        assert!(
            warn("git stash drop stash@{now}").ends_with("names stash@{now}, which ATOMICS cannot place in the stash.")
        );
        let clear = warn("git stash clear");
        assert!(
            clear.contains("drops all 3 entries: stash@{0} (On atomics/t2: t2), which belongs to task t2"),
            "{clear}"
        );
        assert!(clear.contains("; stash@{1} (On fix/mine: t1), this agent's own; stash@{2}"), "{clear}");
        let two = warn("git stash drop 1; git stash drop 0");
        assert_eq!(two.lines().count(), 3, "{two}");
        let many: Vec<_> =
            (0..12).map(|index| Entry { index, commit: "x".into(), subject: "On main: x".into() }).collect();
        let cleared = drops("git stash clear");
        let text = warning([(&cleared[0], Ok(&many[..]))], owner);
        assert!(
            text.contains("stash@{9}") && !text.contains("stash@{10}") && text.ends_with("; and 2 more."),
            "{text}"
        );
        // What each drop takes, by commit, as the warning names it.
        let taken = |command: &str| drops(command).iter().map(|drop| super::taken(drop, &entries)).collect::<Vec<_>>();
        assert_eq!(taken("git stash pop 1"), [vec!["bbb".to_string()]]);
        assert_eq!(taken("git stash drop"), [vec!["aaa".to_string()]]);
        assert_eq!(taken("git stash clear"), [vec!["aaa".to_string(), "bbb".into(), "ccc".into()]]);
        assert_eq!(taken("git stash drop 7; git stash drop stash@{now}"), [Vec::<String>::new(), Vec::new()]);
        let none = drops("git stash clear");
        let empty: &[Entry] = &[];
        assert!(warning([(&none[0], Ok(empty))], owner).ends_with("every entry; there are none now."));
        assert!(warning([(&none[0], Err("not a git repository"))], owner).ends_with("not a git repository"));
    }

    #[test]
    #[cfg(all(feature = "native", any(target_os = "macos", target_os = "linux")))]
    fn list_reads_the_stash_every_worktree_shares() {
        let (dir, repo) = crate::git::tests::project();
        let git = |dir: &Path, args: &[&str]| crate::git::git(dir, args).unwrap();
        let linked = dir.path().join("linked");
        git(&repo, &["worktree", "add", "-q", "-b", "atomics/t2", linked.to_str().unwrap()]);
        assert!(list(&repo).unwrap().is_empty());
        std::fs::write(repo.join("a.txt"), "main change\n").unwrap();
        git(&repo, &["stash", "push", "-q", "-m", "t1"]);
        std::fs::write(linked.join("a.txt"), "linked change\n").unwrap();
        git(&linked, &["stash", "push", "-q", "-m", "t2"]);
        for checkout in [&repo, &linked] {
            let entries = list(checkout).unwrap();
            let subjects: Vec<_> = entries.iter().map(|entry| entry.subject.as_str()).collect();
            assert_eq!(subjects, ["On atomics/t2: t2", "On main: t1"]);
        }
        assert!(list(&dir.path().join("missing")).is_err());
    }
}
