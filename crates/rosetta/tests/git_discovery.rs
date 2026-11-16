use rosetta::{index, storage::Storage};
use std::{path::Path, process::Command};

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_rosetta")
}

fn index_command(database: &Path, source: &Path) -> Command {
    let mut command = Command::new(binary());
    command
        .arg("--database")
        .arg(database)
        .args([
            "--storage",
            "sqlite",
            "index",
            "--name",
            "fixture",
            "--path",
        ])
        .arg(source);
    command
}

#[cfg(unix)]
#[tokio::test]
async fn git_failures_never_fall_back_to_reading_ignored_files() {
    use std::os::unix::fs::PermissionsExt;
    let db = tempfile::tempdir().unwrap();
    let path = db.path().join("graph.db");
    let root = tempfile::tempdir().unwrap();
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(root.path())
            .args(["init", "-q"])
            .status()
            .unwrap()
            .success()
    );
    std::fs::write(root.path().join("app.py"), "def tracked():\n    pass\n").unwrap();
    std::fs::write(root.path().join(".gitignore"), "private.json\n").unwrap();
    std::fs::write(
        root.path().join("private.json"),
        "ignored-sensitive-fixture-marker\n",
    )
    .unwrap();
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(root.path())
            .args(["add", "app.py", ".gitignore"])
            .status()
            .unwrap()
            .success()
    );
    let storage = Storage::sqlite(&path).await.unwrap();
    index::index(&storage, root.path(), "fixture", None)
        .await
        .unwrap();
    let revision = storage.revision().await.unwrap();
    let fake_bin = tempfile::tempdir().unwrap();
    for diagnostic in [
        "fatal: detected dubious ownership",
        "fatal: not a git repository",
    ] {
        let git = fake_bin.path().join("git");
        std::fs::write(
            &git,
            format!("#!/bin/sh\nprintf '%s\\n' '{diagnostic}' >&2\nexit 128\n"),
        )
        .unwrap();
        std::fs::set_permissions(&git, std::fs::Permissions::from_mode(0o755)).unwrap();
        let result = index_command(&path, root.path())
            .env("PATH", fake_bin.path())
            .output()
            .unwrap();
        assert!(!result.status.success());
        assert!(result.stdout.is_empty());
        assert!(
            String::from_utf8_lossy(&result.stderr).contains("Git repository discovery failed")
        );
        assert_eq!(storage.revision().await.unwrap(), revision);
        assert!(
            !storage.load().await.unwrap().repositories["fixture"]
                .files
                .contains_key("private.json")
        );
    }
    std::fs::remove_file(fake_bin.path().join("git")).unwrap();
    let result = index_command(&path, root.path())
        .env("PATH", fake_bin.path())
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert_eq!(storage.revision().await.unwrap(), revision);
}

#[test]
fn credential_roots_and_the_entire_home_are_refused() {
    let db = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    for directory in [".ssh", ".kube"] {
        let root = home.path().join(directory);
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("config"), "synthetic fixture only\n").unwrap();
        let result = index_command(&db.path().join("graph.db"), &root)
            .output()
            .unwrap();
        assert!(!result.status.success());
        assert!(
            String::from_utf8_lossy(&result.stderr)
                .contains("refusing to index a credential directory")
        );
    }
    let result = index_command(&db.path().join("graph.db"), home.path())
        .env("HOME", home.path())
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(
        String::from_utf8_lossy(&result.stderr)
            .contains("refusing to index the entire home directory")
    );
}

#[test]
fn normal_plain_directories_still_work_and_bare_repositories_are_refused() {
    let db = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("app.py"), "def plain():\n    pass\n").unwrap();
    assert!(
        index_command(&db.path().join("plain.db"), root.path())
            .output()
            .unwrap()
            .status
            .success()
    );
    let bare = tempfile::tempdir().unwrap();
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(bare.path())
            .args(["init", "--bare", "-q"])
            .status()
            .unwrap()
            .success()
    );
    let result = index_command(&db.path().join("bare.db"), bare.path())
        .output()
        .unwrap();
    assert!(!result.status.success());
}
