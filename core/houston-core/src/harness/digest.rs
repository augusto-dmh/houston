//! One line per session of a workspace, extracted without judgment, so the
//! agent of a review run reads digests instead of raw transcripts. The terms
//! any edit must keep are carve-out #6 in `docs/internals/invariants.md`.
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{bail, Context, Result};
use regex::Regex;
use serde_json::{json, Map, Value};

use super::window::{format_ms, Window};
use crate::usage::time::parse_rfc3339_ms;

/// A digest line past this is almost all prompt text; 64 KiB keeps the first
/// and last prompts of even a day-long session.
pub const SESSION_LINE_MAX: usize = 64 * 1024;
/// Roughly half a model context of digest; past it, one reading cannot hold
/// the window and a narrower one is the fix.
pub const DIGEST_TOTAL_MAX: usize = 2 * 1024 * 1024;
/// Same bound as the usage scan: a longer line is tool output or an image.
const LINE_MAX: usize = 8 * 1024 * 1024;
const PROMPT_MAX_CHARS: usize = 2_000;
const LAST_TEXT_MAX_CHARS: usize = 2_000;
const REASON_MAX_CHARS: usize = 200;
const ERROR_SAMPLE_CHARS: usize = 200;
const ERROR_SAMPLES_PER_TOOL: usize = 3;
/// How far into a file the cwd and first timestamp are looked for.
const PEEK_LINES: usize = 40;

/// The first line of the Houston review prompt, so a run never digests itself.
pub const REVIEW_MARKER: &str = "[houston harness review]";

/// Wrapper prefixes that are never a person typing; `session-index.py`'s list
/// plus the two wrappers Claude Code added since.
const SKIP_PREFIXES: [&str; 12] = [
    "<command-name>",
    "<local-command",
    "<system-reminder>",
    "Base directory for this skill",
    "<task-notification>",
    "Another Claude session sent",
    "The fork runs as its own",
    "[Request interrupted",
    "<bash-input>",
    "<bash-stdout>",
    "<local-command-caveat>",
    "<command-message>",
];

/// Machine-emitted strings worth counting, from the manual rounds' greps.
const MACHINE_STRINGS: [(&str, &str); 6] = [
    ("sleep_blocked", "Blocked: sleep"),
    ("worktree_guard", "This session is isolated in the worktree"),
    ("low_memory", "running low on memory"),
    (
        "classifier_denied",
        "denied by the Claude Code auto mode classifier",
    ),
    ("interrupted_by_user", "Request interrupted by user"),
    ("usage_limit_reset", "Your claude.ai usage limit has reset"),
];

const CLASSIFIER_PREFIX: &str =
    "Permission for this action was denied by the Claude Code auto mode classifier. Reason: ";

/// The fixed pushback vocabulary of `session-index.py`, kept verbatim so two
/// runs, and a run and a manual round, count the same thing.
fn friction_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)wtf|didn'?t get|i don'?t get|não entendi|nao entendi|why are you|why a subagent|stop that|i don'?t want|too much|not what i asked|you are not|errado|não é isso|request interrupted",
        )
        .expect("the friction vocabulary is a valid regex")
    })
}

fn command_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"<command-name>(/[\w-]+)</command-name>")
            .expect("the command-name pattern is a valid regex")
    })
}

/// `None` when the text is not a person's prompt; slash commands come back as
/// `CMD /<name>`, as `session-index.py` records them.
pub fn human_prompt(text: &str) -> Option<String> {
    if text.trim().is_empty() {
        return None;
    }
    let head: String = text.chars().take(100).collect();
    if head.contains("Caveat: The messages below") {
        return None;
    }
    if let Some(m) = command_re().captures(text) {
        return Some(format!("CMD {}", &m[1]));
    }
    let trimmed = text.trim_start();
    if SKIP_PREFIXES.iter().any(|p| trimmed.starts_with(p)) {
        return None;
    }
    Some(text.to_string())
}

fn cap_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

