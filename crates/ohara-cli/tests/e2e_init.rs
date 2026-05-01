//! End-to-end tests for `ohara init`.
//!
//! Each test creates a fresh tempdir-backed git repo, runs
//! `commands::init::run`, and inspects the on-disk state of
//! `.git/hooks/post-commit` (and, for later tests, `CLAUDE.md`).

use git2::Repository;
use std::fs;
use std::path::Path;

fn init_repo(dir: &Path) {
    Repository::init(dir).expect("git init");
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).expect("read file")
}

#[tokio::test]
async fn init_creates_post_commit_hook_in_fresh_repo() {
    let repo_dir = tempfile::tempdir().unwrap();
    init_repo(repo_dir.path());

    let args = ohara_cli::commands::init::Args {
        path: repo_dir.path().to_path_buf(),
        write_claude_md: false,
        force: false,
    };
    ohara_cli::commands::init::run(args).await.expect("init run");

    let hook = repo_dir.path().join(".git/hooks/post-commit");
    assert!(hook.exists(), "post-commit hook not created");
    let body = read(&hook);
    assert!(body.starts_with("#!/bin/sh"), "hook missing shebang: {body}");
    assert!(body.contains("# >>> ohara managed"), "begin marker missing");
    assert!(body.contains("# <<< ohara managed"), "end marker missing");
    assert!(body.contains("command -v ohara"), "PATH guard missing");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&hook).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755, "hook should be 0755, got {mode:o}");
    }
}

#[tokio::test]
async fn init_is_idempotent_when_run_twice() {
    let repo_dir = tempfile::tempdir().unwrap();
    init_repo(repo_dir.path());

    let mk = || ohara_cli::commands::init::Args {
        path: repo_dir.path().to_path_buf(),
        write_claude_md: false,
        force: false,
    };
    ohara_cli::commands::init::run(mk()).await.expect("first init");
    let hook = repo_dir.path().join(".git/hooks/post-commit");
    let first = read(&hook);

    ohara_cli::commands::init::run(mk()).await.expect("second init");
    let second = read(&hook);

    assert_eq!(first, second, "init should be idempotent");
    let begins = second.matches("# >>> ohara managed").count();
    let ends = second.matches("# <<< ohara managed").count();
    assert_eq!(begins, 1, "expected exactly one begin marker, got {begins}");
    assert_eq!(ends, 1, "expected exactly one end marker, got {ends}");
}

#[tokio::test]
async fn init_appends_to_existing_unmanaged_hook() {
    let repo_dir = tempfile::tempdir().unwrap();
    init_repo(repo_dir.path());
    let hooks_dir = repo_dir.path().join(".git/hooks");
    fs::create_dir_all(&hooks_dir).unwrap();
    let hook = hooks_dir.join("post-commit");
    fs::write(&hook, "#!/bin/sh\necho custom\n").unwrap();

    let args = ohara_cli::commands::init::Args {
        path: repo_dir.path().to_path_buf(),
        write_claude_md: false,
        force: false,
    };
    ohara_cli::commands::init::run(args).await.expect("init run");

    let body = read(&hook);
    assert!(body.contains("echo custom"), "user hook line was clobbered");
    assert!(body.contains("# >>> ohara managed"), "ohara marker missing after append");
    let user_pos = body.find("echo custom").unwrap();
    let begin_pos = body.find("# >>> ohara managed").unwrap();
    assert!(user_pos < begin_pos, "ohara block should be appended after user content");
}

#[tokio::test]
async fn init_with_write_claude_md_creates_file() {
    let repo_dir = tempfile::tempdir().unwrap();
    init_repo(repo_dir.path());

    let args = ohara_cli::commands::init::Args {
        path: repo_dir.path().to_path_buf(),
        write_claude_md: true,
        force: false,
    };
    ohara_cli::commands::init::run(args).await.expect("init run");

    let claude = repo_dir.path().join("CLAUDE.md");
    assert!(claude.exists(), "CLAUDE.md not created");
    let body = read(&claude);
    assert!(body.contains("<!-- ohara:start -->"), "begin marker missing");
    assert!(body.contains("<!-- ohara:end -->"), "end marker missing");
    assert!(body.contains("## ohara"), "stanza header missing");
    assert!(body.contains("find_pattern"), "stanza body missing");
}

#[tokio::test]
async fn init_write_claude_md_is_idempotent() {
    let repo_dir = tempfile::tempdir().unwrap();
    init_repo(repo_dir.path());
    let mk = || ohara_cli::commands::init::Args {
        path: repo_dir.path().to_path_buf(),
        write_claude_md: true,
        force: false,
    };
    ohara_cli::commands::init::run(mk()).await.expect("first init");
    let claude = repo_dir.path().join("CLAUDE.md");
    let first = read(&claude);

    ohara_cli::commands::init::run(mk()).await.expect("second init");
    let second = read(&claude);

    assert_eq!(first, second, "CLAUDE.md writer should be idempotent");
    assert_eq!(second.matches("<!-- ohara:start -->").count(), 1);
    assert_eq!(second.matches("<!-- ohara:end -->").count(), 1);
}

