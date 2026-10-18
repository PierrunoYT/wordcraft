//! The control channel's key file, one rule for the app (which writes it) and the MCP bridge
//! (which reads it): `<settings>/control-key.<instance>`. See `docs/control-protocol.md`, Keys.

use std::io::Write;
use std::path::{Path, PathBuf};

/// The app's settings folder (preferences, logs, key files): macOS
/// `~/Library/Application Support/WordCraft`, Windows `%APPDATA%\WordCraft`, otherwise
/// `$XDG_CONFIG_HOME/wordcraft` or `~/.config/wordcraft`.
/// `WORDCRAFT_PROFILE_DIR` overrides the entire folder for isolated runs.
pub fn settings_dir() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("WORDCRAFT_PROFILE_DIR").filter(|p| !p.is_empty()) {
        return Some(PathBuf::from(path));
    }
    if cfg!(target_os = "macos") {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Application Support/WordCraft"))
    } else if cfg!(windows) {
        std::env::var_os("APPDATA").map(|a| PathBuf::from(a).join("WordCraft"))
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
            .map(|c| c.join("wordcraft"))
    }
}

/// The `instance-id=` value of a `/.flatpak-info` file, only when it is `[A-Za-z0-9_-]+` (it
/// becomes part of a file name).
pub fn parse_instance_id(info: &str) -> Option<String> {
    let v = info.lines().find_map(|l| l.strip_prefix("instance-id="))?.trim();
    (!v.is_empty() && v.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')).then(|| v.to_string())
}

/// The Flatpak instance id, or `None` outside Flatpak.
pub fn instance_id() -> Option<String> {
    parse_instance_id(&std::fs::read_to_string("/.flatpak-info").ok()?)
}

/// `<settings>/control-key.<instance>`, with `<instance>` the Flatpak instance id or
/// `local-<port>`.
pub fn key_path(settings: &Path, port: u16) -> PathBuf {
    key_path_for(settings, instance_id().as_deref(), port)
}

fn key_path_for(settings: &Path, instance: Option<&str>, port: u16) -> PathBuf {
    match instance {
        Some(id) => settings.join(format!("control-key.{id}")),
        None => settings.join(format!("control-key.local-{port}")),
    }
}

/// Write the key file `p`: a stale one (from a run that did not exit cleanly) is replaced. On
/// Unix the new file is created with mode 0600; on Windows it gets an owner-only ACL
/// ([`make_private`]) before the key goes in. When that fails the file keeps its folder's
/// permissions and the result is a warning to log, unless `strict` (`WORDCRAFT_PROFILE_DIR` names
/// the folder, which may be shared): then the file is removed and no key is written.
pub fn write_key_file(p: &Path, key: &str, strict: bool) -> Result<Option<String>, String> {
    write_key_file_with(p, key, strict, make_private)
}

/// [`write_key_file`] with `restrict` in place of [`make_private`].
pub fn write_key_file_with(p: &Path, key: &str, strict: bool, restrict: impl Fn(&Path) -> Result<(), String>) -> Result<Option<String>, String> {
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d).map_err(|e| format!("cannot create {}: {e}", d.display()))?;
    }
    let _ = std::fs::remove_file(p);
    let mut open = std::fs::OpenOptions::new();
    open.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut open, 0o600);
    // No other process may open the file while this handle is open, so nobody can hold a handle
    // from before the ACL is restricted (changing the ACL doesn't revoke one). Setting the ACL
    // needs no share access.
    #[cfg(windows)]
    std::os::windows::fs::OpenOptionsExt::share_mode(&mut open, 0);
    let mut f = open.open(p).map_err(|e| format!("cannot create {}: {e}", p.display()))?;
    let warning = match restrict(p) {
        Ok(()) => None,
        Err(e) if strict => {
            drop(f);
            let _ = std::fs::remove_file(p);
            return Err(format!("cannot make {} private ({e}), and WORDCRAFT_PROFILE_DIR may be a shared folder", p.display()));
        }
        Err(e) => Some(format!("control key file {} keeps its folder's permissions: {e}", p.display())),
    };
    f.write_all(key.as_bytes()).map_err(|e| format!("cannot write {}: {e}", p.display()))?;
    Ok(warning)
}

/// Make `p` readable by the current user only. Unix: nothing to do, [`write_key_file`] creates
/// the file with mode 0600.
#[cfg(not(windows))]
pub fn make_private(_: &Path) -> Result<(), String> {
    Ok(())
}