/// Claude's project-directory name for a path: every non-alphanumeric byte
/// becomes `-`.
pub fn claude_slug(path: &Path) -> String {
    path.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

fn within(ws: &Path, cwd: &str) -> bool {
    Path::new(cwd).starts_with(ws)
}

#[derive(Debug, Clone)]
struct Prompt {
    at: String,
    text: String,
    uuid: Option<String>,
    queued: bool,
}

/// A session reduced to what the digest writes.
#[derive(Debug, Default)]
struct Session {
    provider: &'static str,
    id: String,
    path: PathBuf,
    title: Option<String>,
    branch: Option<String>,
    cwd: Option<String>,
    first_ms: Option<i64>,
    last_ms: Option<i64>,
    in_window: bool,
    cost_usd: Option<f64>,
    models: BTreeMap<String, u64>,
    prompts: Vec<Prompt>,
    skills: BTreeMap<String, u64>,
    skill_sources: BTreeMap<String, u64>,
    subagents: Vec<Value>,
    tools: BTreeMap<String, u64>,
    tool_errors: BTreeMap<String, u64>,
    tool_error_samples: BTreeMap<String, Vec<String>>,
    denials: BTreeMap<String, u64>,
    classifier_reasons: BTreeMap<String, u64>,
    compactions: u64,
    interrupts: u64,
    fork_marker_ms: Option<i64>,
    forked: bool,
    fork_of: Option<String>,
    first_uuid: Option<String>,
    uuids: Vec<(String, i64)>,
    last_assistant_text: Option<String>,
    machine: BTreeMap<&'static str, u64>,
    tool_names: HashMap<String, String>,
}

impl Session {
    fn stamp(&mut self, ts: Option<&str>, window: &Window) -> Option<i64> {
        let ms = parse_rfc3339_ms(ts?)?;
        self.first_ms = Some(self.first_ms.map_or(ms, |f| f.min(ms)));
        self.last_ms = Some(self.last_ms.map_or(ms, |l| l.max(ms)));
        if ms >= window.since_ms && ms < window.until_ms {
            self.in_window = true;
        }
        Some(ms)
    }

    fn count_machine(&mut self, text: &str) {
        for (key, needle) in MACHINE_STRINGS {
            if text.contains(needle) {
                *self.machine.entry(key).or_default() += 1;
            }
        }
    }

    fn tool_error(&mut self, name: &str, text: &str) {
        *self.tool_errors.entry(name.to_string()).or_default() += 1;
        let samples = self.tool_error_samples.entry(name.to_string()).or_default();
        if samples.len() < ERROR_SAMPLES_PER_TOOL {
            samples.push(cap_chars(text.trim(), ERROR_SAMPLE_CHARS));
        }
    }
}

fn text_of(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn str_at<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

fn feed_claude(s: &mut Session, o: &Value, window: &Window) {
    if o.get("isSidechain").and_then(Value::as_bool) == Some(true) {
        return;
    }
    let ts = str_at(o, "timestamp");
    let ms = s.stamp(ts, window);
    let kind = str_at(o, "type").unwrap_or("");
    if s.cwd.is_none() {
        s.cwd = str_at(o, "cwd").map(str::to_string);
    }
    if let Some(b) = str_at(o, "gitBranch") {
        s.branch = Some(b.to_string());
    }
    let uuid = str_at(o, "uuid").map(str::to_string);
    if matches!(kind, "user" | "assistant") {
        if let Some(u) = &uuid {
            if s.first_uuid.is_none() {
                s.first_uuid = Some(u.clone());
            }
            s.uuids.push((u.clone(), ms.unwrap_or(0)));
        }
    }
    match kind {
        "ai-title" => s.title = str_at(o, "aiTitle").map(str::to_string),
        "cost-state" => {
            if let Some(c) = o.get("totalCostUSD").and_then(Value::as_f64) {
                s.cost_usd = Some(c);
            }
        }
        "system" => {
            if str_at(o, "subtype") == Some("compact_boundary") {
                s.compactions += 1;
            }
            if let Some(c) = str_at(o, "content") {
                s.count_machine(c);
            }
        }
        "attachment" => {
            let a = o.get("attachment").unwrap_or(&Value::Null);
            match str_at(a, "type") {
                Some("queued_command")
                    if a.pointer("/origin/kind").and_then(Value::as_str) == Some("human") =>
                {
                    if let Some(p) = str_at(a, "prompt") {
                        s.prompts.push(Prompt {
                            at: ts.unwrap_or("").to_string(),
                            text: p.to_string(),
                            uuid: uuid.clone(),
                            queued: true,
                        });
                    }
                }
                Some("fork_briefing") => {
                    s.fork_marker_ms.get_or_insert(ms.unwrap_or(0));
                }
                _ => {}
            }
        }
        "user" => feed_claude_user(s, o, ts, uuid, ms),
        "assistant" => feed_claude_assistant(s, o),
        _ => {}
    }
}

fn feed_claude_user(
    s: &mut Session,
    o: &Value,
    ts: Option<&str>,
    uuid: Option<String>,
    ms: Option<i64>,
) {
    if o.get("isCompactSummary").and_then(Value::as_bool) == Some(true) {
        s.compactions += 1;
        return;
    }
    if let Some(kind) = str_at(o, "toolDenialKind") {
        *s.denials.entry(kind.to_string()).or_default() += 1;
    }
    let content = o.pointer("/message/content").unwrap_or(&Value::Null);
    let mut texts: Vec<String> = Vec::new();
    match content {
        Value::String(t) => texts.push(t.clone()),
        Value::Array(blocks) => {
            for b in blocks {
                match str_at(b, "type") {
                    Some("text") => texts.push(str_at(b, "text").unwrap_or("").to_string()),
                    Some("tool_result") => {
                        let body = text_of(b.get("content").unwrap_or(&Value::Null));
                        s.count_machine(&body);
                        if let Some(reason) = body.strip_prefix(CLASSIFIER_PREFIX) {
                            let reason = reason.split(" If you have other tasks").next();
                            let reason = cap_chars(reason.unwrap_or("").trim(), REASON_MAX_CHARS);
                            *s.classifier_reasons.entry(reason).or_default() += 1;
                        }
                        if b.get("is_error").and_then(Value::as_bool) == Some(true) {
                            let name = str_at(b, "tool_use_id")
                                .and_then(|id| s.tool_names.get(id).cloned())
                                .unwrap_or_else(|| "unknown".to_string());
                            s.tool_error(&name, &body);
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
    for t in texts {
        s.count_machine(&t);
        let trimmed = t.trim_start();
        if trimmed.starts_with("[Request interrupted") {
            s.interrupts += 1;
        }
        if trimmed.starts_with("The fork runs as its own") {
            s.fork_marker_ms.get_or_insert(ms.unwrap_or(0));
        }
        if trimmed.starts_with("This session is being continued") {
            s.compactions += 1;
        }
        for line in t.lines() {
            if let Some(path) = line.strip_prefix("Base directory for this skill:") {
                *s.skill_sources.entry(path.trim().to_string()).or_default() += 1;
            }
        }
        if let Some(p) = human_prompt(&t) {
            s.prompts.push(Prompt {
                at: ts.unwrap_or("").to_string(),
                text: p,
                uuid: uuid.clone(),
                queued: false,
            });
        }
    }
}

fn feed_claude_assistant(s: &mut Session, o: &Value) {
    let Some(m) = o.get("message") else { return };
    if let Some(model) = str_at(m, "model").filter(|m| !m.starts_with('<')) {
        *s.models.entry(model.to_string()).or_default() += 1;
    }
    let Some(blocks) = m.get("content").and_then(Value::as_array) else {
        return;
    };
    for b in blocks {
        match str_at(b, "type") {
            Some("text") => {
                if let Some(t) = str_at(b, "text").filter(|t| !t.trim().is_empty()) {
                    s.last_assistant_text = Some(t.to_string());
                }
            }
            Some("tool_use") => {
                let name = str_at(b, "name").unwrap_or("unknown").to_string();
                *s.tools.entry(name.clone()).or_default() += 1;
                if let Some(id) = str_at(b, "id") {
                    s.tool_names.insert(id.to_string(), name.clone());
                }
                let input = b.get("input").unwrap_or(&Value::Null);
                if name == "Skill" {
                    if let Some(skill) = str_at(input, "skill") {
                        *s.skills.entry(skill.to_string()).or_default() += 1;
                    }
                }
                if name == "Agent" || name == "Task" {
                    s.subagents.push(json!({
                        "type": str_at(input, "subagent_type"),
                        "model": str_at(input, "model"),
                        "description": str_at(input, "description").map(|d| cap_chars(d, 120)),
                        "background": input.get("run_in_background").and_then(Value::as_bool).unwrap_or(false),
                    }));
                }
            }
            _ => {}
        }
    }
}

fn feed_codex(s: &mut Session, o: &Value, window: &Window, saw_meta: &mut bool) {
    let ts = str_at(o, "timestamp");
    s.stamp(ts, window);
    let payload = o.get("payload").unwrap_or(&Value::Null);
    let ptype = str_at(payload, "type");
    match (str_at(o, "type"), ptype) {
        (Some("session_meta"), _) => {
            if *saw_meta {
                return;
            }
            *saw_meta = true;
            if let Some(id) = str_at(payload, "id") {
                s.id = id.to_string();
            }
            s.cwd = str_at(payload, "cwd").map(str::to_string);
            s.branch = payload
                .pointer("/git/branch")
                .and_then(Value::as_str)
                .map(str::to_string);
            if let Some(map) = payload.as_object() {
                s.forked = crate::usage::transcripts::is_forked_session_meta(map);
            }
        }
        (Some("turn_context"), _) => {
            if let Some(model) = str_at(payload, "model") {
                *s.models.entry(model.to_string()).or_default() += 1;
            }
        }
        (Some("event_msg"), Some("user_message")) => {
            if let Some(p) = str_at(payload, "message").and_then(human_prompt) {
                s.prompts.push(Prompt {
                    at: ts.unwrap_or("").to_string(),
                    text: p,
                    uuid: None,
                    queued: false,
                });
            }
        }
        (Some("event_msg"), Some("thread_name_updated")) => {
            s.title = str_at(payload, "thread_name").map(str::to_string);
        }
        (Some("event_msg"), Some("agent_message")) => {
            s.last_assistant_text = str_at(payload, "message").map(str::to_string);
        }
        (Some("event_msg"), Some("turn_aborted")) => s.interrupts += 1,
        (Some("event_msg"), Some("exec_command_end")) => {
            let code = payload
                .get("exit_code")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            if code != 0 {
                let name = codex_tool_name(s, payload, "exec_command");
                let out = str_at(payload, "stderr").unwrap_or("").to_string();
                s.tool_error(&name, &format!("exit {code} {out}"));
            }
        }
        (Some("event_msg"), Some("patch_apply_end")) => {
            if payload.get("success").and_then(Value::as_bool) == Some(false) {
                let name = codex_tool_name(s, payload, "apply_patch");
                let out = str_at(payload, "stderr").unwrap_or("").to_string();
                s.tool_error(&name, &out);
            }
        }
        (Some("response_item"), Some("function_call" | "custom_tool_call")) => {
            let name = str_at(payload, "name").unwrap_or("unknown").to_string();
            *s.tools.entry(name.clone()).or_default() += 1;
            if let Some(id) = str_at(payload, "call_id") {
                s.tool_names.insert(id.to_string(), name);
            }
        }
        _ => {}
    }
}

fn codex_tool_name(s: &Session, payload: &Value, fallback: &str) -> String {
    str_at(payload, "call_id")
        .and_then(|id| s.tool_names.get(id).cloned())
        .unwrap_or_else(|| fallback.to_string())
}

#[derive(Debug, Default)]
pub struct Tally {
    pub files_listed: u64,
    pub files_read: u64,
    pub unreadable_files: u64,
    pub oversized_lines: u64,
    pub out_of_window: u64,
    pub self_runs_skipped: u64,
    pub forks: u64,
    pub available_from_ms: Option<i64>,
}

fn for_each_line(path: &Path, tally: &mut Tally, mut f: impl FnMut(&Value) -> bool) -> bool {
    let Ok(file) = std::fs::File::open(path) else {
        tally.unreadable_files += 1;
        return false;
    };
    let mut reader = BufReader::with_capacity(64 * 1024, file);
    let mut buf = Vec::with_capacity(8 * 1024);
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) => return true,
            Ok(_) => {}
            Err(_) => {
                tally.unreadable_files += 1;
                return false;
            }
        }
        if buf.len() > LINE_MAX {
            tally.oversized_lines += 1;
            continue;
        }
        let Ok(v) = serde_json::from_slice::<Value>(&buf) else {
            continue;
        };
        if !f(&v) {
            return true;
        }
    }
}

/// The first cwd and timestamp in a file, read from its head only.
fn peek(path: &Path, tally: &mut Tally, codex: bool) -> (Option<String>, Option<i64>) {
    let (mut cwd, mut first) = (None, None);
    let mut n = 0;
    for_each_line(path, tally, |o| {
        n += 1;
        if first.is_none() {
            first = str_at(o, "timestamp").and_then(parse_rfc3339_ms);
        }
        if cwd.is_none() {
            cwd = if codex {
                o.pointer("/payload/cwd").and_then(Value::as_str)
            } else {
                str_at(o, "cwd")
            }
            .map(str::to_string);
        }
        n < PEEK_LINES && (cwd.is_none() || first.is_none())
    });
    (cwd, first)
}

pub struct Request<'a> {
    pub workspace: &'a Path,
    pub claude_roots: Vec<PathBuf>,
    pub codex_roots: Vec<PathBuf>,
    pub window: Window,
    pub session_line_max: usize,
    pub total_max: usize,
}

fn note_available(tally: &mut Tally, first: Option<i64>) {
    if let Some(ms) = first {
        tally.available_from_ms = Some(tally.available_from_ms.map_or(ms, |a| a.min(ms)));
    }
}

fn claude_files(req: &Request, tally: &mut Tally) -> Vec<PathBuf> {
    let slug = claude_slug(req.workspace);
    let mut out = Vec::new();
    for root in &req.claude_roots {
        let Ok(entries) = std::fs::read_dir(root) else {
            continue;
        };
        let mut dirs: Vec<PathBuf> = entries
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with(&slug))
            .map(|e| e.path())
            .collect();
        dirs.sort();
        for dir in dirs {
            let Ok(files) = std::fs::read_dir(&dir) else {
                continue;
            };
            let mut files: Vec<PathBuf> = files
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.is_file() && p.extension().and_then(|e| e.to_str()) == Some("jsonl"))
                .collect();
            files.sort();
            for f in files {
                tally.files_listed += 1;
                let (cwd, first) = peek(&f, tally, false);
                if !cwd.is_some_and(|c| within(req.workspace, &c)) {
                    continue;
                }
                note_available(tally, first);
                out.push(f);
            }
        }
    }
    out
}

fn codex_files(req: &Request, tally: &mut Tally) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for root in &req.codex_roots {
        for file in crate::usage::scan::list_transcripts(root, 0).files {
            tally.files_listed += 1;
            let (cwd, first) = peek(&file.path, tally, true);
            if !cwd.is_some_and(|c| within(req.workspace, &c)) {
                continue;
            }
            note_available(tally, first);
            if file.mtime_ms >= req.window.since_ms {
                out.push(file.path);
            }
        }
    }
    out
}

fn mtime_ms(p: &Path) -> i64 {
    std::fs::metadata(p)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_millis() as i64)
}

