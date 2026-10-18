//! `wordcraft-cli mcp` confines files to the working directory unless told otherwise, says which
//! roots are in force when it starts, and refuses `--unconfined` with root flags (#2745).

use std::path::Path;
use std::process::{Command, Output};

fn mcp(dir: &Path, args: &[&str]) -> Output {
    // `output()` gives the child no stdin: the server sees its client hang up and ends.
    Command::new(env!("CARGO_BIN_EXE_wordcraft-cli")).arg("mcp").args(args).current_dir(dir).output().unwrap()
}

#[test]
fn the_headless_server_names_the_roots_in_force() {
    let dir = std::env::temp_dir().join(format!("wc-cli-mcp-files-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let out = mcp(&dir, &[]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "status {:?}, stderr: {stderr}", out.status);
    let notice = stderr.lines().find(|l| l.contains("files confined")).unwrap_or_default();
    assert!(notice.contains("read root") && notice.contains("write root"), "stderr: {stderr}");
    let name = dir.file_name().unwrap().to_string_lossy().into_owned();
    assert_eq!(notice.matches(name.as_str()).count(), 2, "both roots are the working directory: {notice}");

    let out = mcp(&dir, &["--unconfined"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "status {:?}, stderr: {stderr}", out.status);
    assert!(stderr.contains("files not confined (--unconfined)"), "stderr: {stderr}");

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn unconfined_with_root_flags_exits_with_status_2() {
    let dir = std::env::temp_dir();
    let root = dir.to_string_lossy().into_owned();
    for args in [vec!["--unconfined", "--automation-read-root", &root], vec!["--automation-write-root", &root, "--unconfined"]] {
        let out = mcp(&dir, &args);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(2), "{args:?}: stderr: {stderr}");
        assert!(stderr.contains("--unconfined") && !stderr.contains("files confined"), "{args:?}: stderr: {stderr}");
    }
}
