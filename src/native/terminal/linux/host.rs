use super::super::bound_host::{BoundHostSpec, prepare_bound_launch};
use super::super::{TerminalKind, TerminalSession};
use crate::native::clock::SystemClock;
use crate::native::session::Store;
use anyhow::Result;
use std::{ffi::OsStr, os::unix::process::CommandExt, path::Path};

/// What bash runs. A non-interactive bash runs the file named by `BASH_ENV` before its
/// command, which `--noprofile --norc` does not stop; the launch must not run it, but the
/// provider keeps the variable, so the script exports it again. A value that is not UTF-8
/// cannot be quoted exactly and is dropped.
#[derive(Debug, Eq, PartialEq)]
enum BashScript {
    Plain(String),
    WithoutStartupFile(String),
}

fn bash_script(command: &str, bash_env: Option<&OsStr>) -> BashScript {
    match bash_env {
        None => BashScript::Plain(command.to_owned()),
        Some(value) => BashScript::WithoutStartupFile(match value.to_str() {
            Some(_) => format!(
                "export BASH_ENV={}\n{command}",
                crate::native::terminal::shell_quote(value)
            ),
            None => command.to_owned(),
        }),
    }
}

/// Wait for the exact surface and pending launch, flush startup input, then become
/// the bash that runs the wrapper with job control on the foreground terminal.
pub(in crate::native::terminal) fn run(
    directory: &Path,
    command: &str,
    kind: TerminalKind,
    validate_binding: impl FnOnce(&TerminalSession) -> Result<()>,
) -> Result<()> {
    let binding = kind.registration().binding();
    let name = kind.as_str();
    let prepared = prepare_bound_launch(
        directory,
        &BoundHostSpec { name: binding.host },
        binding,
        &mut SystemClock,
        validate_binding,
    );
    if let Err(error) = prepared {
        // The pane ends with this process and takes the message with it.
        crate::native::session::launch::log(
            &Store::open_unchecked(directory),
            &format!("{name}_host_refused: {error:#}"),
        );
        return Err(error);
    }
    // The surface may inherit provider caller markers from its server.
    let mut bash = crate::native::process_env::helper_command("/bin/bash");
    bash.arg0("bash");
    let script = match bash_script(command, std::env::var_os("BASH_ENV").as_deref()) {
        BashScript::Plain(script) => script,
        BashScript::WithoutStartupFile(script) => {
            bash.env_remove("BASH_ENV");
            script
        }
    };
    let error = bash
        .args(["--noprofile", "--norc", "-m", "-c", &script])
        .exec();
    let error =
        anyhow::Error::new(error).context(format!("could not start bash for the {name} launch"));
    crate::native::session::launch::log(
        &Store::open_unchecked(directory),
        &format!("{name}_host_refused: {error:#}"),
    );
    Err(error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    #[test]
    fn the_launch_bash_runs_no_startup_file_but_keeps_bash_env_for_the_provider() {
        let root = tempfile::tempdir().unwrap();
        let startup = root.path().join("startup.sh");
        let ran = root.path().join("ran");
        let seen = root.path().join("seen");
        std::fs::write(&startup, format!("touch '{}'\n", ran.display())).unwrap();
        let command = format!("printf '%s' \"$BASH_ENV\" > '{}'", seen.display());
        let script = bash_script(&command, Some(startup.as_os_str()));
        let BashScript::WithoutStartupFile(script) = script else {
            panic!("{script:?}")
        };
        let status = Command::new("/bin/bash")
            .env("BASH_ENV", &startup)
            .env_remove("BASH_ENV")
            .args(["--noprofile", "--norc", "-m", "-c", &script])
            .status()
            .unwrap();
        assert!(status.success());
        assert!(!ran.exists(), "BASH_ENV ran before the launch command");
        assert_eq!(
            std::fs::read_to_string(&seen).unwrap(),
            startup.to_str().unwrap()
        );
        assert_eq!(bash_script("cmd", None), BashScript::Plain("cmd".into()));
        use std::os::unix::ffi::OsStrExt;
        assert_eq!(
            bash_script("cmd", Some(OsStr::from_bytes(b"/x\xff"))),
            BashScript::WithoutStartupFile("cmd".into())
        );
    }
}
