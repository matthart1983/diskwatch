//! The release guard: what `.github/workflows/release.yml` checks, through
//! `scripts/release-guard.sh`, before it builds anything.
//!
//! Every red main run since 1 Sep was a release commit that shipped anyway,
//! because nothing tied the release to CI. These tests need bash, git and jq,
//! so they're ignored by default, which keeps packagers' build sandboxes
//! working. CI runs them with `-- --ignored`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const GREEN: &str =
    r#"[{"databaseId":1,"status":"completed","conclusion":"success","event":"push","url":"u"}]"#;
const RED: &str =
    r#"[{"databaseId":1,"status":"completed","conclusion":"failure","event":"push","url":"u"}]"#;
const RUNNING: &str =
    r#"[{"databaseId":1,"status":"in_progress","conclusion":"","event":"push","url":"u"}]"#;
const NO_RUNS: &str = "[]";

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
        .args(args)
        .current_dir(dir)
        .output()
        .expect("git");
    assert!(out.status.success(), "git {args:?}: {out:?}");
}

/// A repository with one commit on main, and a fake `gh` that prints `runs`.
struct Fixture {
    dir: PathBuf,
}

impl Fixture {
    fn new(name: &str, version: &str, changelog: &str, runs: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("diskwatch-guard-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("bin")).unwrap();
        fs::write(
            dir.join("Cargo.toml"),
            format!("[package]\nname = \"x\"\nversion = \"{version}\"\n"),
        )
        .unwrap();
        fs::write(dir.join("CHANGELOG.md"), changelog).unwrap();
        fs::write(dir.join(".gitignore"), "bin\n").unwrap();
        let gh = dir.join("bin/gh");
        fs::write(&gh, format!("#!/bin/sh\ncat <<'JSON'\n{runs}\nJSON\n")).unwrap();
        make_executable(&gh);
        git(&dir, &["init", "-q", "-b", "main"]);
        git(&dir, &["add", "-A"]);
        git(&dir, &["commit", "-q", "-m", "release"]);
        git(&dir, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
        Self { dir }
    }

    /// A commit that origin/main doesn't have.
    fn commit_off_main(&self) {
        fs::write(self.dir.join("extra"), "x").unwrap();
        git(&self.dir, &["add", "-A"]);
        git(&self.dir, &["commit", "-q", "-m", "off main"]);
    }

    fn guard(&self, tag: &str) -> Output {
        let path = format!(
            "{}:{}",
            self.dir.join("bin").display(),
            std::env::var("PATH").unwrap()
        );
        Command::new("bash")
            .arg(root().join("scripts/release-guard.sh"))
            .arg(tag)
            .current_dir(&self.dir)
            .env("PATH", path)
            .env("GUARD_POLL", "0")
            .env("GUARD_FIRST_RUN", "0")
            .env("GUARD_TIMEOUT", "0")
            .output()
            .expect("guard")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

#[cfg(unix)]
fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

#[cfg(not(unix))]
fn make_executable(_: &Path) {}

fn says(out: &Output, needle: &str) -> bool {
    String::from_utf8_lossy(&out.stdout).contains(needle)
}

#[test]
#[ignore = "needs bash, git and jq"]
fn a_green_tagged_release_on_main_passes() {
    let f = Fixture::new("green", "1.2.3", "## [1.2.3] - today\n", GREEN);
    let out = f.guard("v1.2.3");
    assert!(out.status.success(), "{out:?}");
}

#[test]
#[ignore = "needs bash, git and jq"]
fn a_red_commit_fails() {
    let f = Fixture::new("red", "1.2.3", "## [1.2.3] - today\n", RED);
    let out = f.guard("v1.2.3");
    assert!(!out.status.success(), "{out:?}");
    assert!(says(&out, "CI did not pass"), "{out:?}");
}

#[test]
#[ignore = "needs bash, git and jq"]
fn a_commit_off_main_fails_before_asking_about_ci() {
    let f = Fixture::new("off", "1.2.3", "## [1.2.3] - today\n", GREEN);
    f.commit_off_main();
    let out = f.guard("v1.2.3");
    assert!(!out.status.success(), "{out:?}");
    assert!(says(&out, "not on origin/main"), "{out:?}");
}

#[test]
#[ignore = "needs bash, git and jq"]
fn a_tag_that_disagrees_with_the_crate_fails() {
    let f = Fixture::new("ver", "1.2.3", "## [1.2.3] - today\n", GREEN);
    let out = f.guard("v1.2.4");
    assert!(!out.status.success(), "{out:?}");
    assert!(says(&out, "Cargo.toml says 1.2.3"), "{out:?}");
}

#[test]
#[ignore = "needs bash, git and jq"]
fn a_release_with_no_changelog_section_fails() {
    let f = Fixture::new("log", "1.2.3", "## [1.2.2] - before\n", GREEN);
    let out = f.guard("v1.2.3");
    assert!(!out.status.success(), "{out:?}");
    assert!(says(&out, "no '## [1.2.3]' heading"), "{out:?}");
}

#[test]
#[ignore = "needs bash, git and jq"]
fn no_ci_run_fails_rather_than_passing_by_default() {
    let f = Fixture::new("none", "1.2.3", "## [1.2.3] - today\n", NO_RUNS);
    let out = f.guard("v1.2.3");
    assert!(!out.status.success(), "{out:?}");
    assert!(says(&out, "No CI run"), "{out:?}");
}

#[test]
#[ignore = "needs bash, git and jq"]
fn ci_still_running_at_the_deadline_fails() {
    let f = Fixture::new("slow", "1.2.3", "## [1.2.3] - today\n", RUNNING);
    let out = f.guard("v1.2.3");
    assert!(!out.status.success(), "{out:?}");
    assert!(says(&out, "still running"), "{out:?}");
}

/// The jobs in the workflow and what each one needs. Reads this repository's
/// layout (job names at two spaces, their keys at four); not a YAML parser.
fn jobs(workflow: &str) -> Vec<(String, Vec<String>)> {
    let mut jobs: Vec<(String, Vec<String>)> = Vec::new();
    let mut in_jobs = false;
    for line in workflow.lines() {
        if !line.is_empty() && !line.starts_with([' ', '#']) {
            in_jobs = line == "jobs:";
            continue;
        }
        if !in_jobs {
            continue;
        }
        if let Some(name) = line.strip_prefix("  ").and_then(|l| l.strip_suffix(':')) {
            if !name.starts_with([' ', '#']) {
                jobs.push((name.to_string(), Vec::new()));
                continue;
            }
        }
        if let (Some(needs), Some((_, list))) = (line.strip_prefix("    needs:"), jobs.last_mut()) {
            *list = needs
                .trim()
                .trim_matches(['[', ']'])
                .split(',')
                .map(|n| n.trim().to_string())
                .collect();
        }
    }
    jobs
}

#[test]
#[ignore = "needs the repository's workflow files"]
fn every_release_job_waits_for_the_guard() {
    let workflow = fs::read_to_string(root().join(".github/workflows/release.yml")).unwrap();
    let jobs = jobs(&workflow);
    assert!(jobs.iter().any(|(n, _)| n == "guard"), "no guard job");
    assert!(jobs.len() > 1, "found no other jobs: {jobs:?}");
    for (job, _) in jobs.iter().filter(|(n, _)| n != "guard") {
        let mut pending = vec![job.clone()];
        let mut guarded = false;
        while let Some(next) = pending.pop() {
            if next == "guard" {
                guarded = true;
                break;
            }
            let needs = jobs.iter().find(|(n, _)| *n == next);
            pending.extend(needs.map(|(_, list)| list.clone()).unwrap_or_default());
        }
        assert!(guarded, "{job} can run without the guard");
    }
}
