# Notebook A/B experiment

Run the same read-only three-agent plan with `[plan] notebook` off and on. A seed agent discovers how to invoke a
tiny verifier; two fresh peers then verify it. In the on arm the runner acts as the user, checks the seed's proposal
against the fixture, and pins it through the board's control path before peers start. Their briefs contain only the
pinned count. In the off arm the seed's report stays with the chief. No fact is forwarded in a message or brief.
The normal final review is skipped by ATOMICS because these tasks leave no changes.

Prerequisites: Node, git, the newly built ATOMICS binary, and an authenticated Claude-compatible CLI that supports
`--print --verbose --output-format stream-json`. Pin a model in the provider command for meaningful comparisons.
All host state, repositories, worktrees, proposals, provider transcripts and evidence logs live under the output
directory. Existing ATOMICS state and git identity settings are untouched. The runner approves this fixture plan;
it never answers an agent's permission request. The fixture commands have a narrow allow list; unexpected requests
are logged and may cause the run to time out. Such runs are incomplete, not successful samples.

```sh
atomics queue --class test -- nix develop .#default --command cargo build -p atomics
atomics queue --class qa -- node scripts/notebook-ab.mjs \
  --atomics target/debug/atomics --pairs 3 --output /tmp/notebook-ab \
  --command-json '["claude","--print","--verbose","--output-format","stream-json","--model","MODEL"]'
```

Use a fresh output directory. Pairs alternate off/on and on/off to reduce ordering effects. Keep the model, provider
version, permission policy and machine load fixed; do not reuse conversations. Model sampling and provider caches
still vary. This is a small controlled discovery task, not evidence about general coding productivity or an
autonomous chief's curation cost. User pin actions are counted separately so that curation is visible.

The script prints the artifact directory and one JSON result per arm. `manifest.json` records the binary version,
provider command and run settings; `results.json` and each arm's result report:

| Field | Meaning |
| --- | --- |
| Per-agent input/output tokens | Actual provider `result.usage`; cache reads and cache creation are separate fields. Missing usage is `null`, never estimated. |
| Per-agent wall seconds | Provider wrapper start to exit; plan wall time also includes host startup, dispatch, verification and curation, excluding fixture creation. |
| Approvals asked | Unique ATOMICS approval IDs, per agent and per arm. Plan approvals and pin actions are separate counts; provider permission denials are also reported. |
| Rediscoveries after seed | Calls to `./discover` after seed completion (and pinning in the on arm), attributed by host session. |
| Rediscoveries of pinned fact | The same counter in the on arm; `null` in the off arm, where nothing was pinned. |
| Wrong invocations | Calls to `./check` without `--portable`. |

Discovery is deliberately observable through a fixture helper. Reading the helper's source to recover the fact
does not increment the counter; inspect raw transcripts for that behavior. Missing token fields or failed tasks
invalidate a token comparison. Compare peer totals as well as all-agent totals; report each pair before aggregating.
The seed's note must include the verified invocation or the on arm fails instead of silently measuring no pin.

For a repeatable smoke check without model calls:

```sh
atomics queue --class qa -- node scripts/notebook-ab.mjs \
  --atomics target/debug/atomics --smoke --pairs 2 --output /tmp/notebook-ab-smoke
```

Synthetic agents still use the real host, task dispatch, session scoping, proposal/pin/read paths and persistence.
The smoke check asserts one rediscovery per peer with the notebook off and none with it on. It marks every result
`synthetic: true` and leaves token fields `null`; these results establish harness behavior, not token savings.
No live benchmark numbers accompany this change.
