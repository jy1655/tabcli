//! Build-time CLI contract. No state, provider, or environment observation.
use super::{COMMANDS, NativeCommand, args, envelope, failure_kind::FailureKind};
use anyhow::{Result, bail};
use serde::Serialize;
use serde_json::{Value, json};

#[derive(Debug)]
pub(crate) struct Request {
    command: Option<String>,
    json: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Effect {
    ReadsRecords,
    WritesRecords,
    RepairsRecords,
    DeletesRecords,
    ReadsProviderState,
    LaunchesProvider,
    SendsInput,
    RequestsInterruption,
    ClosesSurface,
    ModelCalls,
    ProviderProbes,
}

#[derive(Serialize)]
pub(super) struct Contract {
    summary: &'static str,
    synopsis: &'static [&'static str],
    possible_effects: &'static [Effect],
    examples: &'static [&'static [&'static str]],
    features: &'static [&'static str],
    notes: &'static [&'static str],
}

use Effect::*;

pub(super) const DESCRIBE: Contract = Contract {
    summary: "Describe this binary's public command contract without inspecting the machine.",
    synopsis: &["describe [COMMAND] [--json]"],
    possible_effects: &[],
    examples: &[&["describe", "--json"], &["describe", "wait", "--json"]],
    features: &["command_contract"],
    notes: &["Build support is not runtime availability or permission to execute a command."],
};
pub(super) const ASK: Contract = Contract {
    summary: "Start a new visible provider session and submit a prompt.",
    synopsis: &[
        "ask <codex|claude|agy|pi> [--workspace PATH] (--prompt TEXT | --prompt-file PATH) [--title NAME]\n      [--model MODEL] [--effort EFFORT] [--terminal <ghostty|iterm2|terminal|warp|wezterm|windows-console>]\n      [--yolo] [--timeout-secs N] [--detach] [--json] [--context-result <session>/<request-id>]... [--context-mode <full|compact>]",
    ],
    possible_effects: &[
        ReadsRecords,
        WritesRecords,
        ReadsProviderState,
        LaunchesProvider,
        SendsInput,
        ModelCalls,
        RepairsRecords,
        ClosesSurface,
    ],
    examples: &[&[
        "ask",
        "codex",
        "--prompt",
        "Explain the project entry point.",
        "--detach",
        "--json",
    ]],
    features: &["context_result", "context_mode"],
    notes: &[
        "Can repair a dead owner's records and close its remaining Windows console; recovery may also finish an interrupted close.",
        "Requires an installed, authenticated provider and supported terminal. Retain session and request_id even on errors.",
        "--detach returns after delivery processing, not task completion. --yolo applies only when explicitly requested.",
    ],
};
pub(super) const SELF_TEST: Contract = Contract {
    summary: "Exercise a real provider round trip and close the session it creates.",
    synopsis: &[
        "self-test <codex|claude|agy|pi> [--workspace PATH]\n      [--terminal <ghostty|iterm2|terminal|warp|wezterm|windows-console>] [--model MODEL] [--effort EFFORT]\n      [--yolo] [--timeout-secs N] [--isolated] [--json]",
    ],
    possible_effects: &[
        ReadsRecords,
        WritesRecords,
        ReadsProviderState,
        LaunchesProvider,
        SendsInput,
        ClosesSurface,
        ModelCalls,
        RepairsRecords,
    ],
    examples: &[&["self-test", "codex", "--json"]],
    features: &["isolated"],
    notes: &[
        "Can repair a dead owner's records and close its remaining Windows console; recovery may also finish an interrupted close.",
        "Makes real model calls. Default timeout is 120 seconds per command; retained records and isolated directories are not deleted.",
    ],
};
pub(super) const TELL: Contract = Contract {
    summary: "Submit a new request to an existing ready session.",
    synopsis: &[
        "tell <session> (--prompt TEXT | --prompt-file PATH) [--timeout-secs N] [--detach] [--json]\n      [--context-result <session>/<request-id>]... [--context-mode <full|compact>]",
    ],
    possible_effects: &[
        ReadsRecords,
        WritesRecords,
        ReadsProviderState,
        LaunchesProvider,
        SendsInput,
        ModelCalls,
        RepairsRecords,
        ClosesSurface,
    ],
    examples: &[&[
        "tell",
        "session-example",
        "--prompt",
        "Review the previous result.",
        "--detach",
        "--json",
    ]],
    features: &["context_result", "context_mode"],
    notes: &[
        "Can repair a dead owner's records and close its remaining Windows console; recovery may also finish an interrupted close.",
        "Requires ready, unheld state and the provider's delivery gates. A pending request cannot receive tell.",
        "Creates a new request; it does not answer or resolve an earlier question emission. Never resend an uncertain delivery.",
    ],
};
pub(super) const HOLD: Contract = Contract {
    summary: "Block or release future Bridge follow-up requests for a session.",
    synopsis: &["hold <session> [--release] [--json]"],
    possible_effects: &[ReadsRecords, WritesRecords],
    examples: &[
        &["hold", "session-example", "--json"],
        &["hold", "session-example", "--release", "--json"],
    ],
    features: &[],
    notes: &["Does not interrupt a running provider turn or prevent direct terminal input."],
};
pub(super) const CANCEL: Contract = Contract {
    summary: "Request interruption of a supported pending request.",
    synopsis: &["cancel <session> [--json]"],
    possible_effects: &[ReadsRecords, WritesRecords, RequestsInterruption],
    examples: &[&["cancel", "session-example", "--json"]],
    features: &[],
    notes: &[
        "Only Pi sessions with verified extension readiness support cancellation. Acceptance is not completion; read the exact request result.",
    ],
};
pub(super) const CONSENT: Contract = Contract {
    summary: "Inspect or invalidate Bridge's workspace consent evidence.",
    synopsis: &["consent <inspect|revoke|reset> PATH [--json]"],
    possible_effects: &[ReadsRecords, WritesRecords],
    examples: &[&["consent", "inspect", ".", "--json"]],
    features: &[],
    notes: &[
        "inspect reads; revoke/reset change Bridge records, not provider trust stores. This is not a provider permission mode.",
        "Always prints JSON on success, without the common schema_version/ok envelope. Errors are reported on stderr without a JSON error object.",
    ],
};
pub(super) const SETTINGS: Contract = Contract {
    summary: "Read settings or change a named setting for future operations.",
    synopsis: &[
        "settings [windows-tab-window <dedicated|current>] [--json]",
        "settings macos-open-mode <tab-first|new-window> [--json]",
        "settings codex-memory-workspace <refuse|allow> [--json]",
        "settings codex-lifecycle-hooks <on|off> [--json]",
    ],
    possible_effects: &[ReadsRecords, WritesRecords],
    examples: &[&["settings", "--json"]],
    features: &["codex_lifecycle_hooks"],
    notes: &[
        "No setting arguments reads current settings; a name and value changes them. Success output is JSON without the common schema_version/ok envelope; errors are reported on stderr without a JSON error object.",
    ],
};
pub(super) const REOPEN: Contract = Contract {
    summary: "Resume a closed provider conversation in a new Bridge session.",
    synopsis: &[
        "reopen <closed-session> (--prompt TEXT | --prompt-file PATH) [--title NAME]\n      [--model MODEL] [--effort EFFORT] [--terminal <windows-console>] [--yolo] [--timeout-secs N]\n      [--detach] [--json]",
    ],
    possible_effects: &[
        ReadsRecords,
        WritesRecords,
        ReadsProviderState,
        LaunchesProvider,
        SendsInput,
        ModelCalls,
        RepairsRecords,
        ClosesSurface,
    ],
    examples: &[&[
        "reopen",
        "session-example",
        "--prompt",
        "Continue the recorded task.",
        "--detach",
        "--json",
    ]],
    features: &[],
    notes: &[
        "Can repair a dead owner's records and close its remaining Windows console; recovery may also finish an interrupted close.",
        "Only Claude on native Windows is supported. Closed source, readable completed history and provider ownership checks are required; no exclusive provider hold is promised.",
    ],
};
pub(super) const INSPECT: Contract = Contract {
    summary: "Read a session and optionally its recorded timeline.",
    synopsis: &["inspect <session> [--timeline [--request REQUEST]] [--json]"],
    possible_effects: &[ReadsRecords],
    examples: &[&["inspect", "session-example", "--json"]],
    features: &[
        "timeline",
        "cancel_admission_observation",
        "tell_admission_observation",
        "question_capability_observation",
    ],
    notes: &[
        "Observes records without repair or publication. --request filters a timeline and requires --timeline.",
        "JSON capabilities.cancel reports what cancel's admission would decide at this snapshot; cancel rechecks it, and an admissible cancel records intent, not interruption or completion.",
        "JSON capabilities.tell evaluates tell's Bridge record gates and lists in unevaluated the gates it does not evaluate, including Codex record gates that tell decides at delivery; admissible is an observation, not a reservation. capabilities.questions reports configured emission observation and no answer path.",
    ],
};
pub(super) const RESULT: Contract = Contract {
    summary: "Read recorded request results and any observed question emissions.",
    synopsis: &[
        "result <session> [--latest | --list | --event EVENT | --request REQUEST] [--json]",
        "result <session> --request REQUEST --wait [--timeout-secs N] [--json]",
    ],
    possible_effects: &[ReadsRecords],
    examples: &[&[
        "result",
        "session-example",
        "--request",
        "request-1",
        "--json",
    ]],
    features: &["question_emissions", "request_wait"],
    notes: &[
        "Use the exact request_id, not --latest, when correlating a delegated task. --timeout-secs requires --wait and --wait requires --request.",
        "Question emissions do not prove shown, pending, executed or answered. Absent fields mean unknown observation, not no questions.",
    ],
};
pub(super) const WAIT: Contract = Contract {
    summary: "Observe exact request addresses until one ends or emits a requested question.",
    synopsis: &[
        "wait ADDRESS [ADDRESS ...] [--timeout-secs N] [--json]\n      [--questions [--after-question SESSION/REQUEST/EMISSION_ID]...]",
    ],
    possible_effects: &[ReadsRecords],
    examples: &[&[
        "wait",
        "session-example/request-1",
        "--questions",
        "--timeout-secs",
        "60",
        "--json",
    ]],
    features: &["questions", "after_question"],
    notes: &[
        "ADDRESS is SESSION/REQUEST. --after-question requires --questions and a cursor belonging to one of the waited addresses.",
        "ended:null with question is not completion: retain remaining and use the returned after_question cursor. timed_out:true never cancels or retires a request.",
    ],
};
pub(super) const SEARCH: Contract = Contract {
    summary: "Search retained session records and results.",
    synopsis: &[
        "search <query> [--workspace PATH | --all-workspaces] [--provider <codex|claude|agy|pi>]\n      [--limit N] [--json]",
    ],
    possible_effects: &[ReadsRecords],
    examples: &[&["search", "entry point", "--json"]],
    features: &[],
    notes: &["Does not search arbitrary provider histories or attach sessions."],
};
pub(super) const DOCTOR: Contract = Contract {
    summary: "Diagnose a session or provider, with optional bounded CLI probes.",
    synopsis: &[
        "doctor <session> [--probe] [--json]",
        "doctor --provider <codex|claude|agy|pi> [--probe] [--json]",
    ],
    possible_effects: &[ReadsRecords, ReadsProviderState, ProviderProbes],
    examples: &[&["doctor", "session-example", "--json"]],
    features: &["probe"],
    notes: &[
        "Without --probe, reads existing evidence. --probe runs provider processes; neither form repairs sessions or sends a task.",
    ],
};
pub(super) const SESSIONS: Contract = Contract {
    summary: "List managed sessions, repairing records of verified dead owners when needed.",
    synopsis: &[
        "sessions [--workspace PATH] [--provider <codex|claude|agy|pi>] [--state STATE]\n      [--sort <id|updated>] [--json]",
    ],
    possible_effects: &[ReadsRecords, WritesRecords, RepairsRecords, ClosesSurface],
    examples: &[&["sessions", "--json"]],
    features: &[],
    notes: &[
        "Can repair a dead owner's records and close its remaining Windows console; recovery may also finish an interrupted close.",
        "Can write recovery records. Use status for read-only observation.",
    ],
};
pub(super) const STATUS: Contract = Contract {
    summary: "Read a compact view of managed sessions without repair.",
    synopsis: &[
        "status [--workspace PATH | --all-workspaces] [--provider <codex|claude|agy|pi>] [--all] [--json]",
    ],
    possible_effects: &[ReadsRecords],
    examples: &[&["status", "--json"]],
    features: &[],
    notes: &[
        "Observation is not a promise that a subsequent operation will pass its runtime gates.",
    ],
};
pub(super) const PRUNE: Contract = Contract {
    summary: "Delete sufficiently old closed-session records after ownership checks.",
    synopsis: &["prune-sessions --closed-before-days N --explicit [--json]"],
    possible_effects: &[ReadsRecords, DeletesRecords],
    examples: &[&[
        "prune-sessions",
        "--closed-before-days",
        "30",
        "--explicit",
        "--json",
    ]],
    features: &[],
    notes: &["Destructive: requires --explicit and a positive age. Does not close live sessions."],
};
pub(super) const CLOSE: Contract = Contract {
    summary: "Close a managed session's recorded terminal surface.",
    synopsis: &[
        "close-session <session> --explicit [--with-started-from] [--timeout-secs N] [--json]",
    ],
    possible_effects: &[ReadsRecords, WritesRecords, ClosesSurface],
    examples: &[&["close-session", "session-example", "--explicit", "--json"]],
    features: &["with_started_from"],
    notes: &[
        "Requires explicit authority over the target. --with-started-from includes one level of recorded children within the same deadline. Records are retained.",
    ],
};

