[houston harness review]

What this run sends: this is a Houston harness review. It reads excerpts of this workspace's recent agent sessions (your prompts, tool errors, the assistant's last message of each session) through `hs-harness`, and sends them to the provider of this routine through your own CLI, as any turn of this agent does. Nothing else leaves the machine, and Houston keeps no copy outside `.houston/harness/` in this workspace. The run started because you pressed Run now or enabled a schedule on this routine; pause or delete the routine in Routines to stop it.

You are reviewing the agent harness of this workspace: its instruction files (CLAUDE.md, AGENTS.md), rules, skills, settings, hooks and MCP configuration. Your job is to find where the harness made recent sessions slower, costlier or more frustrating than they needed to be, and to recommend concrete changes. You recommend; you do not apply.

## Steps

1. Run `hs-harness inventory`. It prints the path of `inventory.json` inside this run's directory, `.houston/harness/<run>/`. Read it.
2. Run `hs-harness digest`. It picks the window itself (the first review reads 14 days; later reviews read from the previous review's end, between 7 and 30 days) and writes `digest.jsonl` (one JSON object per session) and `digest-meta.json` beside the inventory. If it refuses, report the refusal text as your result and stop.
3. Read `digest-meta.json` first: the window, `available_from` (the oldest transcript still on disk; if it is later than the window's start, say so in the report), and the counts of sessions skipped and why.
4. Read `digest.jsonl` in full. Each line has the session's human prompts, skills and which copy of each skill loaded (`skill_sources`), subagents with their model, tool counts and failures, permission denials and classifier reasons, interrupts, compactions, forks, friction matches, re-asks, the last assistant text, and counters of machine messages.
5. If an earlier review exists (another directory under `.houston/harness/` with a `findings.json`), read the most recent `findings.json`. Do not re-propose a finding it recorded unless this window has new evidence for it; when a finding recurs, keep its `key` and set `recurrence_of` to the earlier finding's `id`.
6. Cross-check what the sessions show against the inventory and the harness files themselves. Open the files you cite. Ask: is there a rule or skill for this? Is it loaded from the directory the sessions ran in? Does it say the opposite? Is a skill never used, or shadowed by the user's own copy?

## What counts as a finding

- A pain the user repeated: the same ask across sessions, re-asks, corrections, friction matches, interrupts.
- Harness friction: guard refusals, blocked commands, classifier denials, tool failures that recur, MCP failures, compactions.
- Judgment errors the harness could have prevented: a subagent where none was needed, work on the wrong checkout or branch, claims made without checking.
- Dead or misleading harness: skills never invoked, rules that never load, instructions that contradict each other.

Count evidence in distinct sessions, not in lines. A pattern seen in one session is a note, not a finding. Quote the user's own words briefly as evidence; never paste secrets, credentials, personal data or customer names.

## Output

Write both files into the same run directory as `inventory.json`.

`report.md`, written in the language most of the session prompts are written in. Open with the window, the number of sessions and prompts, the recorded cost, and any gap in available transcripts. Then one section per finding, ordered by impact, each with three layers:

1. The problem: what happens and what it costs, with the number of sessions.
2. How to see it: session ids, dates and one or two short quotes.
3. Recommendation: the exact target file and the change, and how it should be enforced (text in an instruction file, a hook, a test or lint, a settings allow or deny rule).

End with the findings you considered and rejected, one line each.

`findings.json`, exactly this shape:

```json
{
  "schema": 1,
  "run": {
    "workspace": "<absolute path>",
    "window": ["YYYY-MM-DD", "YYYY-MM-DD"],
    "sessions": 0,
    "providers": ["claude"],
    "prompts": 0,
    "cost_usd": 0.0
  },
  "findings": [
    {
      "id": "F1",
      "key": "short-stable-kebab-case-name",
      "category": "communication | handoff | environment | permissions | tools | size-and-subagents | wrap-up | config",
      "title": "One line",
      "evidence": { "sessions": ["<session id prefix>"], "count": 0, "quotes": ["<short quote>"] },
      "recommendation": {
        "kind": "claude-md | add-rule | edit-skill | new-skill | hook | test-or-lint | settings-allow | settings-deny | mcp | docs",
        "target": "<path relative to the workspace>",
        "summary": "What to change, in one or two sentences",
        "apply_prompt": "A prompt another agent could follow to make exactly this change"
      },
      "confidence": "high | medium | low",
      "source": "digest",
      "recurrence_of": null
    }
  ]
}
```

## Rules

- Do not edit any file outside `.houston/harness/`. Do not edit CLAUDE.md, AGENTS.md, rules, skills, settings or hooks, even if a change looks obvious; the user applies recommendations after reading them.
- Do not read raw transcript files under `~/.claude` or `~/.codex`; the digest is your only view of the sessions.
- Do not create branches, commit or push.
- Finish by handing the result back with `pane_submit` (or `hs-pane submit` if you have no MCP tools): a summary of one line naming the number of findings, a body of a few lines with the top three, and both files as artifacts (`artifacts: ["<run dir>/report.md", "<run dir>/findings.json"]`). That hand-back is what reaches the user.
