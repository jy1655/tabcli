use super::parse as parse_console_control;
use super::*;
use crate::native::parse_args;
#[cfg(windows)]
use crate::native::session::test_support::{TERMINAL_CLOSING_FILE, TERMINAL_HANDLE_FILE};
#[cfg(windows)]
use std::{fs, time::Instant};

#[cfg(windows)]
struct InProcessHelper {
    directory: PathBuf,
    phase: usize,
    change: &'static str,
    screen: String,
    calls: usize,
    refused: bool,
    written: Vec<windows_sys::Win32::System::Console::INPUT_RECORD>,
}

#[cfg(windows)]
thread_local! {
    static IN_PROCESS_HELPER: std::cell::RefCell<Option<InProcessHelper>> = const { std::cell::RefCell::new(None) };
}

#[cfg(windows)]
pub(in crate::native) fn with_in_process_helper(
    directory: &Path,
    session: &terminal::TerminalSession,
    provider: agent_bridge::FirstPartyCli,
    phase: usize,
    change: &'static str,
    screen: &str,
    run: impl FnOnce(),
) {
    use crate::native::session::{CoreRecord, Store};
    let store = Store::open_unchecked(directory);
    if !store.record(CoreRecord::Manifest).path().exists() {
        store
            .record(CoreRecord::Manifest)
            .write_json(&crate::native::session::schema::SessionManifest {
                schema: 1,
                id: session.managed_session_id.clone().unwrap(),
                provider: provider.as_str().to_owned(),
                provider_path: "test-provider".into(),
                provider_version: "test".into(),
                workspace: directory.into(),
                title: "console helper fixture".into(),
                model: None,
                effort: None,
                yolo: false,
                created_unix_ms: 1,
            })
            .unwrap();
    }
    store.write_terminal(session).unwrap();
    let (_, helper) = with_helper(
        InProcessHelper {
            directory: directory.into(),
            phase,
            change,
            screen: screen.into(),
            calls: 0,
            refused: false,
            written: Vec::new(),
        },
        run,
    );
    assert_eq!(helper.calls, phase);
    assert!(helper.refused);
}

#[cfg(windows)]
fn with_helper<T>(helper: InProcessHelper, run: impl FnOnce() -> T) -> (T, InProcessHelper) {
    IN_PROCESS_HELPER.with(|slot| {
        assert!(slot.borrow().is_none());
        *slot.borrow_mut() = Some(helper);
    });
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            IN_PROCESS_HELPER.with(|slot| *slot.borrow_mut() = None);
        }
    }
    let _reset = Reset;
    let result = run();
    let helper = IN_PROCESS_HELPER.with(|slot| slot.borrow_mut().take().unwrap());
    (result, helper)
}

#[cfg(windows)]
pub(super) fn helper_output(
    command: &mut std::process::Command,
    deadline: Instant,
    label: &str,
) -> Option<
    std::result::Result<std::process::Output, crate::native::process_env::CommandOutputFailure>,
> {
    use super::client::HelperProcess;
    IN_PROCESS_HELPER.with(|slot| {
        slot.borrow_mut()
            .as_mut()
            .map(|helper| helper.output(command, deadline, label))
    })
}

#[cfg(windows)]
impl client::HelperProcess for InProcessHelper {
    fn output(
        &mut self,
        command: &mut std::process::Command,
        _deadline: Instant,
        _label: &str,
    ) -> std::result::Result<std::process::Output, crate::native::process_env::CommandOutputFailure>
    {
        use std::os::windows::process::ExitStatusExt;
        let mut stdout = Vec::new();
        let result = (|| -> Result<()> {
            let args = command
                .get_args()
                .map(|value| {
                    value
                        .to_str()
                        .context("helper argument is not UTF-8")
                        .map(str::to_owned)
                })
                .collect::<Result<Vec<_>>>()?;
            let [command, args @ ..] = args.as_slice() else {
                bail!("missing helper command");
            };
            assert_eq!(command, "native-console-control");
            let (id, request) = decode_arguments(args)?;
            let expected_directory = self.directory.clone();
            run_with(
                request,
                &id,
                |directory, id, session| {
                    assert_eq!(directory, expected_directory);
                    session.verify_managed_session(id)
                },
                |action, _, records| self.execute(action, records, &mut stdout),
            )
        })();
        Ok(std::process::Output {
            status: std::process::ExitStatus::from_raw(u32::from(result.is_err())),
            stdout,
            stderr: result
                .err()
                .map(|error| format!("Error: {error}").into_bytes())
                .unwrap_or_default(),
        })
    }
}

