//! Pi's reported usage, read from the extension's hook payload and from its failure signal.
//!
//! The extension sums the `usage` of the assistant messages of every `agent_end` of the
//! correlated claim (Pi documents `usage` as required on every assistant message) and sends
//! either `usage` or a bounded `usage_error`. This module validates that object once more
//! with the same rules (non-negative integers, counts within `u32`) and turns it into the
//! provider-neutral [`ProviderUsage`]; a payload that fails validation becomes a bounded
//! `usage_error` and never fails the completion. Replace the extension's accumulation when
//! Pi reports a turn's usage through an addressed first-party signal.
use crate::native::session::schema::{ProviderUsage, bounded_usage_error};
use serde_json::Value;

/// The documented signal the usage is taken from.
pub(super) const USAGE_SOURCE: &str = "pi_extension_agent_end";

/// The usage of one request as the payload reports it: the usage, or the bounded reason
/// there is none. Both absent when the payload carries neither field (an older extension).
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct ReportedUsage {
    pub(super) usage: Option<ProviderUsage>,
    pub(super) usage_error: Option<String>,
}

pub(super) fn reported_usage(payload: &Value) -> ReportedUsage {
    if let Some(error) = payload.get("usage_error") {
        let error = match error.as_str().map(str::trim) {
            Some(error) if !error.is_empty() => error,
            _ => "malformed usage_error",
        };
        return ReportedUsage {
            usage: None,
            usage_error: Some(bounded_usage_error(error)),
        };
    }
    let Some(usage) = payload.get("usage") else {
        return ReportedUsage::default();
    };
    match validate(usage) {
        Ok(usage) => ReportedUsage {
            usage: Some(usage),
            usage_error: None,
        },
        Err(field) => ReportedUsage {
            usage: None,
            usage_error: Some(bounded_usage_error(&format!("malformed usage: {field}"))),
        },
    }
}

/// JavaScript's `Number.MAX_SAFE_INTEGER`: the extension's contract is safe integers, and a
/// larger count can only be a sum JavaScript could not represent exactly.
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

fn safe_count(value: &Value, field: &'static str) -> Result<u64, &'static str> {
    match value.as_u64() {
        Some(count) if count <= MAX_SAFE_INTEGER => Ok(count),
        _ => Err(field),
    }
}

fn count(usage: &Value, field: &'static str) -> Result<u64, &'static str> {
    safe_count(usage.get(field).ok_or(field)?, field)
}

fn small_count(usage: &Value, field: &'static str) -> Result<u32, &'static str> {
    u32::try_from(count(usage, field)?).map_err(|_| field)
}

fn validate(usage: &Value) -> Result<ProviderUsage, &'static str> {
    if !usage.is_object() {
        return Err("usage is not an object");
    }
    let coverage = match usage.get("coverage").and_then(Value::as_str) {
        Some(coverage @ ("complete" | "uncertain")) => coverage.to_owned(),
        _ => return Err("coverage"),
    };
    let reasoning_tokens = match usage.get("reasoning_tokens") {
        None | Some(Value::Null) => None,
        Some(value) => Some(safe_count(value, "reasoning_tokens")?),
    };
    Ok(ProviderUsage {
        source: USAGE_SOURCE.to_owned(),
        scope: "request".to_owned(),
        coverage,
        runs: small_count(usage, "runs")?,
        assistant_messages: small_count(usage, "assistant_messages")?,
        input_tokens: count(usage, "input_tokens")?,
        output_tokens: count(usage, "output_tokens")?,
        cache_read_tokens: count(usage, "cache_read_tokens")?,
        cache_write_tokens: count(usage, "cache_write_tokens")?,
        reasoning_tokens,
        total_tokens: count(usage, "total_tokens")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn complete() -> Value {
        json!({"coverage": "complete", "runs": 1, "assistant_messages": 2, "input_tokens": 10,
            "output_tokens": 20, "cache_read_tokens": 30, "cache_write_tokens": 40,
            "reasoning_tokens": 5, "total_tokens": 100})
    }

    #[test]
    fn valid_usage_becomes_the_provider_neutral_record() {
        let reported = reported_usage(&json!({"usage": complete()}));
        assert_eq!(reported.usage_error, None);
        let usage = reported.usage.unwrap();
        assert_eq!(usage.source, USAGE_SOURCE);
        assert_eq!(usage.scope, "request");
        assert_eq!(usage.coverage, "complete");
        assert_eq!((usage.runs, usage.assistant_messages), (1, 2));
        assert_eq!(
            (
                usage.input_tokens,
                usage.output_tokens,
                usage.cache_read_tokens,
                usage.cache_write_tokens,
                usage.reasoning_tokens,
                usage.total_tokens
            ),
            (10, 20, 30, 40, Some(5), 100)
        );
        let mut without_reasoning = complete();
        without_reasoning
            .as_object_mut()
            .unwrap()
            .remove("reasoning_tokens");
        let usage = reported_usage(&json!({"usage": without_reasoning}))
            .usage
            .unwrap();
        assert_eq!(usage.reasoning_tokens, None);
        assert!(!serde_json::to_string(&usage).unwrap().contains("reasoning"));
    }

    #[test]
    fn absent_fields_report_nothing_and_errors_are_bounded() {
        assert_eq!(reported_usage(&json!({})), ReportedUsage::default());
        let long = "x".repeat(500);
        let reported = reported_usage(&json!({"usage_error": long, "usage": complete()}));
        assert_eq!(reported.usage, None);
        assert_eq!(reported.usage_error.unwrap().chars().count(), 200);
        assert_eq!(
            reported_usage(&json!({"usage_error": "  "})).usage_error,
            Some("malformed usage_error".to_owned())
        );
    }

    #[test]
    fn every_malformed_operand_is_named_without_a_partial_record() {
        for (field, value) in [
            ("input_tokens", json!(-1)),
            ("output_tokens", json!(1.5)),
            ("cache_read_tokens", json!("3")),
            ("cache_write_tokens", json!(null)),
            ("total_tokens", json!(u64::MAX as f64 * 4.0)),
            ("reasoning_tokens", json!(-2)),
            ("runs", json!(u64::from(u32::MAX) + 1)),
            ("assistant_messages", json!(-1)),
            ("coverage", json!("partial")),
        ] {
            let mut usage = complete();
            usage[field] = value;
            let reported = reported_usage(&json!({"usage": usage}));
            assert_eq!(reported.usage, None, "{field}");
            assert_eq!(
                reported.usage_error.as_deref(),
                Some(format!("malformed usage: {field}").as_str())
            );
        }
        let reported = reported_usage(&json!({"usage": 7}));
        assert_eq!(
            reported.usage_error.as_deref(),
            Some("malformed usage: usage is not an object")
        );
    }

    #[test]
    fn counts_above_javascript_safe_integers_are_malformed() {
        for field in ["total_tokens", "reasoning_tokens"] {
            let mut usage = complete();
            usage[field] = json!(MAX_SAFE_INTEGER);
            let accepted = reported_usage(&json!({"usage": usage})).usage.unwrap();
            let value = if field == "reasoning_tokens" {
                accepted.reasoning_tokens.unwrap()
            } else {
                accepted.total_tokens
            };
            assert_eq!(value, 9_007_199_254_740_991, "{field}");
            let mut usage = complete();
            usage[field] = json!(MAX_SAFE_INTEGER + 1);
            let reported = reported_usage(&json!({"usage": usage}));
            assert_eq!(reported.usage, None, "{field}");
            assert_eq!(
                reported.usage_error.as_deref(),
                Some(format!("malformed usage: {field}").as_str())
            );
        }
    }
}
