# Harness review

A harness review is a routine that reads a workspace's recent agent sessions, compares them
with the workspace's agent harness (CLAUDE.md, AGENTS.md, `.claude/rules`, skills, settings,
hooks and MCP configuration), and hands back a report of what slowed those sessions down and
what to change. It recommends; it never edits the harness itself.

## What a run sends

A run is an ordinary agent pane. Its agent calls `hs-harness`, which writes two local files
under `<workspace>/.houston/harness/<run>/`:

- `inventory.json`: the harness files, their sizes and headings, rule paths, skills (including
  your user-level copies in `~/.claude/skills`), permission lists, hook counts and MCP servers.
- `digest.jsonl`: one line per Claude Code or Codex session whose working directory is this
  workspace or below it, within the run's window: your prompts, the skills and subagents used,
  tool failures, permission denials, interrupts, and the last assistant message.

The agent then reads those files, which sends their content to the routine's provider through
your own CLI, as any turn of that agent does. Nothing else leaves the machine, Houston keeps
nothing in its database, and OpenCode, Cursor, Grok and Antigravity sessions are not read.

## When it runs

Only when you press **Run now**, or after you enable the routine's schedule; it is created
paused because every run spends tokens. The first run reads the last 14 days; each later run
reads from where the previous one stopped, never less than 7 days nor more than 30. Sessions
older than your CLI keeps (`cleanupPeriodDays` in `~/.claude/settings.json` for Claude Code)
cannot be read, and the report says so.

## Creating one

Create a routine in **Routines** with this workspace as its working directory, **Accept
edits**, no isolation, and the instructions printed by `hs-harness preset` from any pane in the
workspace. Leave it paused.

## Reading the result

Enable orchestration in **Settings ▸ Orchestration**; the run hands back through it. The
result arrives in the workspace's inbox under the bell, with two files: `report.md`, one
section per finding (the problem, how to see it, the recommended change and its target file),
and `findings.json`, the same findings in a fixed shape that a later review reads so it does
not repeat what you already dismissed.

## Turning it off

Pause the routine to stop scheduled runs, or delete it. Delete
`<workspace>/.houston/harness/` to remove every past report and digest; the directory is
ignored by Git.