/// Make `p` readable by the current user only. Windows: an ACL with one entry, full control for
/// the current user (by SID), with no entries inherited from the folder and none added by other
/// software. Setting an ACL directly needs unsafe Win32 calls, which the workspace forbids, so
/// this runs the system's own `whoami.exe` and `icacls.exe` from `%SystemRoot%\System32` (never
/// through `PATH`). A child process inherits the parent's inheritable handles (on Windows that
/// includes cloned sockets), so call it before serving connections.
#[cfg(windows)]
pub fn make_private(p: &Path) -> Result<(), String> {
    let p = std::path::absolute(p).map_err(|e| format!("cannot resolve {}: {e}", p.display()))?;
    let out = run_system_tool("whoami.exe", &["/user", "/fo", "csv", "/nh"].map(std::ffi::OsString::from), true)?;
    let out = String::from_utf8_lossy(&out);
    let sid = parse_whoami_sid(&out).ok_or("whoami gave no user SID")?;
    for args in icacls_steps(&p, sid) {
        run_system_tool("icacls.exe", &args, false)?;
    }
    Ok(())
}

/// The user's SID in the output of `whoami /user /fo csv /nh` (`"domain\user","S-1-5-21-…"`):
/// the last field, only when it looks like a SID (`S-` then digits and dashes).
#[cfg(any(windows, test))]
fn parse_whoami_sid(out: &str) -> Option<&str> {
    let line = out.lines().map(str::trim).rfind(|l| !l.is_empty())?;
    let sid = line.rsplit(',').next()?.trim().trim_matches('"');
    let rest = sid.strip_prefix("S-")?;
    let digits_and_dashes = rest.chars().all(|c| c.is_ascii_digit() || c == '-');
    (rest.len() <= 200 && rest.starts_with(|c: char| c.is_ascii_digit()) && digits_and_dashes).then_some(sid)
}

/// The two `icacls` runs that leave `p` with one entry: `icacls <p> /reset` drops the explicit
/// entries (leaving the inherited ones), then `icacls <p> /inheritance:r /grant:r *<sid>:F`
/// drops the inherited ones and grants the user full control.
#[cfg(any(windows, test))]
fn icacls_steps(p: &Path, sid: &str) -> [Vec<std::ffi::OsString>; 2] {
    let p = p.as_os_str().to_owned();
    [vec![p.clone(), "/reset".into()], vec![p, "/inheritance:r".into(), "/grant:r".into(), format!("*{sid}:F").into()]]
}

