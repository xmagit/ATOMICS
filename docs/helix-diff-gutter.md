# Helix diff gutters

ATOMICS reviews the worktree against its merge-base with `base_ref`. Helix 25.07.1's
gutter compares a tracked file with **HEAD**, including unsaved edits. Consequently,
committed branch changes can appear in ATOMICS while Helix has no gutter marker.
Files absent from HEAD, including untracked files, have no Helix diff base either.

This follows Helix's versioned implementation: [`get_diff_base` reads
`head_commit`, and `find_file_in_commit` rejects untracked paths](https://github.com/helix-editor/helix/blob/25.07.1/helix-vcs/src/git.rs#L26-L53).
There is no diff-base setting in that release. Upstream
[#5825](https://github.com/helix-editor/helix/issues/5825) requests alternative
baseline selection, specifically the index instead of HEAD. Changing ATOMICS's
startup theme or Git environment does not select a review baseline in Helix.

The gutter is enabled by the default Helix setting:

```toml
[editor]
gutters = ["diagnostics", "spacer", "line-numbers", "spacer", "diff"]
```

See the [25.07.1 configuration documentation](https://github.com/helix-editor/helix/blob/25.07.1/book/src/editor.md#editor-section).
An existing custom `gutters` list must include `diff` to show markers. This setting
enables the gutter; it does not change its baseline.

## Linux investigation, 2026-10-31

Tested CachyOS Helix `25.07.1 (4746c276)` with `TERM=xterm-256color`, using
`script -qc` to capture the terminal after two seconds. A scratch repository had
one committed three-line file and a modification on line two. Both themes drew
the `▍` marker on that changed line:

| Repository layout | Default theme | `:theme base16_transparent` |
|---|---|---|
| Plain checkout | Marker present | Marker present |
| Linked worktree | Marker present | Marker present |
| Linked worktree with a 170-character path | Marker present | Marker present |
| Linked worktree with 53 linked worktrees in the main repository | Marker present | Marker present |

Further captures with ATOMICS's terminal environment and startup theme:

| Control | Result |
|---|---|
| The scratch change after committing it | No marker |
| An untracked scratch file | No marker |
| A tracked, modified file in the real ATOMICS worktree | Marker present |
| A tracked file in the affected Foundation worktree, now clean | No marker |
| An unsaved edit to that Foundation file, discarded on exit | Marker present |

The original missing-marker symptom for a tracked file modified against HEAD
was not reproduced. The affected worktree was clean when inspected; the earlier
uncommitted state was no longer available. The captures demonstrate the baseline
limitation and rule out path length, worktree count, and the startup theme in
these cases. No Helix configuration, repository state, or editor environment was
changed to force markers.

The targeted test
`app::editor::tests::helix_diff_gutter_uses_head_in_a_linked_worktree_with_startup_theme`
also starts the installed Helix through ATOMICS's real editor launcher and reads
its native terminal screen. It checks modified, committed, and untracked files
with the normal startup theme. It reports a skip if Helix is not installed.