#[cfg(windows)]
impl InProcessHelper {
    fn execute(
        &mut self,
        action: ResolvedAction,
        records: &terminal::WindowsSessionRecords<'_>,
        stdout: &mut Vec<u8>,
    ) -> Result<()> {
        use crate::native::session::{SessionState, Store, turn, update_status};
        use terminal::windows::{AttachedInput, dispatch_attached_input, read_send_payload};
        if matches!(action, ResolvedAction::Screen) {
            stdout.extend_from_slice(screen_output(&self.screen)?.as_bytes());
            return Ok(());
        }
        self.calls += 1;
        let stale = self.calls == self.phase;
        let (bytes, action) = match action {
            ResolvedAction::Dialog { input } => {
                (RecordReader::at(&input).raw_bytes()?, AttachedInput::Dialog)
            }
            ResolvedAction::Send {
                input,
                submit_count,
                newline,
                timeout,
            } => {
                let (input, deadline) = read_send_payload(&input, submit_count, timeout)?;
                assert_eq!(submit_count, 0, "this fixture expects guarded text input");
                (
                    input.into_bytes(),
                    AttachedInput::Text { newline, deadline },
                )
            }
            _ => bail!("the race fixture expects console input"),
        };
        let token = turn::current_claim_token(&Reader::open_unchecked(&self.directory))?.unwrap();
        if stale {
            let store = Store::open_unchecked(&self.directory);
            let _lock = store.lock()?;
            match self.change {
                "retired" => fs::remove_file(self.directory.join("turn.claim"))?,
                "replaced" => {
                    fs::remove_file(self.directory.join("turn.claim"))?;
                    store.private("turn.claim").write_private(b"1-2-3")?;
                }
                "closed" => update_status(&self.directory, SessionState::Closed, None, None)?,
                "expired" => {}
                _ => unreachable!(),
            }
        }
        let bytes = match action {
            AttachedInput::Dialog => {
                let mut input = decode_dialog_payload(&bytes)?;
                assert_eq!(input.expected_claim.as_deref(), Some(token.as_str()));
                assert!(input.expires_unix_ms.is_some());
                if stale && self.change == "expired" {
                    input.expires_unix_ms = Some(0);
                }
                serde_json::to_vec(&input)?
            }
            AttachedInput::Text { .. } => {
                let mut input = decode_text_payload(&bytes)?;
                assert_eq!(input.expected_claim.as_deref(), Some(token.as_str()));
                assert!(input.expires_unix_ms.is_some());
                if stale && self.change == "expired" {
                    input.expires_unix_ms = Some(0);
                }
                serde_json::to_vec(&input)?
            }
        };
        let now = Instant::now();
        let mut count = 0;
        let mut written = Vec::new();
        let result = dispatch_attached_input(
            records.directory,
            &bytes,
            action,
            || Ok(self.screen.clone()),
            |chunk| {
                assert!(
                    Store::open_unchecked(records.directory)
                        .try_lock()?
                        .is_none()
                );
                count += chunk.len();
                written.extend_from_slice(chunk);
                Ok(chunk.len() as u32)
            },
            || {
                assert!(
                    Store::open_unchecked(records.directory)
                        .try_lock()?
                        .is_some()
                );
                Ok(0)
            },
            stdout,
            &mut crate::native::clock::FakeClock::new(now),
        );
        if stale {
            assert_eq!(count, 0, "stale helper must write zero input records");
            self.refused = true;
        } else {
            assert!(count > 0);
        }
        self.written.extend(written);
        result
    }
}

