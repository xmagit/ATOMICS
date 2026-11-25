# Navigation

How a user moves around ATOMICS: between plans, their tasks, agent sessions, the chief, settings and a session's
diff. The first part is the audit of how it worked before this change; the second is what changed. The UI is drawn
once in `atomics-ui`, so the desktop, a browser on a computer and a tablet behave the same; the phone has its own pane
bar. The keys are the last section.

## Audit (before)

| Going to | How | Where it fell short |
| --- | --- | --- |
| A plan | The Boards dropdown in the top bar: boards that need you first, then the active ones. History, in the same menu, for finished plans, and **Open board** on a finished task's tile. | Every board took a tab, the CHIEF pill moved sideways as tabs came and went, and the bar never stood still. |
| A plan's board | **Board** in the view switcher, `cmd+b`, or the board's row in the dropdown. | Picking a plan switched the panel back to the diff, whatever the user had chosen. |
| A task | Its card on the board; `open` and `diff` buttons on the card. | A click on the card only opened or closed it. Nothing marked the card of the session on screen. |
| A session | The sidebar's AGENTS list, a tile, the tab strip, `cmd+1…9`, `cmd+j`, the notice line. | The sidebar was one flat list; which plan a session worked for, and which ones needed the user, was only in a status word. |
| The chief | The CHIEF pill (`cmd+o`); on the phone a Chief pane in the bottom bar. | The phone and the desktop put it in different places. |
| A diff | **Show diff**/**Hide diff**, `cmd+d`, a tile's title, `shift+↓/↑`, the explorer. | Three buttons (Show diff, Plan, Tickets) toggled one panel in three different ways. |
| Settings | The Settings window; a gear beside Actions when the folded bar has room, otherwise inside Actions. | — |
| Where am I | The selected tab or the name on the Boards button, the active tile's header, the diff panel's title. | No single line said plan › task › session. |
| Back | — | No way back to the place before: opening a task's diff from the board lost the board. |

## Result

- **The CHIEF pill** has a fixed width for each of three sizes, picked by the window's width alone, and always sits
  in the middle of the top bar. The brand and the navigation share the half on its left, the actions the half on its
  right; what does not fit folds into the **Boards** and **Actions** menus. The load gauge, which opens Machine, stays
  in the bar wherever it fits: among the actions, or beside their folded menu, before an update under way; **Machine**
  stays in the menu too. On the phone the pill sits in the middle of
  the top bar too; a tap opens the chief's pane, a second tap goes back to the pane before it.
- **One Boards dropdown** replaces a tab per plan. Its button names the board on screen and counts the boards that need
  the user, plans waiting for approval among them. They come first in its menu, then the other open boards with
  done/total and state, then the done ones, then History with the archived ones. A row shows its board whichever view
  the panel was on, and the board's button approves the plan. A row under the pointer offers **Complete** and
  **Archive** in words (a done board, Archive). Finished boards stay out of the bar's button; **Open board** on a
  finished task's tile reaches them too. Ctrl+1 to 9 closes the menu along with picking the agent. A finished task's
  tile closes with its ×, or all with **Hide all**; on a done board they stay out of sight. **Show finished agents** in
  the board's BOARDS menu brings them back.
- **Complete and Archive** work on any plan, from the board's header, a BOARDS row under the pointer and the dropdown.
  With nothing at stake (every task accepted, no agent at work) one click does it, and an Undo shows for a few
  seconds. Otherwise a short confirmation says what happens: the tasks not accepted, the agents at work (completing
  closes those of accepted tasks and leaves the others running; archiving closes them all), and what stays on disk
  (worktrees with uncommitted work, the task branches); then **Complete anyway** or **Archive anyway**. Completing an
  unfinished plan sends `ClosePlan` with `force`, which only the host's own window offers. **Complete plan** is the
  board's main action once every task is accepted.
- **The sidebar's BOARDS** list the same boards in the same order, each as simple mode shows a plan: its state's mark,
  its goal, and how it stands (waits for your approval, needs you, n running · done of total, done) with when it last
  moved. The board on screen stands first, by itself, with its sessions; the others take a line each under OTHERS, and
  a click brings one to the top. Under a board in full are the sessions that worked for it: live ones with their
  status dot and Ctrl+N number, closed ones quiet and marked closed, which a click resumes. Sessions of no board are
  under UNGROUPED; closed sessions of no board and the archived boards fold away under CLOSED SESSIONS and HISTORY,
  where a click shows an archived board and its **Reopen plan** brings it back. **New work** at the top goes back to the
  start screen. `cmd+alt+down` / `cmd+alt+up` step to the next and previous board in this order.
- **The start screen** shows while no board is on screen and nothing runs, and after New work until the window goes
  somewhere else: a box to tell the chief what to do, which starts it (or continues it) with the message; with a
  ticket system, a ticket to plan from, which sends the message as the Tickets panel's Plan with chief does; the
  recent boards to continue; and, quietly, an agent to start by hand. While it shows and the chief has nothing to do,
  the chief is not docked beside it: it docks again once it works, a board opens, or its pill or `cmd+o` asks for it.
- **A finished board** says Done (or Archived · in History) above its goal and what it delivered: its tasks done,
  where they were integrated and its pull request. Beside it are Archive, Clean up plan…, Reopen plan and Start new
  work, which goes back to the start screen. A done task's card resumes its closed agent, or says why it cannot.
- **One view switcher** (Board, Diff, Tickets) replaces Show diff, Plan and the Tickets item. The view on screen is
  filled; picking it again hides the panel. `cmd+b`, `cmd+d` and `cmd+k` do the same as its segments. Going to a plan
  from the location line or the sidebar keeps the view; each window remembers its choice (the desktop in its store, a browser in `localStorage`). The
  phone's pane bar shows Board and Diffs with the same icons and counts.
- **The location line** over the agents and the panel says where the window is: the plan › the task the active
  session works on › the session and its status (or the worktree whose diff shows). Each part is a link: the plan
  and the task to the plan's board, the session to its terminal and diff.
- **Back and forward** walk the places the window showed (tab, session or worktree, view): the ‹ › arrows at the start
  of the location line, `cmd+alt+left` / `cmd+alt+right` (`[keys] back`, `forward`), and **Escape** while the keyboard
  is in no terminal, text field, menu or dialog, so it never takes an Escape meant for an agent. Each window keeps
  its own history (client state, `atomics_ui::nav`).
- **Settings, Remote and the project picker** take the keyboard when they open, so nothing typed at them reaches a
  terminal, including after an agent chord selects another agent. **Escape** closes the one on top (a menu inside it
  first), and the keyboard returns to the active terminal.
  A click on a terminal behind one types there; Escape still closes the window.
- **A task card's click** shows its session with its diff, or a finished task's diff; its title still opens and
  closes the card. The card of the session on screen is outlined in the accent color.
- **The sidebar's AGENTS** group the sessions under the plan they work for (the heading shows the plan), then those
  outside plans. A session that needs the user has a warning bar and its state in the warning color, with the number
  of permission requests waiting; the active one keeps the accent fill. AGENTS and the EXPLORER open folded, and each
  window remembers which it opened (the desktop in its store). The providers running now show as meters at its foot.
- **Each agent's row shows its number**, under its status dot: Ctrl+N goes to that row, whatever order the plan groups
  put it in. The tile strip lists the agents in that order too.
- **An agent started through control tools or another client** leaves this window's active agent and keyboard alone.
- **Keyboard focus is visible** on the custom controls: the pill, the switcher's segments, the location's arrows and
  links, the sidebar's rows and the task cards draw a focus ring in the theme's selection color.

The phone keeps its panes: each pane's header says where it is (the board's plan, the tile's session), and the
browser's own Back stays the page's. A tablet has the frame, with the sidebar in a drawer from the top bar where it does
not fit (the browser's Back closes it while it is open), and the chief over the area under the top bar where it does
not sit beside the agents.

## The keys that move around, on every client

The desktop, a browser and `--connect` decide these in one place, `atomics_ui::window_keys`. Each only says which keys
never reach a terminal.

| Key | Does | Terminal |
| --- | --- | --- |
| Ctrl+1 to 9, Command on a Mac | The Nth agent of the tab, the number on its row, with its diff against main. The keyboard stays with the diff, and an open menu closes. Past the last agent, nothing. | Never gets it, nor any other Ctrl+digit |
| Command+Option+↓/↑ on a Mac, Ctrl+Shift+Alt+↓/↑ elsewhere | The next and previous board, in the order BOARDS and the Boards dropdown list them, wrapping round; from no board, the first or the last. | Never gets it |
| Command+Shift+L on a Mac, Ctrl+Alt+Shift+L elsewhere | Simple mode, and back to the full window, for this window alone. | Never gets it |
| Shift+↓/↑ | The next and previous changed file, from anywhere but a text field or the editor. | Never gets it |
| PageDown/PageUp | The next and previous hunk across files, while the diff has the keyboard. | Keeps them while it has the keyboard, for its programs' own scrolling |
| ↑/↓ | Scroll the diff by lines, while it has the keyboard. | Keeps them |
| Typing, Escape | From the diff: the keyboard goes back to the active agent's terminal, with the key typed. | |
| Command+G on macOS/iOS, Ctrl+G elsewhere (`[keys] github`) | Open or close the client's grouped pull request list. ↑/↓ selects a visible row, Enter loads its diff, Esc closes. Arrows skip folded rows. | Never gets the configured chord or keys while the list holds the keyboard |
| Ctrl with another letter | Nothing here. | The terminal's |

The pull request list is shared by desktop clients (including VM windows) and browsers. Its rows
show the title first, then muted repository, author and branches; View diff loads the diff and Open
opens an HTTP(S) link. The host's own GitHub dialog keeps its existing controls and keys.

Two exceptions are deliberate: with Tickets open the reader follows the agent and no diff comes up, and in editor mode
the agent's editor takes the keyboard. A phone with a keyboard takes the same keys for its Agents and Diffs panes and
keeps its panes otherwise; the key bar's sticky Ctrl still types a control byte, digits included, since a phone has no
other way to.

**What a browser keeps.** A page can take Ctrl+1 to 9 from the browser, so a browser needs no other keys. Checked in
Chrome 156 on Linux with real X11 keys: in a tab, where a page that leaves them alone switches tabs and this one does
not, in a frame of a hosting page, and in an app window (`--app`), the same kind of window as an installed app. Keys
Chrome keeps for itself, such as Ctrl+T and Ctrl+W, never reach a page, and a key pressed while the keyboard is on the
hosting page's own controls, not in the frame, is the browser's. On a Mac, an iPad or an iPhone the page asks for
Command; Safari and Firefox were not checked.