/// Files that share the uuid of their first message are one conversation
/// copied by a fork or a branch. The original is the member with no fork
/// notice of its own; otherwise the one whose own lines start first.
fn resolve_forks(sessions: &mut [Session], tally: &mut Tally) {
    let mut families: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, s) in sessions.iter().enumerate() {
        if let Some(u) = &s.first_uuid {
            families.entry(u.clone()).or_default().push(i);
        }
    }
    for members in families.values().filter(|m| m.len() > 1) {
        let mut shared: HashMap<&str, usize> = HashMap::new();
        for &i in members {
            for (u, _) in &sessions[i].uuids {
                *shared.entry(u.as_str()).or_default() += 1;
            }
        }
        let own_start = |i: usize| {
            sessions[i]
                .uuids
                .iter()
                .find(|(u, _)| shared.get(u.as_str()) == Some(&1))
                .map_or(i64::MAX, |(_, ms)| *ms)
        };
        let mut order: Vec<(bool, i64, usize)> = members
            .iter()
            .map(|&i| (sessions[i].fork_marker_ms.is_some(), own_start(i), i))
            .collect();
        order.sort();
        let original = order[0].2;
        let original_id = sessions[original].id.clone();
        let mut seen: HashSet<String> = HashSet::new();
        for &(_, _, i) in &order {
            let s = &mut sessions[i];
            if i != original {
                s.forked = true;
                s.fork_of = Some(original_id.clone());
                tally.forks += 1;
            }
            s.prompts.retain(|p| match &p.uuid {
                Some(u) => !seen.contains(u),
                None => true,
            });
            seen.extend(s.uuids.iter().map(|(u, _)| u.clone()));
        }
    }
}