#[test]
fn console_control_parser_preserves_positional_action_contract() {
    let timeout = Duration::from_millis(7);
    for (tail, expected) in [
        (vec!["screen", "session-route"], Operation::Screen),
        (vec!["close", "session-route"], Operation::Close),
        (
            vec!["window", "session-route", "7"],
            Operation::Window { timeout },
        ),
        (
            vec!["send", "session-route", "pending-prompt-route.txt", "7"],
            Operation::Send {
                mode: SendMode::ProviderDefault,
                input_name: "pending-prompt-route.txt".into(),
                timeout,
            },
        ),
        (
            vec![
                "send",
                "session-route",
                "pending-prompt-route.txt",
                "7",
                "0",
            ],
            Operation::Send {
                mode: SendMode::TextOnly,
                input_name: "pending-prompt-route.txt".into(),
                timeout,
            },
        ),
        (
            vec!["dialog", "session-route", "pending-prompt-route.txt", "7"],
            Operation::Dialog {
                input_name: "pending-prompt-route.txt".into(),
                timeout,
            },
        ),
    ] {
        let NativeCommand::ConsoleControl { request, id } =
            parse_console_control(&tail.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>())
                .unwrap()
        else {
            panic!("wrong command")
        };
        assert_eq!(request.operation, expected);
        assert_eq!(id, "session-route");
    }
    for tail in [
        vec!["screen", "session-route", "7"],
        vec!["close", "session-route", "7"],
        vec!["window", "session-route"],
        vec!["window", "session-route", "7", "extra"],
        vec!["send", "session-route"],
        vec!["send", "session-route", "pending-prompt-route.txt"],
        vec![
            "send",
            "session-route",
            "pending-prompt-route.txt",
            "7",
            "extra",
        ],
        vec!["dialog", "session-route"],
        vec!["dialog", "session-route", "pending-prompt-route.txt"],
        vec![
            "dialog",
            "session-route",
            "pending-prompt-route.txt",
            "7",
            "extra",
        ],
    ] {
        let error =
            parse_console_control(&tail.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>())
                .unwrap_err();
        assert_eq!(
            error.to_string(),
            "invalid native console control arguments",
            "{tail:?}"
        );
    }
    for action in ["send", "dialog"] {
        for input in [
            "outside.txt".to_owned(),
            "../pending-prompt-route.txt".into(),
            "pending-prompt-route.json".into(),
            format!("pending-prompt-{}.txt", "x".repeat(110)),
            format!("pending-prompt-{}.txt", "가".repeat(37)),
        ] {
            let error = parse_args([
                "native-console-control",
                action,
                "session-route",
                &input,
                "invalid",
            ])
            .unwrap_err();
            assert_eq!(
                error.to_string(),
                "invalid native console control arguments"
            );
        }
        let longest = format!("pending-prompt-{}.txt", "x".repeat(109));
        assert!(
            parse_args([
                "native-console-control",
                action,
                "session-route",
                &longest,
                "18446744073709551615"
            ])
            .is_ok()
        );
    }
    for action in ["send", "dialog", "window"] {
        for (timeout, expected) in [
            ("0", "native console timeout must be positive"),
            (
                "bad",
                "invalid native console timeout: invalid digit found in string",
            ),
            (
                "18446744073709551616",
                "invalid native console timeout: number too large to fit in target type",
            ),
        ] {
            let mut args = vec!["native-console-control", action, "session-route"];
            if action != "window" {
                args.push("pending-prompt-route.txt");
            }
            args.push(timeout);
            assert_eq!(format!("{:#}", parse_args(args).unwrap_err()), expected);
        }
    }
}

