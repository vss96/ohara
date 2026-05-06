//! `ohara plan` e2e: print-only by default (issue #37); `--write` opts in
//! to writing a marker-fenced `.oharaignore`.

use std::path::Path;
use std::process::Command;

fn ohara_bin() -> String {
    env!("CARGO_BIN_EXE_ohara").to_string()
}

/// Build a small repo where one top-level dir dominates commit count.
fn init_noisy_repo(repo: &Path) {
    Command::new("git").arg("init").arg(repo).output().unwrap();
    for i in 0..10 {
        let p = repo.join("noise");
        std::fs::create_dir_all(&p).unwrap();
        std::fs::write(p.join(format!("f{i}.txt")), "x").unwrap();
        Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["add", "."])
            .output()
            .unwrap();
        Command::new("git")
            .arg("-C")
            .arg(repo)
            .args([
                "-c",
                "user.email=a@a",
                "-c",
                "user.name=a",
                "commit",
                "-m",
                &format!("noise {i}"),
            ])
            .output()
            .unwrap();
    }
    std::fs::write(repo.join("README.md"), "real\n").unwrap();
    Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["add", "."])
        .output()
        .unwrap();
    Command::new("git")
        .arg("-C")
        .arg(repo)
        .args([
            "-c",
            "user.email=a@a",
            "-c",
            "user.name=a",
            "commit",
            "-m",
            "real",
        ])
        .output()
        .unwrap();
}

#[test]
fn plan_default_is_print_only_and_does_not_write_oharaignore() {
    // Issue #37: the default `ohara plan` path must NOT auto-write
    // `.oharaignore`. The previous auto-write silently excluded
    // top-level engine dirs (e.g. QuestDB's `core/`). Demoted to
    // print-only; users opt in with `--write`.
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_noisy_repo(repo);

    let out = Command::new(ohara_bin())
        .args(["plan"])
        .arg(repo)
        .output()
        .expect("run ohara plan");
    assert!(
        out.status.success(),
        "plan failed: status={:?} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );

    assert!(
        !repo.join(".oharaignore").exists(),
        ".oharaignore must NOT be written by default (issue #37)"
    );

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("noise/"),
        "high-share dir still surfaced in stdout suggestions: {stdout}"
    );
    assert!(
        stdout.contains("--write"),
        "stdout must hint at `--write` to apply: {stdout}"
    );
}

#[test]
fn plan_write_writes_marker_fenced_oharaignore() {
    // Issue #37: `--write` is the explicit opt-in to the historical
    // auto-write behavior. No prompt, no separate `--yes`.
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_noisy_repo(repo);

    let out = Command::new(ohara_bin())
        .args(["plan", "--write"])
        .arg(repo)
        .output()
        .expect("run ohara plan --write");
    assert!(
        out.status.success(),
        "plan --write failed: status={:?} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );

    let body = std::fs::read_to_string(repo.join(".oharaignore"))
        .expect(".oharaignore must exist after --write");
    assert!(body.contains("ohara plan v"), "begin marker missing");
    assert!(body.contains("end auto-generated"), "end marker missing");
    assert!(
        body.contains("noise/"),
        "high-share dir not suggested: {body}"
    );
}

#[test]
fn plan_replace_requires_write() {
    // Issue #37: `--replace` only makes sense when writing; clap
    // should refuse `--replace` without `--write`.
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_noisy_repo(repo);

    let out = Command::new(ohara_bin())
        .args(["plan", "--replace"])
        .arg(repo)
        .output()
        .expect("run ohara plan --replace");
    assert!(
        !out.status.success(),
        "`--replace` without `--write` must fail; stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !repo.join(".oharaignore").exists(),
        ".oharaignore must NOT be created when `--replace` is rejected"
    );
}