fn friction_and_reasks(prompts: &[Prompt]) -> (Vec<Value>, u64) {
    let mut friction = Vec::new();
    let mut reasks = 0;
    let mut prev: Option<String> = None;
    for p in prompts {
        if !p.text.starts_with("CMD /") {
            if let Some(m) = friction_re().find(&p.text) {
                friction.push(json!({ "at": p.at, "match": m.as_str() }));
            }
        }
        let key: String = p.text.chars().take(40).collect();
        if prev.as_deref() == Some(key.as_str()) {
            reasks += 1;
        }
        prev = Some(key);
    }
    (friction, reasks)
}

fn session_value(s: &Session, prompts: &[Prompt], dropped: usize) -> Value {
    let (friction, reasks) = friction_and_reasks(&s.prompts);
    let minutes = match (s.first_ms, s.last_ms) {
        (Some(a), Some(b)) => (b - a + 30_000) / 60_000,
        _ => 0,
    };
    let mut machine: Map<String, Value> = s
        .machine
        .iter()
        .map(|(k, v)| (k.to_string(), json!(v)))
        .collect();
    let chrome: u64 = s
        .tool_errors
        .iter()
        .filter(|(k, _)| k.starts_with("mcp__claude-in-chrome__"))
        .map(|(_, v)| v)
        .sum();
    if chrome > 0 {
        machine.insert("chrome_mcp_errors".into(), json!(chrome));
    }
    json!({
        "v": 1,
        "provider": s.provider,
        "id": s.id,
        "path": s.path.to_string_lossy(),
        "title": s.title,
        "branch": s.branch,
        "cwd": s.cwd,
        "start": s.first_ms.map(format_ms),
        "end": s.last_ms.map(format_ms),
        "minutes": minutes,
        "cost_usd": s.cost_usd,
        "models": s.models,
        "prompt_count": s.prompts.len(),
        "prompts": prompts.iter().map(|p| {
            let mut v = json!({ "at": p.at, "text": cap_chars(&p.text, PROMPT_MAX_CHARS) });
            if p.queued {
                v["queued"] = json!(true);
            }
            v
        }).collect::<Vec<_>>(),
        "prompts_dropped": dropped,
        "skills": s.skills,
        "skill_sources": s.skill_sources,
        "subagents": s.subagents,
        "tools": s.tools,
        "tool_errors": s.tool_errors,
        "tool_error_samples": s.tool_error_samples,
        "denials": s.denials,
        "classifier_reasons": s.classifier_reasons,
        "compactions": s.compactions,
        "interrupts": s.interrupts,
        "forked": s.forked,
        "fork_of": s.fork_of,
        "friction": friction,
        "reasks": reasks,
        "last_assistant_text": s.last_assistant_text.as_deref().map(|t| cap_chars(t, LAST_TEXT_MAX_CHARS)),
        "machine": machine,
    })
}

