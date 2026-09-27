#![cfg(unix)]

mod common;

use common::start_daemon_with_handle;
use houston_core::daemon::{CreateParams, Daemon};
use houston_core::orchestrate::Submission;
use houston_protocol as proto;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

async fn wait_until(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if f() {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

fn review_routine(daemon: &Daemon, ws: &Path) -> proto::Routine {
    let proto::ServerMsg::Routines { routines, .. } = daemon
        .harness_review_create(&ws.display().to_string())
        .expect("the preset creates")
    else {
        panic!("harness_review_create answers Routines");
    };
    routines
        .into_iter()
        .find(|r| r.name.starts_with("Harness review"))
        .expect("the review routine is listed")
}

fn workspace(state: &Path) -> std::path::PathBuf {
    let ws = state.join("project");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::canonicalize(ws).unwrap()
}

/// Runs the review routine now with `cmd` standing in for the CLI, and returns
/// the run id and its pane.
async fn run_now(daemon: &Arc<Daemon>, routine: u32, cmd: Vec<String>) -> (u32, u32) {
    daemon.set_routine_pane_cmd_for_test(cmd);
    daemon.routine_run_now(routine).expect("run now");
    let proto::ServerMsg::RoutineRuns { runs } = daemon.routine_runs_list(Some(routine)) else {
        panic!("routine_runs_list answers RoutineRuns");
    };
    let run = runs.first().expect("run now records a run");
    (run.id, run.session_id.expect("the run opened a pane"))
}

fn sleeper() -> Vec<String> {
    vec!["sh".into(), "-c".into(), "sleep 30".into()]
}

#[tokio::test]
async fn preset_creates_a_paused_manual_review_routine() {
    let (_addr, state, daemon) = start_daemon_with_handle().await;
    let ws = workspace(state.path());
    let r = review_routine(&daemon, &ws);
    assert_eq!(r.name, "Harness review · project");
    assert!(!r.enabled, "a review spends tokens, so it starts paused");
    assert_eq!(
        r.cadence,
        proto::Cadence::Clock {
            hour: 9,
            minute: 0,
            weekdays: Some(vec![1])
        }
    );
    assert_eq!(r.engine, proto::AgentKind::Claude);
    assert_eq!(r.model, None);
    assert_eq!(r.effort, None);
    assert_eq!(r.permission_mode, proto::ChatPermissionMode::AcceptEdits);
    assert!(!r.isolate);
    assert_eq!(r.workspace_id.as_deref(), Some(ws.to_str().unwrap()));
    assert_eq!(r.prompt, houston_core::routines::HARNESS_REVIEW_PROMPT);
}

#[tokio::test]
async fn routine_pane_sees_its_run_id() {
    let (_addr, state, daemon) = start_daemon_with_handle().await;
    let ws = workspace(state.path());
    let r = review_routine(&daemon, &ws);
    let out = ws.join("run-env.txt");
    let cmd = vec![
        "sh".into(),
        "-c".into(),
        format!(
            "printf '%s' \"$HOUSTON_ROUTINE_RUN\" > '{}'; sleep 30",
            out.display()
        ),
    ];
    let (run_id, _) = run_now(&daemon, r.id, cmd).await;
    wait_until("the pane to write its env", || {
        std::fs::read_to_string(&out).is_ok_and(|s| !s.is_empty())
    })
    .await;
    assert_eq!(std::fs::read_to_string(&out).unwrap(), run_id.to_string());
}

#[tokio::test]
async fn hs_harness_wrapper_sits_beside_hs_pane() {
    let (_addr, state, daemon) = start_daemon_with_handle().await;
    let ws = workspace(state.path());
    let r = review_routine(&daemon, &ws);
    run_now(&daemon, r.id, sleeper()).await;
    let bin = ws.join(".houston/orchestration/bin");
    let wrapper = bin.join("hs-harness");
    assert!(bin.join("hs-pane").is_file());
    let script = std::fs::read_to_string(&wrapper).expect("the hs-harness wrapper exists");
    assert!(script.contains("exec "), "{script}");
    assert!(script.trim_end().ends_with("hs-harness \"$@\""), "{script}");
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(&wrapper).unwrap().permissions().mode();
    assert_eq!(mode & 0o111, 0o111, "the wrapper is executable");
}

#[tokio::test]
async fn routine_run_submit_reaches_the_operator_inbox() {
    let (_addr, state, daemon) = start_daemon_with_handle().await;
    let ws = workspace(state.path());
    let r = review_routine(&daemon, &ws);
    let (_, pane) = run_now(&daemon, r.id, sleeper()).await;
    let run_dir = ws.join(".houston/harness/r1");
    std::fs::create_dir_all(&run_dir).unwrap();
    std::fs::write(run_dir.join("report.md"), "# Report\n").unwrap();
    std::fs::write(
        run_dir.join("findings.json"),
        r#"{"schema":1,"findings":[]}"#,
    )
    .unwrap();

    let outcome = daemon
        .orchestrate_submit(
            pane,
            Submission {
                body: "2 findings: handoff, worktree guard".into(),
                summary: Some("Harness review: 2 findings".into()),
                artifacts: vec![
                    ".houston/harness/r1/report.md".into(),
                    ".houston/harness/r1/findings.json".into(),
                ],
                request_id: None,
            },
        )
        .expect("a routine run's pane can hand back");
    assert_eq!(outcome.reason, Some("routine"));

    let rows = daemon.inbox_rows_for_test(0);
    let row = rows
        .iter()
        .find(|row| row.id == outcome.row_id)
        .expect("the result is in the operator's inbox");
    assert_eq!(row.to_session, 0);
    assert_eq!(row.kind, "result");
    assert_eq!(row.reason.as_deref(), Some("routine"));
    assert_eq!(row.from_session, Some(pane));
    assert_eq!(
        row.artifacts,
        [
            run_dir.join("report.md").display().to_string(),
            run_dir.join("findings.json").display().to_string()
        ]
    );
}

#[tokio::test]
async fn parentless_non_routine_submit_is_still_refused() {
    let (_addr, state, daemon) = start_daemon_with_handle().await;
    let ws = workspace(state.path());
    let info = daemon
        .create_session(CreateParams {
            agent: proto::AgentKind::Custom,
            project_dir: ws,
            cmd: Some(sleeper()),
            cols: 80,
            rows: 24,
            cwd_from: None,
            shell_integration: false,
            auto_approve: false,
            acp: None,
            profile: None,
            prompt: None,
        })
        .unwrap();
    let err = daemon
        .orchestrate_submit(info.id, "a result".to_string().into())
        .expect_err("a pane nobody spawned has nobody to submit to")
        .to_string();
    assert!(
        err.contains("was not spawned by an agent — nothing to submit to"),
        "{err}"
    );
}
