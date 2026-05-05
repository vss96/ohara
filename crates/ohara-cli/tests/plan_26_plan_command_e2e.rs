//! Plan-26 end-to-end: `ohara plan --yes` produces a `.oharaignore`
//! at the repo root with the auto-generated marker block.

use std::process::Command;

fn ohara_bin() -> String {
    env!("CARGO_BIN_EXE_ohara").to_string()
}

#[test]
fn plan_yes_writes_marker_fenced_oharaignore() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();

    // Synthesize a repo where one top-level dir dominates commit count.
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

    let out = Command::new(ohara_bin())
        .args(["plan", "--yes"])
        .arg(repo)
        .output()
        .expect("run ohara plan");
    assert!(
        out.status.success(),
        "plan failed: status={:?} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );

    let body = std::fs::read_to_string(repo.join(".oharaignore")).expect(".oharaignore must exist");
    assert!(body.contains("ohara plan v"), "begin marker missing");
    assert!(body.contains("end auto-generated"), "end marker missing");
    assert!(
        body.contains("noise/"),
        "high-share dir not suggested: {body}"
    );
}