fn public_command(name: &str) -> Option<&'static super::CommandSpec> {
    COMMANDS
        .iter()
        .find(|entry| entry.name == name && !entry.hidden)
}

pub(super) fn parse(arguments: &[String]) -> Result<NativeCommand> {
    let parsed = (|| {
        let mut command = None;
        let mut json = false;
        for argument in arguments {
            match argument.as_str() {
                "--json" => args::set_flag_once(&mut json, "--json")?,
                name if !name.starts_with('-') => {
                    if public_command(name).is_none() {
                        bail!("unknown public command");
                    }
                    args::set_once(&mut command, name.to_owned(), "command")?;
                }
                _ => bail!("describe accepts one public command and --json"),
            }
        }
        Ok(NativeCommand::Describe(Request { command, json }))
    })();
    if let Err(error) = &parsed {
        envelope::parse_failure(arguments, error, serde_json::Map::new())?;
    }
    parsed
}

fn value(request: &Request) -> Value {
    let commands: Vec<_> = COMMANDS
        .iter()
        .filter(|entry| !entry.hidden)
        .filter(|entry| {
            request
                .command
                .as_deref()
                .is_none_or(|name| name == entry.name)
        })
        .map(|entry| {
            let mut contract =
                serde_json::to_value(entry.contract.expect("public command contract")).unwrap();
            contract["name"] = json!(entry.name);
            contract
        })
        .collect();
    envelope::ok(serde_json::Map::from_iter([
        ("contract_version".into(), json!(1)),
        ("binary_version".into(), json!(env!("CARGO_PKG_VERSION"))),
        ("scope".into(), json!("build")),
        (
            "invocation".into(),
            json!({"executable": agent_bridge::PUBLIC_COMMAND, "format": "argv", "examples_include_executable": false}),
        ),
        ("commands".into(), json!(commands)),
        (
            "output".into(),
            json!({
                "schema_version": envelope::SCHEMA_VERSION,
                "success_defaults": envelope::ok(serde_json::Map::new()),
                "error_example": envelope::error(&anyhow::anyhow!("example error"), serde_json::Map::new()),
                "failure_kinds": FailureKind::ALL.map(FailureKind::as_str),
            "notes": ["Common defaults are not universal: consent and settings use legacy JSON without this envelope, and report errors only on stderr. Other command fields may override defaults. Consult result fields; exit status alone does not identify request completion.", "failure_kind is optional, diagnostic only, and never authorizes retry. Absence is unknown, not proof of non-delivery."]
            }),
        ),
        (
            "notes".into(),
            json!([
                "This describes the build, not installed providers, platform availability, permissions or session readiness.",
                "possible_effects is the union across command forms, not authorization. Runtime observation: status, inspect and doctor; every action rechecks its gates.",
                "Synopsis and argv examples describe invocation; this is not a complete input or result JSON schema. For structured errors, pass --json."
            ]),
        ),
    ]))
}

