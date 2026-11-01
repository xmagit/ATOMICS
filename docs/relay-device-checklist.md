# Relay remote control: device checklist

Steps for Max. Run the iPhone (installed PWA) first, then Android Chrome. Setup is in
[Relay: remote control](tickets.md#relay-remote-control). Record each result as **Pass**, **Fail** or
**Skip**, with a short note (what you saw, a screenshot name, the message text). The chief collects the
results; reply to it with the table for each device.

## Before you start

- ATOMICS runs on the desk machine with the workspace `dev.toml` (`[tickets.remote] enabled = true`), signed
  in to Relay. The sign-in chip shows `relay · <agent> (agent) · steering: <you>`.
- Use a scratch ticket you may spam (RELAY-11985 worked on 2026-11-01). Link it to the ATOMICS ticket
  panel, then **Plan with chief** so there is a plan on it.
- On the phone, sign in to Relay as yourself (the steering human, not the agent). Notifications for the
  Relay PWA are allowed in the OS settings (iPhone: the PWA must be added to the Home Screen first).
- Note the time you start each step; it helps match events in the conversation.

## Checklist (once per device)

| # | Step | Expected | iPhone | Android |
|---|------|----------|--------|---------|
| 1 | Open the ticket's Conversation in the PWA. | Mirror shows the chief's prompts and answers, tool blocks as short cards, text in order, no workers/terminals/diffs. | | |
| 2 | Scroll back and reload the page. | Nothing duplicated; order unchanged; text over 2000 characters ends with "(… N more characters in ATOMICS)". | | |
| 3 | Steering: send "Hello, reply with one word". | In ATOMICS the chief receives it labelled "From <you>, a verified steering human, via the relay conversation …"; a status "delivered to the chief" appears; the chief's answer is mirrored back. | | |
| 4 | Send a second message while the chief is busy. | It is queued/delivered once, never twice. Quit and restart ATOMICS mid-delivery if you can: a "may not have reached the chief" note, no repaste. | | |
| 5 | Plan approval: ask the chief to plan so a plan is unapproved. | Plan events (one per task, `t1 · Title` …) then an Approval event ending `/approve <12 hex>`. | | |
| 6 | Send the `/approve <digest>` line (or tap Approve if the PWA shows one). | "Approved by <you> from the conversation; the chief is dispatching"; the board shows the plan approved; the chief dispatches. | | |
| 7 | Send `/approve 000000000000` on the newest plan, with a stale digest. | "This plan changed; the new digest is …" and a fresh request; nothing approved; chief sees nothing. | | |
| 8 | Notification: make the plan wait for approval (step 5) with the app in the background. | One notification for WaitingForApproval, not repeated for the same digest. | | |
| 9 | Notification: make a task fail (e.g. a task that exits non-zero). | One notification for Failed; the conversation shows the blocked status. | | |
| 10 | Let the plan complete (approve, let tasks finish, accept). | Completed activity; the conversation ends; the PWA shows it ended; ATOMICS stops polling and shows no error. | | |
| 11 | Takeover: with plan A open on the ticket, create plan B on the same ticket. | Toast says B owns the remote and A is older; the conversation continues (no new thread). | | |
| 12 | In the conversation, send a message after B took over. | It reaches B's chief once; A's chief gets nothing; messages A already handled are not delivered again. | | |
| 13 | Close plan B (finish or archive). | Conversation does not end if A is still open; A takes over and keeps working. | | |

## Results

Filled by the chief with Max's answers.

| Device | Steps | Result | Notes |
|--------|-------|--------|-------|
| iPhone (installed PWA) | 1–13 | _pending_ | |
| Android Chrome | 1–13 | _pending_ | |
