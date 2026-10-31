//! The worktree's file tree and fuzzy file matching for the file picker.

use std::collections::{BTreeMap, HashSet};

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Dir {
    pub dirs: BTreeMap<String, Dir>,
    /// File names in this directory, sorted.
    pub files: Vec<String>,
    /// Changed files anywhere below this directory.
    pub changed: usize,
}

/// Builds the tree from slash-separated paths, counting `changed` files per directory.
pub fn build<P: AsRef<str>>(paths: &[P], changed: &HashSet<&str>) -> Dir {
    let mut root = Dir::default();
    for path in paths {
        let path = path.as_ref();
        let is_changed = changed.contains(path);
        let mut dir = &mut root;
        if is_changed {
            dir.changed += 1;
        }
        let mut parts = path.split('/').peekable();
        while let Some(part) = parts.next() {
            if parts.peek().is_none() {
                dir.files.push(part.to_string());
            } else {
                dir = dir.dirs.entry(part.to_string()).or_default();
                if is_changed {
                    dir.changed += 1;
                }
            }
        }
    }
    sort(&mut root);
    root
}

fn sort(dir: &mut Dir) {
    dir.files.sort_by_key(|name| name.to_lowercase());
    dir.files.dedup();
    for child in dir.dirs.values_mut() {
        sort(child);
    }
}

/// `a/b/c.rs` → `a`, `a/b`.
pub fn ancestors(path: &str) -> impl Iterator<Item = &str> {
    path.match_indices('/').map(move |(i, _)| &path[..i])
}

/// Subsequence match of `query` in `path`, case-insensitive. Higher is better;
/// `None` when the query letters do not all appear in order. Matches in the file
/// name, at word starts and in runs score higher; long paths score lower.
pub fn fuzzy(query: &str, path: &str) -> Option<i64> {
    let query: Vec<char> = query.chars().filter(|c| !c.is_whitespace()).flat_map(char::to_lowercase).collect();
    if query.is_empty() {
        return Some(0);
    }
    let chars: Vec<char> = path.chars().collect();
    let name_start = path.rfind('/').map(|i| path[..=i].chars().count()).unwrap_or(0);
    let mut score: i64 = 0;
    let mut q = 0;
    let mut previous: Option<usize> = None;
    for (i, c) in chars.iter().enumerate() {
        if q == query.len() {
            break;
        }
        if c.to_lowercase().next() != Some(query[q]) {
            continue;
        }
        let mut points = 1;
        if previous == Some(i.wrapping_sub(1)) {
            points += 5;
        }
        let boundary = i == 0 || matches!(chars[i - 1], '/' | '_' | '-' | '.' | ' ');
        let camel = i > 0 && c.is_uppercase() && chars[i - 1].is_lowercase();
        if boundary || camel {
            points += 8;
        }
        if i >= name_start {
            points += 4;
        }
        score += points;
        previous = Some(i);
        q += 1;
    }
    (q == query.len()).then(|| score * 16 - chars.len() as i64)
}

/// The best `limit` matches for `query`, best first.
pub fn best_matches<'a, P: AsRef<str>>(query: &str, paths: &'a [P], limit: usize) -> Vec<&'a str> {
    let mut scored: Vec<(i64, &str)> =
        paths.iter().map(AsRef::as_ref).filter_map(|p| fuzzy(query, p).map(|s| (s, p))).collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));
    scored.into_iter().take(limit).map(|(_, p)| p).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(list: &[&str]) -> Vec<String> {
        list.iter().map(|p| p.to_string()).collect()
    }

    #[test]
    fn tree_nests_and_counts_changes() {
        let all = paths(&["README.md", "src/app.rs", "src/ui/tile.rs", "src/ui/tabs.rs", "tests/a.rs"]);
        let changed: HashSet<&str> = ["src/ui/tile.rs", "README.md"].into_iter().collect();
        let tree = build(&all, &changed);
        assert_eq!(tree.files, ["README.md"]);
        assert_eq!(tree.changed, 2);
        let src = &tree.dirs["src"];
        assert_eq!(src.files, ["app.rs"]);
        assert_eq!(src.changed, 1);
        assert_eq!(src.dirs["ui"].files, ["tabs.rs", "tile.rs"]);
        assert_eq!(tree.dirs["tests"].changed, 0);
    }

    #[test]
    fn ancestors_of_a_path() {
        assert_eq!(ancestors("a/b/c.rs").collect::<Vec<_>>(), ["a", "a/b"]);
        assert_eq!(ancestors("c.rs").count(), 0);
    }

    #[test]
    fn fuzzy_prefers_file_names_and_word_starts() {
        let all = paths(&["src/diff_view.rs", "src/divide/fever.rs", "docs/diff-viewer-notes.md", "Cargo.toml"]);
        assert_eq!(best_matches("dv", &all, 1), ["src/diff_view.rs"]);
        assert_eq!(best_matches("cargo", &all, 5), ["Cargo.toml"]);
        assert!(fuzzy("xyz", "src/diff_view.rs").is_none());
        assert_eq!(best_matches("", &all, 2).len(), 2);
    }
}
