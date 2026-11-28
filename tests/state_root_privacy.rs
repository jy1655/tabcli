mod support;

use serde_json::Value;
use std::{
    path::Path,
    process::{Command, Output},
};

fn run(root: &Path, args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_tabcli"));
    command
        .args(args)
        .env("AGENT_BRIDGE_NATIVE_STATE_DIR", root);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            command.pre_exec(|| {
                libc::umask(0o022);
                Ok(())
            });
        }
    }
    command.output().unwrap()
}

fn successful(output: Output) -> Value {
    assert!(output.status.success(), "{output:?}");
    serde_json::from_slice(&output.stdout).unwrap()
}

fn diagnosis(root: &Path, args: &[&str]) -> Value {
    let value = successful(run(root, args));
    value["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["id"] == "state_root")
        .cloned()
        .expect("doctor must report state_root")
}

#[test]
fn doctor_reports_a_missing_root_without_creating_it() {
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("missing");
    for args in [
        &["doctor", "--provider", "pi", "--json"][..],
        &["doctor", "session-missing", "--json"],
    ] {
        let row = diagnosis(&root, args);
        assert_eq!(row["reason_code"], "state_root_absent");
        assert_eq!(row["next_action"], "TAB creates it at the first session.");
        assert!(!root.exists());
    }
}

#[test]
fn queries_preserve_root_permissions_and_session_records() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let session = root.join("session-query");
    std::fs::create_dir_all(session.join("events")).unwrap();
    support::write(
        &session.join("manifest.json"),
        &support::manifest(
            "session-query",
            "pi",
            Path::new("pi"),
            "1.0",
            root,
            "root privacy query",
        ),
    );
    support::write(
        &session.join("status.json"),
        &support::status("exited", 2, None),
    );
    successful(run(root, &["sessions", "--json"]));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o750)).unwrap();
    }
    #[cfg(target_os = "macos")]
    assert!(
        Command::new("/bin/chmod")
            .args(["+a", "everyone allow add_file,delete_child"])
            .arg(root)
            .status()
            .unwrap()
            .success()
    );
    #[cfg(windows)]
    assert!(
        Command::new("icacls")
            .arg(root)
            .args(["/grant", "*S-1-1-0:(R)"])
            .status()
            .unwrap()
            .success()
    );
    let permissions = || {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let meta = std::fs::metadata(root).unwrap();
            #[cfg(target_os = "macos")]
            let acl = {
                let out = Command::new("/bin/ls")
                    .arg("-den")
                    .arg(root)
                    .output()
                    .unwrap();
                assert!(out.status.success());
                out.stdout
            };
            #[cfg(not(target_os = "macos"))]
            let acl = Vec::<u8>::new();
            (meta.uid(), meta.mode(), acl)
        }
        #[cfg(windows)]
        {
            let out = Command::new("powershell")
                .args([
                    "-NoProfile",
                    "-NonInteractive",
                    "-Command",
                    "(Get-Acl -LiteralPath $env:TABCLI_ROOT_FIXTURE -ErrorAction Stop).Sddl",
                ])
                .env("TABCLI_ROOT_FIXTURE", root)
                .output()
                .unwrap();
            assert!(out.status.success() && !out.stdout.is_empty(), "{out:?}");
            out.stdout
        }
    };
    let before = permissions();
    let records = support::entries(root);
    for args in [
        &["sessions", "--json"][..],
        &["status", "--json"],
        &["inspect", "session-query", "--json"],
        &["result", "session-query", "--list", "--json"],
        &["doctor", "session-query", "--json"],
    ] {
        successful(run(root, args));
        assert_eq!(permissions(), before, "{args:?}");
        assert_eq!(support::entries(root), records, "{args:?}");
    }
}