/// Drops prompts from the middle until the line fits, keeping the opening ask
/// and the latest ones, where corrections of course land.
fn session_line(
    s_value: impl Fn(&[Prompt], usize) -> Value,
    prompts: &[Prompt],
    max: usize,
) -> String {
    let mut kept: Vec<Prompt> = prompts.to_vec();
    let mut dropped = 0;
    loop {
        let line = s_value(&kept, dropped).to_string();
        if line.len() <= max || kept.len() <= 2 {
            return line;
        }
        let excess = line.len() - max;
        let mut freed = 0;
        while freed < excess && kept.len() > 2 {
            let mid = kept.len() / 2;
            freed += kept.remove(mid).text.len().min(PROMPT_MAX_CHARS * 4) + 16;
            dropped += 1;
        }
    }
}

pub struct Outcome {
    pub lines: Vec<String>,
    pub tally: Tally,
}

pub fn run(req: &Request) -> Result<Outcome> {
    let mut tally = Tally::default();
    let mut sessions: Vec<Session> = Vec::new();
    for path in claude_files(req, &mut tally) {
        if mtime_ms(&path) < req.window.since_ms {
            continue;
        }
        let mut s = Session {
            provider: "claude",
            id: path
                .file_stem()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            path: path.clone(),
            ..Default::default()
        };
        tally.files_read += 1;
        for_each_line(&path, &mut tally, |o| {
            feed_claude(&mut s, o, &req.window);
            true
        });
        sessions.push(s);
    }
    resolve_forks(&mut sessions, &mut tally);
    for path in codex_files(req, &mut tally) {
        let mut s = Session {
            provider: "codex",
            path: path.clone(),
            ..Default::default()
        };
        let mut saw_meta = false;
        tally.files_read += 1;
        for_each_line(&path, &mut tally, |o| {
            feed_codex(&mut s, o, &req.window, &mut saw_meta);
            true
        });
        if s.forked {
            tally.forks += 1;
        }
        sessions.push(s);
    }
    sessions.sort_by_key(|s| s.first_ms);

    let mut lines = Vec::new();
    let mut total = 0usize;
    for s in &sessions {
        if !s.in_window {
            tally.out_of_window += 1;
            continue;
        }
        if s.prompts
            .first()
            .is_some_and(|p| p.text.trim_start().starts_with(REVIEW_MARKER))
        {
            tally.self_runs_skipped += 1;
            continue;
        }
        let line = session_line(
            |p, d| session_value(s, p, d),
            &s.prompts,
            req.session_line_max,
        );
        total += line.len() + 1;
        lines.push(line);
    }
    if total > req.total_max {
        bail!(
            "the digest is {total} bytes for {} sessions, over the {} byte cap; narrow the \
             window with --since/--until",
            lines.len(),
            req.total_max
        );
    }
    Ok(Outcome { lines, tally })
}

