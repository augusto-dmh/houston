//! What a cleanup pass asks git about one managed worktree. The daemon owns the
//! order of the checks and the removal; these answer one question each.

use std::path::Path;

fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = crate::spawn::command("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The branch checked out in `tree`; `Some(None)` on a detached HEAD.
pub fn current_branch(tree: &Path) -> Option<Option<String>> {
    let out = git(tree, &["branch", "--show-current"])?;
    let name = out.trim();
    Some((!name.is_empty()).then(|| name.to_string()))
}

/// Every modified, staged or untracked file, one per path. The flags are explicit so a
/// repository's `status.showUntrackedFiles` or submodule settings cannot hide one.
pub fn dirty_files(tree: &Path) -> Option<u32> {
    let out = git(
        tree,
        &[
            "status",
            "--porcelain",
            "--untracked-files=all",
            "--ignore-submodules=none",
        ],
    )?;
    Some(out.lines().filter(|l| !l.trim().is_empty()).count() as u32)
}

/// Loose ignored files (`.env`, local settings) that `git worktree remove` would delete.
/// A wholly ignored directory is listed once with a trailing `/` and is taken as build
/// output (`target/`, `node_modules/`), so it is not counted.
pub fn ignored_files(tree: &Path) -> Option<u32> {
    let out = git(
        tree,
        &[
            "status",
            "--porcelain",
            "--ignored=traditional",
            "--untracked-files=normal",
        ],
    )?;
    Some(
        out.lines()
            .filter_map(|l| l.strip_prefix("!! "))
            .filter(|p| !p.trim_end_matches('"').ends_with('/'))
            .count() as u32,
    )
}

pub fn has_object(tree: &Path, oid: &str) -> bool {
    git(tree, &["cat-file", "-e", &format!("{oid}^{{commit}}")]).is_some()
}

/// Tries every remote, since only the base repository carries `refs/pull/*`.
pub fn fetch_pr_head(tree: &Path, pr: u32, oid: &str) -> bool {
    let Some(remotes) = git(tree, &["remote"]) else {
        return false;
    };
    let refspec = format!("refs/pull/{pr}/head");
    for remote in remotes.lines().map(str::trim).filter(|r| !r.is_empty()) {
        let _ = git(tree, &["fetch", "-q", remote, &refspec]);
        if has_object(tree, oid) {
            return true;
        }
    }
    false
}

/// Local commits the PR head does not contain; `None` when git cannot answer.
pub fn commits_outside(tree: &Path, pr_head: &str) -> Option<u32> {
    git(tree, &["rev-list", "--count", &format!("{pr_head}..HEAD")])?
        .trim()
        .parse()
        .ok()
}

/// After a `git fetch --prune`, a branch whose upstream was deleted reads `[gone]`.
pub fn upstream_gone(repo: &Path, branch: &str) -> bool {
    git(
        repo,
        &[
            "for-each-ref",
            "--format=%(upstream:track)",
            &format!("refs/heads/{branch}"),
        ],
    )
    .is_some_and(|t| t.trim() == "[gone]")
}

pub fn fetch_prune(repo: &Path) {
    let _ = git(repo, &["fetch", "-q", "--prune", "--all"]);
}

/// The sum of file sizes under `path`, symlinks not followed.
pub fn tree_bytes(path: &Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(meta) = entry.path().symlink_metadata() else {
                continue;
            };
            if meta.is_dir() {
                stack.push(entry.path());
            } else if meta.is_file() {
                total += meta.len();
            }
        }
    }
    total
}

/// GitHub's `mergedAt` (`YYYY-MM-DDTHH:MM:SSZ`) as Unix milliseconds.
pub fn parse_github_time(s: &str) -> Option<i64> {
    let s = s.strip_suffix('Z')?;
    let (date, time) = s.split_once('T')?;
    let mut d = date.splitn(3, '-').map(|p| p.parse::<i64>().ok());
    let (y, m, day) = (d.next()??, d.next()??, d.next()??);
    let mut t = time.splitn(3, ':').map(|p| p.parse::<i64>().ok());
    let (hh, mm, ss) = (t.next()??, t.next()??, t.next()??);
    if !(1..=12).contains(&m) || !(1..=31).contains(&day) {
        return None;
    }
    // Days-from-civil (Howard Hinnant), valid for the proleptic Gregorian calendar.
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(((days * 86_400) + hh * 3600 + mm * 60 + ss) * 1000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_time_parses_to_unix_millis() {
        assert_eq!(parse_github_time("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_github_time("2026-09-29T15:15:18Z"),
            Some(1_790_694_918_000)
        );
        assert_eq!(parse_github_time("2026-09-29 15:15:18"), None);
    }
}