#[cfg(unix)]
mod unix {
    use super::*;
    use std::fs;
    use std::os::unix::fs::{PermissionsExt, symlink};

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o7777
    }

    #[test]
    fn standalone_consent_prepares_the_root_before_creating_its_records() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("state");
        for action in ["revoke", "reset"] {
            successful(run(
                &root,
                &["consent", action, tmp.path().to_str().unwrap(), "--json"],
            ));
            assert_eq!(mode(&root), 0o700);
            assert!(!root.join("state-root.durable").exists());
            fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    #[test]
    fn doctor_observes_a_linked_root_and_settings_repairs_it() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("link");
        let dir = tmp.path().join("directory");
        fs::create_dir(&dir).unwrap();
        symlink(&dir, &root).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o750)).unwrap();
        let row = diagnosis(&root, &["doctor", "--provider", "pi", "--json"]);
        assert_eq!(row["reason_code"], "state_root_not_private");
        assert_eq!(
            row["next_action"],
            "Session creation repairs what it can. Otherwise, follow the remedy in the detail."
        );
        assert_eq!(mode(&dir), 0o750);
        successful(run(&root, &["settings", "macos-open-mode", "tab-first"]));
        assert_eq!(mode(&dir), 0o700);
        let row = diagnosis(&root, &["doctor", "--provider", "pi", "--json"]);
        assert_eq!(row["reason_code"], "state_root_private");
        assert_eq!(row["next_action"], "No action is needed.");
    }

    #[test]
    fn warning_writers_continue_on_an_unreadable_linked_root() {
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("target");
        let root = temp.path().join("root");
        fs::create_dir(&target).unwrap();
        symlink(&target, &root).unwrap();
        successful(run(&root, &["settings", "macos-open-mode", "tab-first"]));
        fs::set_permissions(&target, fs::Permissions::from_mode(0o300)).unwrap();
        for action in ["revoke", "reset"] {
            let output = run(
                &root,
                &["consent", action, temp.path().to_str().unwrap(), "--json"],
            );
            let warning = String::from_utf8(output.stderr.clone()).unwrap();
            let record = successful(output);
            assert_eq!(record["revoked"], action == "revoke");
            assert!(
                warning.starts_with(&format!("warning: state root {}: ", root.display())),
                "{warning}"
            );
            assert_eq!(mode(&target), 0o300);
        }
        let output = run(&root, &["settings", "macos-open-mode", "tab-first"]);
        let warning = String::from_utf8(output.stderr).unwrap();
        // Syncing a settings write may itself need read access to the root.
        assert!(
            warning.starts_with(&format!("warning: state root {}: ", root.display())),
            "{warning}"
        );
        assert_eq!(mode(&target), 0o300);
        fs::set_permissions(&target, fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[test]
    fn unsearchable_parent_reports_where_inspection_failed() {
        if unsafe { libc::geteuid() } == 0 {
            eprintln!("requires a caller subject to directory search permissions");
            return;
        }
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("state");
        fs::create_dir(&root).unwrap();
        fs::set_permissions(parent.path(), fs::Permissions::from_mode(0o600)).unwrap();
        let row = diagnosis(&root, &["doctor", "--provider", "pi", "--json"]);
        let output = run(&root, &["settings", "macos-open-mode", "tab-first"]);
        fs::set_permissions(parent.path(), fs::Permissions::from_mode(0o700)).unwrap();
        assert!(!output.status.success());
        let stderr = String::from_utf8(output.stderr).unwrap();
        let inspection = format!("failed to inspect state directory {}", root.display());
        assert!(stderr.contains(&inspection), "{stderr}");
        assert_eq!(row["reason_code"], "state_root_unreadable");
        assert_eq!(
            row["next_action"],
            "Check access to the directory and its parents, then rerun doctor."
        );
        assert!(fs::read_dir(&root).unwrap().next().is_none());
    }

    #[test]
    fn isolated_self_test_only_prepares_a_parent_it_created() {
        let temp = tempfile::tempdir().unwrap();
        let parent = temp.path().join("container");
        for expected in [0o700, 0o750] {
            use std::os::unix::process::CommandExt;
            let mut command = Command::new(env!("CARGO_BIN_EXE_tabcli"));
            unsafe {
                command.pre_exec(|| {
                    libc::umask(0o022);
                    Ok(())
                });
            }
            let out = command
                .args(["self-test", "pi", "--isolated", "--json"])
                .current_dir(temp.path())
                .env("AGENT_BRIDGE_NATIVE_STATE_DIR", &parent)
                .env("HOME", temp.path())
                .env("PATH", "")
                .env_remove("HC_TERMINAL_ID")
                .output()
                .unwrap();
            let report: Value =
                serde_json::from_slice(&out.stdout).unwrap_or_else(|_| panic!("{out:?}"));
            let child = Path::new(report["state_root"].as_str().unwrap());
            assert_eq!(child.parent(), Some(parent.as_path()));
            assert_eq!(mode(child), 0o700);
            assert_eq!(mode(&parent), expected);
            fs::set_permissions(&parent, fs::Permissions::from_mode(0o750)).unwrap();
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_named_user_acl_is_masked_after_preparation() {
        let tmp = tempfile::tempdir().unwrap();
        // Linux's posix_acl_xattr uapi, including a named user and an rwx mask.
        let uid = unsafe { libc::geteuid() }.wrapping_add(1);
        let mut acl = 2u32.to_le_bytes().to_vec();
        for (tag, perm, id) in [
            (1u16, 7u16, u32::MAX),
            (2, 7, uid),
            (4, 0, u32::MAX),
            (16, 7, u32::MAX),
            (32, 0, u32::MAX),
        ] {
            acl.extend(tag.to_le_bytes());
            acl.extend(perm.to_le_bytes());
            acl.extend(id.to_le_bytes());
        }
        let dir = fs::File::open(tmp.path()).unwrap();
        use std::os::fd::AsRawFd;
        let key = c"system.posix_acl_access";
        let result = unsafe {
            libc::fsetxattr(
                dir.as_raw_fd(),
                key.as_ptr(),
                acl.as_ptr().cast(),
                acl.len(),
                0,
            )
        };
        if result != 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EOPNOTSUPP) {
                eprintln!(
                    "skipping POSIX ACL readback: temporary directory does not support ACLs ({error})"
                );
                return;
            }
            panic!("setting fixture POSIX access ACL: {error}");
        }
        assert_eq!(mode(tmp.path()), 0o770);
        successful(run(
            tmp.path(),
            &["settings", "macos-open-mode", "tab-first"],
        ));
        let mut readback = vec![0; acl.len()];
        assert_eq!(
            unsafe {
                libc::fgetxattr(
                    dir.as_raw_fd(),
                    key.as_ptr(),
                    readback.as_mut_ptr().cast(),
                    readback.len(),
                )
            },
            acl.len() as isize
        );
        acl[30..32].copy_from_slice(&0u16.to_le_bytes());
        assert_eq!(readback, acl);
        assert_eq!(mode(tmp.path()), 0o700);
    }

    #[cfg(target_os = "macos")]
    fn has_acl(path: &Path) -> bool {
        use std::{ffi::c_void, os::fd::AsRawFd};
        unsafe extern "C" {
            fn acl_get_fd_np(fd: i32, kind: i32) -> *mut c_void;
            fn acl_free(acl: *mut c_void) -> i32;
        }
        let directory = fs::File::open(path).unwrap();
        let acl = unsafe { acl_get_fd_np(directory.as_raw_fd(), 0x100) };
        if acl.is_null() {
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::ENOENT)
            );
            return false;
        }
        assert_eq!(unsafe { acl_free(acl) }, 0);
        true
    }

    #[cfg(target_os = "macos")]
    fn chmod(path: &Path, entry: &str) {
        let output = Command::new("/bin/chmod")
            .args(["+a", entry])
            .arg(path)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_directory_list_denial_reports_acl_remedy() {
        if unsafe { libc::geteuid() } == 0 {
            eprintln!("requires a caller subject to directory access checks");
            return;
        }
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        chmod(root.path(), "everyone deny list");
        let row = diagnosis(root.path(), &["doctor", "--provider", "pi", "--json"]);
        let output = run(root.path(), &["settings", "macos-open-mode", "tab-first"]);
        assert!(
            Command::new("/bin/chmod")
                .arg("-N")
                .arg(root.path())
                .status()
                .unwrap()
                .success()
        );
        let remedy = format!("run chmod -N '{}' and retry", root.path().display());
        assert_eq!(row["reason_code"], "state_root_unreadable");
        assert!(row["detail"].as_str().unwrap().contains(&remedy), "{row}");
        assert!(String::from_utf8(output.stderr).unwrap().contains(&remedy));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_writers_require_a_tab_entry_before_removing_a_deny_acl() {
        let root = tempfile::tempdir().unwrap();
        chmod(root.path(), "everyone deny delete");
        // Darwin's well-known GUID for the everyone principal.
        let refusal = "ACL entry 0 (deny, principal abcdefab-cdef-abcd-efab-cdef0000000c); the directory holds no TAB entry (state-root.durable, settings.json, workspace-consent/ or a session-* directory); set AGENT_BRIDGE_NATIVE_STATE_DIR to a directory meant for TAB";
        let workspace = root.path().to_str().unwrap();
        for args in [
            &["settings", "macos-open-mode", "tab-first"][..],
            &["consent", "revoke", workspace, "--json"],
            &["consent", "reset", workspace, "--json"],
        ] {
            let output = run(root.path(), args);
            assert!(!output.status.success(), "{args:?}: {output:?}");
            let stderr = String::from_utf8(output.stderr).unwrap();
            assert_eq!(
                stderr.lines().take(4).collect::<Vec<_>>().join("\n"),
                format!("Error: state root {workspace}\n\nCaused by:\n    {refusal}"),
                "{args:?}: {stderr}"
            );
            assert!(has_acl(root.path()));
            assert!(fs::read_dir(root.path()).unwrap().next().is_none());
        }
        fs::create_dir(root.path().join("workspace-consent")).unwrap();
        assert!(has_acl(root.path()));
        assert_eq!(
            diagnosis(root.path(), &["doctor", "--provider", "pi", "--json"])["reason_code"],
            "state_root_not_private"
        );
        successful(run(
            root.path(),
            &["settings", "macos-open-mode", "tab-first"],
        ));
        assert!(!has_acl(root.path()));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_clears_an_inherited_acl_before_consent_and_repairs_a_read_denial() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("state");
        chmod(parent.path(), "everyone allow add_file,directory_inherit");
        successful(run(
            &root,
            &[
                "consent",
                "reset",
                parent.path().to_str().unwrap(),
                "--json",
            ],
        ));
        assert!(!has_acl(&root));
        chmod(&root, "everyone deny readsecurity");
        successful(run(
            &root,
            &[
                "consent",
                "revoke",
                parent.path().to_str().unwrap(),
                "--json",
            ],
        ));
        assert!(!has_acl(&root));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_existing_tab_root_loses_its_extended_acl() {
        let tmp = tempfile::tempdir().unwrap();
        successful(run(
            tmp.path(),
            &["settings", "macos-open-mode", "tab-first"],
        ));
        let perms = Command::new("/bin/chmod")
            .args(["+a", "everyone allow add_file,delete_child"])
            .arg(tmp.path())
            .output()
            .unwrap();
        assert!(perms.status.success(), "{perms:?}");
        let listing = || {
            Command::new("/bin/ls")
                .arg("-den")
                .arg(tmp.path())
                .output()
                .unwrap()
                .stdout
        };
        let before = listing();
        assert!(has_acl(tmp.path()));
        assert!(
            before
                .split(|b| *b == b'\n')
                .any(|line| line.starts_with(b" 0:"))
        );
        assert_eq!(
            diagnosis(tmp.path(), &["doctor", "--provider", "pi", "--json"])["reason_code"],
            "state_root_not_private"
        );
        assert_eq!(listing(), before);
        successful(run(
            tmp.path(),
            &["settings", "macos-open-mode", "tab-first"],
        ));
        assert!(!has_acl(tmp.path()));
        assert!(
            !listing()
                .split(|b| *b == b'\n')
                .any(|line| line.starts_with(b" 0:"))
        );
    }
}