/// Runs the digest and writes it only when the whole of it is within caps.
pub fn run_and_write(req: &Request, path: &Path) -> Result<Outcome> {
    let out = run(req)?;
    write_lines(path, &out.lines)?;
    Ok(out)
}

/// Written beside a temporary name and renamed, so a failed write never
/// leaves a partial `digest.jsonl` behind for the agent to read.
fn write_lines(path: &Path, lines: &[String]) -> Result<()> {
    let tmp = path.with_extension("jsonl.tmp");
    let mut body = lines.join("\n");
    if !body.is_empty() {
        body.push('\n');
    }
    std::fs::write(&tmp, body).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("renaming into {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prompt(text: &str) -> Prompt {
        Prompt {
            at: "2026-09-12T10:00:00Z".into(),
            text: text.into(),
            uuid: None,
            queued: false,
        }
    }

    #[test]
    fn human_prompt_filter_matches_session_index() {
        for wrapper in [
            "<command-message>grilling</command-message>",
            "<local-command-caveat>Caveat: The messages below were generated</local-command-caveat>",
            "<local-command-stdout>ok</local-command-stdout>",
            "Base directory for this skill: /home/u/.claude/skills/grilling\n\n# Grilling",
            "<task-notification>\n<task-id>1</task-id>",
            "Caveat: The messages below were generated by the user while running local commands.",
            "<system-reminder>\nnote\n</system-reminder>",
            "[Request interrupted by user]",
            "   ",
        ] {
            assert_eq!(human_prompt(wrapper), None, "{wrapper:?} is not a person typing");
        }
        assert_eq!(
            human_prompt(
                "<command-name>/model</command-name>\n<command-message>model</command-message>"
            ),
            Some("CMD /model".to_string())
        );
        assert_eq!(
            human_prompt("Traz a branch pro meu checkout"),
            Some("Traz a branch pro meu checkout".to_string())
        );
    }

    #[test]
    fn friction_vocabulary_and_reasks_match_session_index() {
        for hit in [
            "wtf is this",
            "I didn't get it",
            "i dont get the point",
            "não entendi nada",
            "nao entendi",
            "why are you editing that",
            "why a subagent here?",
            "stop that now",
            "I don't want a PR",
            "too much text",
            "not what I asked",
            "you are not listening",
            "isso está errado",
            "não é isso",
            "[request interrupted by user] redo",
        ] {
            let (friction, _) = friction_and_reasks(&[prompt(hit)]);
            assert_eq!(friction.len(), 1, "{hit:?} is in the vocabulary");
        }
        let (friction, _) = friction_and_reasks(&[prompt("all good, ship it")]);
        assert!(friction.is_empty());

        let same = "please make the checkout my current branch now";
        let (_, reasks) = friction_and_reasks(&[prompt(same), prompt(same), prompt("other")]);
        assert_eq!(reasks, 1);
    }

    #[test]
    fn session_line_is_capped_keeping_first_and_last_prompts() {
        let prompts: Vec<Prompt> = (0..300)
            .map(|i| prompt(&format!("p{i:03} {}", "x".repeat(1020))))
            .collect();
        let s = Session {
            provider: "claude",
            id: "s".into(),
            ..Default::default()
        };
        let line = session_line(|p, d| session_value(&s, p, d), &prompts, SESSION_LINE_MAX);
        assert!(line.len() <= SESSION_LINE_MAX, "{} bytes", line.len());
        let v: Value = serde_json::from_str(&line).unwrap();
        let kept = v["prompts"].as_array().unwrap();
        assert!(kept[0]["text"].as_str().unwrap().starts_with("p000"));
        assert!(kept.last().unwrap()["text"]
            .as_str()
            .unwrap()
            .starts_with("p299"));
        assert!(v["prompts_dropped"].as_u64().unwrap() > 0);
        assert_eq!(
            v["prompts_dropped"].as_u64().unwrap() as usize + kept.len(),
            300
        );
    }

    fn claude_line(ts: &str, text: &str) -> String {
        json!({
            "type": "user", "uuid": format!("u-{ts}"), "timestamp": ts,
            "cwd": "/ws", "sessionId": "s",
            "message": { "role": "user", "content": text },
        })
        .to_string()
    }

    fn request<'a>(ws: &'a Path, root: &Path, total_max: usize) -> Request<'a> {
        Request {
            workspace: ws,
            claude_roots: vec![root.to_path_buf()],
            codex_roots: vec![],
            window: super::super::window::explicit("2026-09-10", "2026-09-17").unwrap(),
            session_line_max: SESSION_LINE_MAX,
            total_max,
        }
    }

    #[test]
    fn total_cap_refuses_without_partial_output() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join(claude_slug(Path::new("/ws")));
        std::fs::create_dir_all(&dir).unwrap();
        for n in 0..3 {
            std::fs::write(
                dir.join(format!("s{n}.jsonl")),
                claude_line("2026-09-12T10:00:00Z", &"a long ask ".repeat(100)),
            )
            .unwrap();
        }
        let out = tempfile::tempdir().unwrap();
        let target = out.path().join("digest.jsonl");
        let err = run_and_write(&request(Path::new("/ws"), root.path(), 1_000), &target)
            .err()
            .expect("three sessions exceed a 1000 byte cap")
            .to_string();
        assert!(err.contains("1000 byte cap"), "{err}");
        assert!(err.contains("3 sessions"), "{err}");
        assert!(!target.exists());
        assert!(!out.path().join("digest.jsonl.tmp").exists());
    }

    #[test]
    fn oversized_lines_are_skipped_and_counted() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join(claude_slug(Path::new("/ws")));
        std::fs::create_dir_all(&dir).unwrap();
        let big = claude_line("2026-09-12T10:00:01Z", &"z".repeat(9 * 1024 * 1024));
        let body = format!(
            "{}\n{}\n{}\n",
            claude_line("2026-09-12T10:00:00Z", "first ask"),
            big,
            claude_line("2026-09-12T10:00:02Z", "second ask")
        );
        std::fs::write(dir.join("s.jsonl"), body).unwrap();
        let out = run(&request(Path::new("/ws"), root.path(), DIGEST_TOTAL_MAX)).unwrap();
        assert_eq!(out.tally.oversized_lines, 1);
        let v: Value = serde_json::from_str(&out.lines[0]).unwrap();
        let texts: Vec<&str> = v["prompts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["text"].as_str().unwrap())
            .collect();
        assert_eq!(texts, ["first ask", "second ask"]);
    }
}