pub(super) fn run(request: Request) -> Result<()> {
    let value = value(&request);
    envelope::emit(request.json, &value, || {
        println!(
            "Build command contract for {} {}",
            agent_bridge::PUBLIC_COMMAND,
            env!("CARGO_PKG_VERSION")
        );
        println!("Use --json for metadata; runtime availability requires session/provider checks.");
        for command in value["commands"].as_array().unwrap() {
            println!(
                "\n{}: {}",
                command["name"].as_str().unwrap(),
                command["summary"].as_str().unwrap()
            );
            for synopsis in command["synopsis"].as_array().unwrap() {
                println!(
                    "  {} {}",
                    agent_bridge::PUBLIC_COMMAND,
                    synopsis.as_str().unwrap()
                );
            }
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contracts_cover_only_public_registry_and_examples_parse() {
        let help = crate::help_text();
        for entry in COMMANDS {
            assert_eq!(entry.contract.is_none(), entry.hidden, "{}", entry.name);
            if let Some(contract) = entry.contract {
                for synopsis in contract.synopsis {
                    assert!(
                        help.contains(&format!("  {} {synopsis}", agent_bridge::PUBLIC_COMMAND)),
                        "missing synopsis for {}",
                        entry.name
                    );
                }
                assert!(!contract.examples.is_empty());
                for example in contract.examples {
                    assert_eq!(example[0], entry.name);
                    super::super::parse_args(example.iter())
                        .unwrap_or_else(|error| panic!("{example:?}: {error:#}"));
                }
            }
        }
    }

    #[test]
    fn effects_distinguish_observation_from_repair_and_execution() {
        for contract in [&INSPECT, &RESULT, &WAIT, &SEARCH, &STATUS] {
            assert_eq!(contract.possible_effects, &[ReadsRecords]);
        }
        assert!(DESCRIBE.possible_effects.is_empty());
        assert!(SESSIONS.possible_effects.contains(&RepairsRecords));
        assert!(SESSIONS.possible_effects.contains(&WritesRecords));
        for contract in [&SESSIONS, &ASK, &TELL, &REOPEN, &SELF_TEST] {
            assert!(
                contract.possible_effects.contains(&RepairsRecords),
                "missing repair effect: {}",
                contract.summary
            );
            assert!(
                contract.possible_effects.contains(&ClosesSurface),
                "missing surface close effect: {}",
                contract.summary
            );
        }
        assert!(SELF_TEST.possible_effects.contains(&ModelCalls));
        assert!(CLOSE.possible_effects.contains(&ClosesSurface));
        assert!(CANCEL.possible_effects.contains(&RequestsInterruption));
        assert!(PRUNE.possible_effects.contains(&DeletesRecords));
    }
}
