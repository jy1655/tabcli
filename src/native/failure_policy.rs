//! Public failure-output choices belong to the command registry. Runtime producers retain
//! session/request fields, observation and failure timing; this module does no record I/O.
use super::{COMMANDS, envelope};
use anyhow::Result;
use serde_json::{Map, Value, json};

#[derive(Clone, Copy)]
pub(super) enum Fields {
    Empty,
    Sessions,
    Wait,
    Search,
}

#[derive(Clone, Copy)]
pub(super) enum RuntimeFailure {
    Envelope,
    // Addressed lifecycle output stays with TurnReport, including its late-bound fields.
    TurnReport,
    // The number preserves the established prose order, independently of dispatch order.
    Stderr(usize),
}

#[derive(Clone, Copy)]
pub(super) struct Policy {
    // None means stderr only. The number preserves the established describe-note order.
    pub(super) parse_order: Option<usize>,
    pub(super) runtime: RuntimeFailure,
    pub(super) fields: Fields,
}

impl Policy {
    fn fields(self, query: Option<&String>) -> Map<String, Value> {
        match self.fields {
            Fields::Empty => Map::new(),
            Fields::Sessions => json!({"sessions": []}).as_object().unwrap().clone(),
            Fields::Wait => json!({"ended": null, "remaining": [], "timed_out": false})
                .as_object()
                .unwrap()
                .clone(),
            Fields::Search => json!({"query": query, "hits": []})
                .as_object()
                .unwrap()
                .clone(),
        }
    }

    pub(super) fn emit_parse_failure(self, args: &[String], error: &anyhow::Error) -> Result<()> {
        if self.parse_order.is_none() {
            return Ok(());
        }
        // Search's first argument is a query even when its exact text is "--json".
        let options = match self.fields {
            Fields::Search => args.get(1..).unwrap_or_default(),
            _ => args,
        };
        let query = args.first().filter(|query| !query.trim().is_empty());
        envelope::parse_failure(options, error, self.fields(query))
    }
}

/// Runtime data is supplied by the existing owner, and overrides the registry's defaults.
/// This does not move errors across a runtime producer's observation or publication phase.
pub(super) fn runtime_error(
    command: &str,
    error: &anyhow::Error,
    fields: Map<String, Value>,
) -> Value {
    let policy = COMMANDS
        .iter()
        .find(|entry| entry.name == command)
        .and_then(|entry| entry.failure)
        .expect("public failure policy");
    assert!(matches!(policy.runtime, RuntimeFailure::Envelope));
    let mut defaults = policy.fields(None);
    defaults.extend(fields);
    envelope::error(error, defaults)
}

pub(super) fn output_note() -> String {
    let mut parse = Vec::new();
    let mut stderr = Vec::new();
    for entry in COMMANDS {
        let Some(policy) = entry.failure else {
            continue;
        };
        if let Some(order) = policy.parse_order {
            parse.push((order, entry.name));
        }
        if let RuntimeFailure::Stderr(order) = policy.runtime {
            stderr.push((order, entry.name));
        }
    }
    parse.sort_unstable();
    stderr.sort_unstable();
    fn names(entries: &[(usize, &str)]) -> String {
        let (last, rest) = entries.split_last().expect("failure note commands");
        format!(
            "{} and {}",
            rest.iter()
                .map(|(_, name)| *name)
                .collect::<Vec<_>>()
                .join(", "),
            last.1
        )
    }
    format!(
        "Common defaults are not universal: consent, settings, sessions, prune-sessions and close-session print JSON without schema_version, and self-test's report has outcome instead of ok; {} report errors on stderr without a JSON error object. With --json, an argument-parsing failure prints a JSON error object only for {}; other commands report it on stderr only. Other command fields may override defaults. Consult result fields; exit status alone does not identify request completion.",
        names(&stderr),
        names(&parse)
    )
}