#[cfg(windows)]
#[test]
fn arguments_round_trip_every_operation_and_send_mode() {
    for args in [
        vec!["screen", "session-route"],
        vec!["close", "session-route"],
        vec!["window", "session-route", "7"],
        vec!["dialog", "session-route", "pending-prompt-route.txt", "7"],
        vec!["send", "session-route", "pending-prompt-route.txt", "7"],
        vec![
            "send",
            "session-route",
            "pending-prompt-route.txt",
            "7",
            "0",
        ],
        vec![
            "send",
            "session-route",
            "pending-prompt-route.txt",
            "7",
            "3",
        ],
    ] {
        let args = args.into_iter().map(str::to_owned).collect::<Vec<_>>();
        let (id, request) = decode_arguments(&args).unwrap();
        let encoded = encode_arguments(&id, &request)
            .into_iter()
            .map(|arg| arg.into_string().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(encoded, args);
        assert_eq!(decode_arguments(&encoded).unwrap(), (id, request));
    }
}

#[cfg(windows)]
#[test]
fn payload_and_reply_codecs_preserve_the_existing_contract() {
    let input = decode_text_payload(br#"{"text":"a\n\ud83d\ude00","extra":true}"#).unwrap();
    assert_eq!(input.text, "a\n😀");
    assert_eq!(input.expected_claim, None);
    assert_eq!(input.expires_unix_ms, None);
    assert!(
        format!("{:#}", decode_text_payload(b"{}").err().unwrap())
            .starts_with("invalid text-only console payload: missing field `text`")
    );
    assert!(decode_dialog_payload(br#"{"screen":"dialog","key":"Enter"}"#).is_ok());
    assert!(decode_dialog_payload(br#"{"screen":"dialog","key":"down_enter"}"#).is_err());
    for (reply, bytes, expected) in [
        (DialogReply::Sent, b"sent\n".as_slice(), Some(true)),
        (DialogReply::Changed, b"changed\n".as_slice(), Some(false)),
        (DialogReply::Stale, b"stale\n".as_slice(), None),
    ] {
        let mut output = Vec::new();
        write_dialog_reply(&mut output, reply).unwrap();
        assert_eq!(output, bytes);
        match expected {
            Some(expected) => assert_eq!(decode_dialog_reply(&output).unwrap(), expected),
            None => assert!(
                decode_dialog_reply(&output)
                    .unwrap_err()
                    .downcast_ref::<StaleDialogClaim>()
                    .is_some()
            ),
        }
    }
    for bytes in [b"sent\nchanged".as_slice(), b"SENT", b"\xff"] {
        assert_eq!(
            decode_dialog_reply(bytes).unwrap_err().to_string(),
            "unexpected managed console dialog result"
        );
    }
    assert!(decode_dialog_reply(b" \r\nsent\r\n").unwrap());
    let mut output = Vec::new();
    output.extend_from_slice(screen_output("한😀\n\"screen\"").unwrap().as_bytes());
    assert_eq!(
        serde_json::from_slice::<String>(&output).unwrap(),
        "한😀\n\"screen\""
    );
    output.clear();
    output.extend_from_slice(window_output(-7).as_bytes());
    assert_eq!(output, b"-7\n");
    assert_eq!(decode_window(&output), Some(-7));
    for bytes in [b"0\n".as_slice(), b"1\n2", b"\xff", b""] {
        assert_eq!(decode_window(bytes), None);
    }
}

#[cfg(windows)]
#[test]
fn process_start_evidence_and_prewrite_refusals_classify_delivery() {
    use crate::native::process_env::CommandOutputFailure;
    struct FailedProcess {
        started: bool,
    }
    impl client::HelperProcess for FailedProcess {
        fn output(
            &mut self,
            _: &mut std::process::Command,
            _: Instant,
            label: &str,
        ) -> std::result::Result<std::process::Output, CommandOutputFailure> {
            let error = anyhow::anyhow!("{label} timed out");
            Err(if self.started {
                CommandOutputFailure::started(error)
            } else {
                CommandOutputFailure::not_started(error)
            })
        }
    }
    let mut command = std::process::Command::new("unused-by-adapter");
    for started in [false, true] {
        let failure = client::run_console_send_with(
            &mut FailedProcess { started },
            &mut command,
            Instant::now(),
        )
        .unwrap_err();
        assert_eq!(failure.delivery_may_have_occurred(), started);
        assert_eq!(
            failure.error().to_string(),
            "Windows console control helper timed out"
        );
    }
    assert!(helper_reports_missing(
        " Error: console process is no longer available\n"
    ));
    assert!(!helper_reports_missing(
        "Error: console process is no longer available: Access is denied. (os error 5)"
    ));
    assert!(!helper_proves_no_input(
        "Windows console claim stale after input dispatch"
    ));
}

#[cfg(windows)]
fn isolated_root(test: &str, child: &str) -> Option<PathBuf> {
    if std::env::var_os(child).is_some() {
        return Some(
            std::env::var_os(crate::native::session::primitives::STATE_DIR_ENV)
                .unwrap()
                .into(),
        );
    }
    let root = tempfile::tempdir().unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--test-threads=1", "--nocapture"])
        .env(child, "1")
        .env(
            crate::native::session::primitives::STATE_DIR_ENV,
            root.path(),
        )
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
    None
}

#[cfg(windows)]
fn session_fixture(root: &Path, id: &str, provider: &str) -> (PathBuf, terminal::TerminalSession) {
    use crate::native::session::{CoreRecord, Store};
    let directory = root.join(id);
    fs::create_dir(&directory).unwrap();
    let session: terminal::TerminalSession = serde_json::from_value(serde_json::json!({
        "terminal":"windows-console", "session_id":"1", "managed_session_id": id,
    }))
    .unwrap();
    let store = Store::open_unchecked(&directory);
    store
        .record(CoreRecord::Manifest)
        .write_json(&crate::native::session::schema::SessionManifest {
            schema: 1,
            id: id.into(),
            provider: provider.into(),
            provider_path: "test-provider".into(),
            provider_version: "test".into(),
            workspace: directory.clone(),
            title: "console fixture".into(),
            model: None,
            effort: None,
            yolo: false,
            created_unix_ms: 1,
        })
        .unwrap();
    store.write_terminal(&session).unwrap();
    (directory, session)
}

#[cfg(windows)]
#[test]
fn provider_facts_resolve_from_the_manifest_after_surface_verification() {
    let Some(root) = isolated_root(
        "native::console_control::tests::provider_facts_resolve_from_the_manifest_after_surface_verification",
        "AB_CONSOLE_PROTOCOL_PROVIDER_FACTS",
    ) else {
        return;
    };
    for (provider, count, newline) in [
        ("codex", 2, terminal::ConsoleNewline::ShiftReturn),
        ("agy", 1, terminal::ConsoleNewline::LineFeed),
        ("pi", 1, terminal::ConsoleNewline::LineFeed),
    ] {
        let id = format!("session-{provider}");
        let (directory, _) = session_fixture(&root, &id, provider);
        for override_count in [None, Some(0), Some(3)] {
            let mut args = vec![
                "send".to_owned(),
                id.clone(),
                "pending-prompt-test.txt".to_owned(),
                "5000".to_owned(),
            ];
            if let Some(count) = override_count {
                args.push(count.to_string());
            }
            let (id, request) = decode_arguments(&args).unwrap();
            let calls = std::cell::RefCell::new(Vec::new());
            run_with(
                request,
                &id,
                |path, id, session| {
                    assert_eq!(path, directory);
                    session.verify_managed_session(id)?;
                    calls.borrow_mut().push("owner");
                    Ok(())
                },
                |action, _, records| {
                    calls.borrow_mut().push("execute");
                    assert_eq!(records.directory, directory);
                    let ResolvedAction::Send {
                        input,
                        submit_count,
                        newline: actual,
                        timeout,
                    } = action
                    else {
                        panic!("wrong action");
                    };
                    assert_eq!(submit_count, override_count.unwrap_or(count));
                    assert_eq!(actual, newline);
                    assert_eq!(timeout, Duration::from_secs(5));
                    assert_eq!(input, directory.join("pending-prompt-test.txt"));
                    assert!(!input.exists(), "resolution must not prefetch the payload");
                    Ok(())
                },
            )
            .unwrap();
            assert_eq!(*calls.borrow(), ["owner", "execute"]);
        }
    }
    let (directory, session) = session_fixture(&root, "session-invalid", "unsupported-provider");
    let args = [
        "send",
        "session-invalid",
        "pending-prompt-missing.txt",
        "5000",
    ]
    .map(str::to_owned);
    let (id, request) = decode_arguments(&args).unwrap();
    let error = run_with(
        request,
        &id,
        |_, _, _| bail!("owner refused"),
        |_, _, _| panic!("executed before owner verification"),
    )
    .unwrap_err();
    assert_eq!(error.to_string(), "owner refused");
    let store = crate::native::session::Store::open_unchecked(&directory);
    let mut foreign = session;
    foreign.managed_session_id = Some("session-foreign".into());
    store.write_terminal(&foreign).unwrap();
    for action in ["close", "window"] {
        let mut args = vec![action.to_owned(), id.clone()];
        if action == "window" {
            args.push("600".into());
        }
        let (id, request) = decode_arguments(&args).unwrap();
        let error = run_with(
            request,
            &id,
            |_, _, _| panic!("bound-surface operation asked for an owner"),
            |_, _, _| panic!("unbound surface executed"),
        )
        .unwrap_err();
        assert!(error.to_string().contains("managed session"), "{error:#}");
        assert!(
            !error.to_string().contains("provider"),
            "provider parsed before session verification: {error:#}"
        );
    }
}

#[cfg(windows)]
#[test]
fn in_process_delivery_uses_the_actual_provider_newline_and_payload_reply_codecs() {
    use crate::native::session::{SessionState, Store, turn, update_status};
    use windows_sys::Win32::System::Console::SHIFT_PRESSED;
    let Some(root) = isolated_root(
        "native::console_control::tests::in_process_delivery_uses_the_actual_provider_newline_and_payload_reply_codecs",
        "AB_CONSOLE_PROTOCOL_IN_PROCESS",
    ) else {
        return;
    };
    for provider in ["codex", "agy", "pi"] {
        let (directory, session) = session_fixture(&root, &format!("session-{provider}"), provider);
        fs::create_dir(directory.join("events")).unwrap();
        update_status(&directory, SessionState::Working, None, None).unwrap();
        let claim = turn::claim(&Store::open_unchecked(&directory), &[]).unwrap();
        let prompt = directory.join("pending-prompt-source.txt");
        Store::open_unchecked(&directory)
            .private("pending-prompt-source.txt")
            .write_private("a\n한😀\n".as_bytes())
            .unwrap();
        let (_, helper) = with_helper(
            InProcessHelper {
                directory: directory.clone(),
                phase: usize::MAX,
                change: "",
                screen: "draft".into(),
                calls: 0,
                refused: false,
                written: Vec::new(),
            },
            || {
                let count = terminal::send_file_text_only(
                    &session,
                    &prompt,
                    claim.token(),
                    Instant::now() + Duration::from_secs(5),
                )
                .unwrap();
                assert_eq!(count, 5);
            },
        );
        assert_eq!(helper.calls, 1);
        assert!(!helper.refused);
        let keys = helper
            .written
            .iter()
            .map(|record| unsafe { record.Event.KeyEvent })
            .collect::<Vec<_>>();
        assert_eq!(
            keys.iter()
                .filter(|key| key.wVirtualKeyCode == 0x0d && key.dwControlKeyState == SHIFT_PRESSED)
                .count(),
            if provider == "codex" { 4 } else { 0 }
        );
        assert!(
            !keys
                .iter()
                .any(|key| key.wVirtualKeyCode == 0x0d && key.dwControlKeyState == 0)
        );
        assert_eq!(
            fs::read_dir(&directory)
                .unwrap()
                .filter_map(|entry| {
                    let name = entry.unwrap().file_name();
                    let name = name.to_str().unwrap();
                    (name.starts_with("pending-prompt-") && name != "pending-prompt-source.txt")
                        .then_some(())
                })
                .count(),
            0,
            "the client must retire its private payload after the reply"
        );
        claim.retain();
    }
}

#[cfg(windows)]
#[test]
fn close_deadline_console_helper_is_bounded() {
    let powershell = terminal::windows_powershell_executable().unwrap();
    let mut command = std::process::Command::new(&powershell);
    command.args([
        "-NoProfile",
        "-NonInteractive",
        "-Command",
        "Start-Sleep -Seconds 30",
    ]);
    let started = Instant::now();
    let error =
        client::run_console_helper_command(&mut command, started + Duration::from_millis(100))
            .unwrap_err();
    assert!(format!("{error:#}").contains("timed out"), "{error:#}");
    // The timeout error alone does not prove the bound: a helper that failed to end the
    // command would report it too, after waiting for the 30 s sleep. 25 s proves that the
    // helper did not wait for the sleep, whether it ended the running command or timed out
    // before PowerShell had started, and leaves a loaded runner room to start and end
    // PowerShell; a 5 s bound let a slow runner decide the result, as the 10 s deadline
    // below did (#180).
    let elapsed = started.elapsed();
    assert!(elapsed < Duration::from_secs(25), "{elapsed:?}");
    let mut command = std::process::Command::new(&powershell);
    command.args([
        "-NoProfile",
        "-NonInteractive",
        "-Command",
        "[Console]::Error.WriteLine('own close timeout'); exit 1",
    ]);
    // This case checks that the command's own message survives a failure before the
    // deadline, so the deadline must not expire first. PowerShell did not print within
    // 10 s on a loaded `windows-latest` runner (#180). The command exits at once, so the
    // test never waits this long.
    let error =
        client::run_console_helper_command(&mut command, Instant::now() + Duration::from_secs(120))
            .unwrap_err();
    assert_eq!(error.to_string(), "own close timeout");
}

#[cfg(windows)]
#[test]
fn guarded_dialog_file_stays_private_in_the_supplied_helper_directory() {
    use crate::native::{
        consent,
        terminal::{self, DialogKey, GuardedDialogInput, TerminalSession},
    };
    use std::{path::PathBuf, process::Command};
    const CHILD: &str = "AB_WINDOWS_DIALOG_DIRECTORY_TEST";
    const TEST: &str = "native::console_control::tests::guarded_dialog_file_stays_private_in_the_supplied_helper_directory";
    if std::env::var_os(CHILD).is_none() {
        let root = tempfile::tempdir().unwrap();
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", TEST, "--test-threads=1", "--nocapture"])
            .env(CHILD, "1")
            .env("AGENT_BRIDGE_NATIVE_STATE_DIR", root.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let root = PathBuf::from(std::env::var_os("AGENT_BRIDGE_NATIVE_STATE_DIR").unwrap());
    let directory = root.join("session-dialog");
    fs::create_dir(&directory).unwrap();
    let handle: TerminalSession = serde_json::from_value(serde_json::json!({
        "terminal":"windows-console", "session_id":"1", "managed_session_id":"session-dialog"
    }))
    .unwrap();
    let input = GuardedDialogInput {
        expected_claim: None,
        expires_unix_ms: None,
        screen: "exact dialog".into(),
        key: DialogKey::Enter,
    };
    let file = client::dialog_input_file(&directory, &handle, &input).unwrap();
    assert_eq!(file.path().parent(), Some(directory.as_path()));
    let bytes = consent::read_store(file.path()).unwrap().unwrap();
    assert_eq!(bytes, serde_json::to_string(&input).unwrap());
    drop(file);
    assert_eq!(fs::read_dir(&directory).unwrap().count(), 0);
    let other_root = tempfile::tempdir().unwrap();
    let other = other_root.path().join("session-dialog");
    fs::create_dir(&other).unwrap();
    let error = terminal::submit_if_unchanged(
        &other,
        &handle,
        &input.screen,
        None,
        Instant::now() + Duration::from_secs(2),
    )
    .unwrap_err();
    assert!(error.to_string().contains("helper state root"), "{error:#}");
    for id in [None, Some("session-other".into())] {
        let mut wrong = handle.clone();
        wrong.managed_session_id = id;
        let error = terminal::guarded_dialog_input(
            &directory,
            &wrong,
            &input,
            Instant::now() + Duration::from_secs(2),
        )
        .unwrap_err();
        assert!(error.to_string().contains("managed session"), "{error:#}");
    }
    assert_eq!(
        terminal::submit_if_unchanged(&directory, &handle, &input.screen, None, Instant::now())
            .unwrap_err()
            .to_string(),
        "managed console dialog input timed out before it started"
    );
    assert_eq!(fs::read_dir(&directory).unwrap().count(), 0);
    assert_eq!(fs::read_dir(&other).unwrap().count(), 0);
}

#[cfg(windows)]
#[test]
fn text_only_launcher_rejects_malformed_success_as_uncertain() {
    assert_eq!(
        super::parse_text_only_output(b"{\"schema\":1,\"written_chars\":3}\n").unwrap(),
        3
    );
    for output in [
        "",
        "{}",
        "{\"schema\":1,\"written_chars\":3,\"unknown\":true}",
        "{\"schema\":1,\"schema\":1,\"written_chars\":3}",
        "{\"schema\":1,\"written_chars\":184467440737095516160}",
        "{\"schema\":2,\"written_chars\":3}",
        "{\"schema\":1,\"written_chars\":1.5}",
        "{\"schema\":1,\"written_chars\":-1}",
        "{\"schema\":1,\"written_chars\":3} garbage",
        "{\"schema\":1,\"written_chars\":3}\n{}",
    ] {
        let failure = super::parse_text_only_output(output.as_bytes()).unwrap_err();
        assert!(failure.delivery_may_have_occurred());
    }
    assert!(
        super::parse_text_only_output(b"\xff")
            .unwrap_err()
            .delivery_may_have_occurred()
    );
    assert_eq!(
        super::parse_text_only_output(b" \n {\"schema\":1,\"written_chars\":3}\n ").unwrap(),
        3
    );
}

#[cfg(windows)]
#[test]
fn text_only_launcher_classifies_start_and_exit_failures() {
    let root = tempfile::tempdir().unwrap();
    let mut missing = std::process::Command::new(root.path().join("missing.exe"));
    let deadline = Instant::now() + Duration::from_secs(120);
    assert!(
        !client::run_console_send_command(&mut missing, deadline)
            .unwrap_err()
            .delivery_may_have_occurred()
    );
    let mut failed =
        crate::native::process_env::helper_command(std::env::var_os("ComSpec").unwrap());
    failed.args(["/d", "/c", "exit 1"]);
    assert!(
        client::run_console_send_command(&mut failed, deadline)
            .unwrap_err()
            .delivery_may_have_occurred()
    );
}

#[cfg(windows)]
#[test]
fn console_protocol_failure_and_dialog_replies_keep_exact_boundaries() {
    use std::os::windows::process::ExitStatusExt;
    for message in [
        "Windows console claim stale before any input",
        "Windows console submission delays do not fit inside the remaining turn timeout",
    ] {
        for prefix in ["", "Error: "] {
            let output = std::process::Output {
                status: std::process::ExitStatus::from_raw(1),
                stdout: vec![],
                stderr: format!(" \n{prefix}{message}\r\n").into_bytes(),
            };
            let failure = super::classify_send_output(output).unwrap_err();
            assert!(!failure.delivery_may_have_occurred());
            assert_eq!(failure.error().to_string(), format!("{prefix}{message}"));
        }
        assert!(!super::helper_proves_no_input(&format!("{message}: extra")));
    }
    for stderr in [b"".as_slice(), b"\n \r\n", b"\xff"] {
        let output = std::process::Output {
            status: std::process::ExitStatus::from_raw(1),
            stdout: b"not a reply".to_vec(),
            stderr: stderr.to_vec(),
        };
        let failure = super::classify_send_output(output).unwrap_err();
        assert!(failure.delivery_may_have_occurred());
        assert_eq!(
            failure.error().to_string(),
            if stderr == b"\xff" {
                "\u{fffd}"
            } else {
                "Windows console control helper failed"
            }
        );
    }
}

#[cfg(windows)]
#[test]
fn console_helpers_explicitly_forward_only_the_selected_state_root() {
    use crate::native::session::primitives::STATE_DIR_ENV;
    const CHILD: &str = "ISSUE111_HELPER_TEST_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let test = "native::console_control::tests::console_helpers_explicitly_forward_only_the_selected_state_root";
        for root in [Some("C:\\selected-private-root"), None] {
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command
                .args(["--exact", test, "--nocapture", "--test-threads=1"])
                .env(CHILD, "1");
            for name in crate::native::process_env::non_provider_removals() {
                command.env(name, "caller-sentinel");
            }
            if let Some(root) = root {
                command.env(STATE_DIR_ENV, root);
            } else {
                command.env_remove(STATE_DIR_ENV);
            }
            let output = crate::native::process_env::command_output_until(
                &mut command,
                Instant::now() + Duration::from_secs(30),
                "helper environment test",
            )
            .unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
        }
        return;
    }
    let session = terminal::TerminalSession {
        managed_session_id: Some("session-environment".into()),
        kind: terminal::TerminalKind::WindowsConsole,
        id: "1".into(),
        tab_id: None,
        window_id: None,
        windows_process_identity: None,
        wezterm_mux: None,
    };
    let root = std::env::var_os(STATE_DIR_ENV);
    for operation in [
        Operation::Window {
            timeout: Duration::from_millis(600),
        },
        Operation::Screen,
        Operation::Dialog {
            input_name: "pending-prompt-test.txt".into(),
            timeout: Duration::from_secs(1),
        },
        Operation::Send {
            input_name: "pending-prompt-test.txt".into(),
            timeout: Duration::from_secs(1),
            mode: SendMode::ProviderDefault,
        },
        Operation::Close,
    ] {
        let command = client::request_command(&session, operation).unwrap();
        crate::native::process_env::tests::assert_helper_environment(
            &command,
            &[(STATE_DIR_ENV, root.as_deref())],
        );
    }
}

#[cfg(windows)]
#[test]
fn windows_console_helper_accepts_only_managed_session_identity() {
    assert!(matches!(
        parse_args([
            "native-console-control",
            "send",
            "session-owner123",
            "pending-prompt-1.txt",
            "5000",
        ])
        .unwrap(),
        NativeCommand::ConsoleControl {
            request: ConsoleControlRequest { operation: Operation::Send { input_name, timeout, mode: SendMode::ProviderDefault } },
            id,
        } if timeout == Duration::from_millis(5000) && id == "session-owner123" && input_name == "pending-prompt-1.txt"
    ));
    assert!(parse_args(["native-console-control", "close", "1234"]).is_err());
    assert!(
        parse_args([
            "native-console-control",
            "send",
            "session-owner123",
            "..\\outside.txt",
            "5000",
        ])
        .is_err()
    );
    assert!(matches!(
        parse_args(["native-console-control", "window", "session-owner123", "600"]).unwrap(),
        NativeCommand::ConsoleControl {
            request: ConsoleControlRequest { operation: Operation::Window { timeout } },
            id,
        } if timeout == Duration::from_millis(600) && id == "session-owner123"
    ));
    for invalid in [
        &["window", "session-owner123"][..],
        &["window", "session-owner123", "0"],
        &["window", "session-owner123", "pending-prompt-1.txt", "600"],
    ] {
        let mut arguments = vec!["native-console-control"];
        arguments.extend_from_slice(invalid);
        assert!(parse_args(arguments).is_err(), "{invalid:?}");
    }
}

#[cfg(windows)]
#[test]
fn console_control_requests_preserve_surface_verification_and_handle_selection() {
    let directory = tempfile::tempdir().unwrap();
    let closing = directory.path().join(TERMINAL_CLOSING_FILE);
    fs::create_dir(&closing).unwrap();
    assert_eq!(
        terminal::windows_console_handle_path(directory.path(), true),
        directory.path().join(TERMINAL_HANDLE_FILE)
    );
    fs::remove_dir(&closing).unwrap();
    fs::write(&closing, b"closing").unwrap();
    for (request, bound_only) in [
        (Operation::Screen, false),
        (
            Operation::Send {
                mode: SendMode::ProviderDefault,
                input_name: "pending-prompt-route.txt".into(),
                timeout: Duration::from_millis(7),
            },
            false,
        ),
        (
            Operation::Dialog {
                input_name: "pending-prompt-route.txt".into(),
                timeout: Duration::from_millis(7),
            },
            false,
        ),
        (
            Operation::Window {
                timeout: Duration::from_millis(7),
            },
            true,
        ),
        (Operation::Close, true),
    ] {
        let request = ConsoleControlRequest { operation: request };
        assert_eq!(request.needs_only_bound_surface(), bound_only);
        let expected = if request.is_close() {
            TERMINAL_CLOSING_FILE
        } else {
            TERMINAL_HANDLE_FILE
        };
        assert_eq!(
            terminal::windows_console_handle_path(directory.path(), request.is_close()),
            directory.path().join(expected)
        );
    }
}