/// A system tool that doesn't finish in this time is killed (the ACL is then not set).
#[cfg(windows)]
const TOOL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Run `%SystemRoot%\System32\<exe>` without a console window: its standard output (at most
/// 64 KiB) when `capture`, and an error unless it exits with success.
#[cfg(windows)]
fn run_system_tool(exe: &str, args: &[std::ffi::OsString], capture: bool) -> Result<Vec<u8>, String> {
    use std::io::Read;
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let root = std::env::var_os("SystemRoot").filter(|r| !r.is_empty()).ok_or("SystemRoot is not set")?;
    let path = PathBuf::from(root).join("System32").join(exe);
    let mut child = Command::new(&path)
        .args(args)
        .stdin(Stdio::null())
        .stdout(if capture { Stdio::piped() } else { Stdio::null() })
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map_err(|e| format!("cannot run {}: {e}", path.display()))?;
    let started = std::time::Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() < TOOL_TIMEOUT => std::thread::sleep(std::time::Duration::from_millis(10)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("{exe} did not finish"));
            }
            Err(e) => return Err(format!("{exe}: {e}")),
        }
    };
    let mut out = Vec::new();
    if let Some(stdout) = child.stdout.take() {
        let _ = stdout.take(64 * 1024).read_to_end(&mut out);
    }
    if !status.success() {
        return Err(format!("{exe} failed ({status})"));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn instance_id_only_safe_values() {
        assert_eq!(parse_instance_id("[Instance]\ninstance-id=abc_123-X\n").as_deref(), Some("abc_123-X"));
        assert_eq!(parse_instance_id("instance-id= 42 \n").as_deref(), Some("42"));
        // It ends up in a file name.
        assert_eq!(parse_instance_id("instance-id=../etc\n"), None);
        assert_eq!(parse_instance_id("instance-id=a/b\n"), None);
        assert_eq!(parse_instance_id("instance-id=a b\n"), None);
        assert_eq!(parse_instance_id("instance-id=..\n"), None);
        assert_eq!(parse_instance_id("instance-id=\n"), None);
        assert_eq!(parse_instance_id("app=ai.storyteller.wordcraft\n"), None);
    }

    #[test]
    fn key_file_per_instance() {
        let dir = Path::new("/settings/wordcraft");
        assert_eq!(key_path_for(dir, Some("1234"), 7981), dir.join("control-key.1234"));
        assert_eq!(key_path_for(dir, None, 7981), dir.join("control-key.local-7981"));
        assert_eq!(key_path_for(dir, None, 7982), dir.join("control-key.local-7982"));
        if instance_id().is_none() {
            assert_eq!(key_path(dir, 7981), dir.join("control-key.local-7981"));
        }
    }

    const KEY: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("wordcraft-control-key-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn a_stale_key_file_is_replaced() {
        let dir = temp_dir("stale");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("control-key.test");
        std::fs::write(&p, "old").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        assert_eq!(write_key_file(&p, KEY, true).unwrap(), None);
        assert_eq!(std::fs::read_to_string(&p).unwrap(), KEY);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_key_file_that_cannot_be_made_private() {
        let dir = temp_dir("unrestricted");
        let p = dir.join("control-key.test");
        let fail = |_: &Path| Err("no ACL".to_string());
        // WORDCRAFT_PROFILE_DIR (strict): refused, and no file is left behind.
        let err = write_key_file_with(&p, KEY, true, fail).unwrap_err();
        assert!(err.contains("no ACL") && err.contains("WORDCRAFT_PROFILE_DIR"), "{err}");
        assert!(!p.exists());
        // The default per-user folder: written anyway, with a warning.
        let warning = write_key_file_with(&p, KEY, false, fail).unwrap().unwrap();
        assert!(warning.contains("no ACL"), "{warning}");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), KEY);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn whoami_sid_and_icacls_arguments() {
        let sid = "S-1-5-21-908040966-1656599027-2995977752-1001";
        assert_eq!(parse_whoami_sid(&format!("\"pc\\pierre\",\"{sid}\"\r\n")), Some(sid));
        assert_eq!(parse_whoami_sid(&format!("\r\n\"dom\\a, b\",\"{sid}\"\r\n\r\n")), Some(sid));
        assert_eq!(parse_whoami_sid("\"pc\\pierre\",\"S-1-5-18\""), Some("S-1-5-18"));
        // Not a SID: never handed to icacls.
        for bad in ["", "\r\n", "\"pc\\pierre\"", "\"pc\\pierre\",\"S-\"", "\"x\",\"S-1-5 /grant Everyone\"", "\"x\",\"S-x\"", "\"x\",\"*S-1-5-18\""]
        {
            assert_eq!(parse_whoami_sid(bad), None, "{bad:?}");
        }
        assert_eq!(parse_whoami_sid(&format!("\"x\",\"S-1{}\"", "-1".repeat(200))), None);
        let p = Path::new(r"C:\Users\me\AppData\Roaming\WordCraft\control-key.local-7981");
        let steps = icacls_steps(p, sid).map(|args| args.iter().map(|a| a.to_string_lossy().into_owned()).collect::<Vec<_>>());
        let path = p.to_str().unwrap();
        assert_eq!(steps[0], [path, "/reset"]);
        assert_eq!(steps[1], [path, "/inheritance:r", "/grant:r", &format!("*{sid}:F")]);
    }

    /// The file's DACL as SDDL, from `icacls /save` (UTF-16; it names accounts by SID, unlike
    /// the localised listing `icacls <file>` prints).
    #[cfg(windows)]
    fn dacl_of(p: &Path, dir: &Path) -> String {
        let saved = dir.join("acl.txt");
        let icacls = PathBuf::from(std::env::var_os("SystemRoot").unwrap()).join(r"System32\icacls.exe");
        assert!(std::process::Command::new(icacls).arg(p).arg("/save").arg(&saved).output().unwrap().status.success());
        let bytes = std::fs::read(&saved).unwrap();
        let units: Vec<u16> = bytes.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        let text = String::from_utf16_lossy(&units);
        let sddl = text.lines().find(|l| l.contains("D:")).unwrap();
        sddl[sddl.find("D:").unwrap()..].trim().to_string()
    }

    #[cfg(windows)]
    #[test]
    fn the_key_file_is_private_on_windows() {
        let dir = temp_dir("acl");
        let p = dir.join("control-key.test");
        assert_eq!(write_key_file(&p, KEY, true).unwrap(), None);
        assert_eq!(std::fs::read_to_string(&p).unwrap(), KEY);
        let out = run_system_tool("whoami.exe", &["/user", "/fo", "csv", "/nh"].map(std::ffi::OsString::from), true).unwrap();
        let out = String::from_utf8_lossy(&out);
        let me = parse_whoami_sid(&out).unwrap();
        // Protected (nothing inherited) with one entry: full access for the current user only.
        // Not SYSTEM, Administrators or Users (an administrator can still take ownership).
        let dacl = dacl_of(&p, &dir);
        assert!(dacl.starts_with("D:P"), "{dacl}");
        assert_eq!(dacl.matches('(').count(), 1, "{dacl}");
        assert!(dacl.ends_with(&format!("(A;;FA;;;{me})")), "{dacl}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn settings_dir_is_the_apps_folder() {
        if let Some(d) = settings_dir() {
            let name = d.file_name().and_then(|n| n.to_str()).unwrap_or("");
            assert!(name == "wordcraft" || name == "WordCraft", "{}", d.display());
        }
    }
}