#[tokio::test]
async fn init_write_claude_md_preserves_other_content() {
    let repo_dir = tempfile::tempdir().unwrap();
    init_repo(repo_dir.path());
    let claude = repo_dir.path().join("CLAUDE.md");
    let preexisting = "# Project rules\n\nDo the right thing.\n";
    fs::write(&claude, preexisting).unwrap();

    let args = ohara_cli::commands::init::Args {
        path: repo_dir.path().to_path_buf(),
        write_claude_md: true,
        force: false,
    };
    ohara_cli::commands::init::run(args).await.expect("init run");

    let body = read(&claude);
    assert!(body.contains("# Project rules"), "user header was clobbered");
    assert!(body.contains("Do the right thing."), "user body was clobbered");
    assert!(body.contains("<!-- ohara:start -->"), "stanza not appended");
    let user_pos = body.find("Do the right thing.").unwrap();
    let stanza_pos = body.find("<!-- ohara:start -->").unwrap();
    assert!(user_pos < stanza_pos, "ohara stanza should be appended after user content");
}

#[cfg(unix)]
#[tokio::test]
async fn post_commit_hook_invokes_ohara_index_on_synthetic_commit() {
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;

    let repo_dir = tempfile::tempdir().unwrap();
    init_repo(repo_dir.path());

    // Configure user.name/user.email so git2 commit doesn't fail.
    let repo = git2::Repository::open(repo_dir.path()).unwrap();
    let mut cfg = repo.config().unwrap();
    cfg.set_str("user.name", "Test User").unwrap();
    cfg.set_str("user.email", "test@example.com").unwrap();
    drop(cfg);

    // Install the hook.
    let args = ohara_cli::commands::init::Args {
        path: repo_dir.path().to_path_buf(),
        write_claude_md: false,
        force: false,
    };
    ohara_cli::commands::init::run(args).await.expect("init run");

    // Build a shim `ohara` script that touches a sentinel and prepend its
    // dir to PATH so the hook resolves to the shim instead of any real
    // installation.
    let shim_dir = tempfile::tempdir().unwrap();
    let sentinel = shim_dir.path().join("invoked");
    let shim = shim_dir.path().join("ohara");
    let shim_body = format!("#!/bin/sh\ntouch {:?}\n", sentinel.to_string_lossy());
    fs::write(&shim, shim_body).unwrap();
    let mut perms = fs::metadata(&shim).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&shim, perms).unwrap();

    // Make a commit using the git CLI (so the post-commit hook actually
    // fires; libgit2 commits do not run hooks).
    fs::write(repo_dir.path().join("a.txt"), "hello").unwrap();
    let path_var = format!(
        "{}:{}",
        shim_dir.path().display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let status = Command::new("git")
        .args(["add", "a.txt"])
        .current_dir(repo_dir.path())
        .status()
        .expect("git add");
    assert!(status.success());
    let status = Command::new("git")
        .args(["commit", "-m", "init"])
        .current_dir(repo_dir.path())
        .env("PATH", &path_var)
        .status()
        .expect("git commit");
    assert!(status.success(), "git commit failed");

    assert!(
        sentinel.exists(),
        "sentinel was not touched — post-commit hook did not invoke `ohara`"
    );
}

#[tokio::test]
async fn init_replaces_managed_block_in_place() {
    let repo_dir = tempfile::tempdir().unwrap();
    init_repo(repo_dir.path());
    let hooks_dir = repo_dir.path().join(".git/hooks");
    fs::create_dir_all(&hooks_dir).unwrap();
    let hook = hooks_dir.join("post-commit");
    let stale = "\
#!/bin/sh
# >>> ohara managed (do not edit) >>>
echo this-is-stale-content
# <<< ohara managed <<<
";
    fs::write(&hook, stale).unwrap();

    let args = ohara_cli::commands::init::Args {
        path: repo_dir.path().to_path_buf(),
        write_claude_md: false,
        force: false,
    };
    ohara_cli::commands::init::run(args).await.expect("init run");

    let body = read(&hook);
    assert!(!body.contains("this-is-stale-content"), "stale managed block was not replaced");
    assert!(body.contains("command -v ohara"), "current managed body not written");
    assert_eq!(
        body.matches("# >>> ohara managed").count(),
        1,
        "expected exactly one begin marker after replacement"
    );
}
