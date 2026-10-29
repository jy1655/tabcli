#!/usr/bin/env python3
"""Project supplied public query JSON onto a bounded, offline support summary."""

import argparse
import json
import math
import re
import sys


MAX_INPUT_BYTES = 4 * 1024 * 1024
MAX_DEPTH = 32
MAX_NODES = 100000
MAX_ITEMS = 24
MAX_OUTPUT_BYTES = 64 * 1024
MAX_INTEGER = (1 << 53) - 1
MISSING = object()


def enum(words):
    return frozenset(words.split())


# These are literal public categories, never patterns accepting arbitrary tokens.
PROVIDERS = enum("codex claude agy pi")
SESSION_STATES = enum("""
    launching running awaiting-initial-input ready claimed working resume-pending
    exited failed closed unknown
""")
REQUEST_STATES = enum("""
    completed failed cancelled recovery_required pending unresolved unavailable unknown
""")
AVAILABILITY = enum("available unavailable unknown")
ATTENTION = enum("""
    cancel_requested held delivery_unconfirmed launch_timeout launch_failed launch_uncertain
    claim_without_receipt recovery_required owner_exited owner_identity_mismatch owner_unverified
    records_partially_unreadable request_index_incomplete residual_surface_unverified
    session_failed session_exited origin_ended question_emitted
""")
CHECK_IDS = enum("""
    platform session_records session_state active_request turn completion request_index
    terminal_selection terminal_surface terminal_record powershell provider provider_executable
    provider_version workspace workspace_consent owner hold cancel origin reopen_marker
    claude_inbound_setting claude_caller_markers claude_messaging claude_resumed_conversation_holders
    claude_completion claude_completion_diagnostic
    codex_terminal_follow_up codex_queue_version codex_current_queue_version codex_thread codex_daemon
    codex_hook_trust codex_lifecycle codex_mcp codex_notify_payload_budget
    agy_follow_up agy_workspace_trust agy_input_receipt
    agy_tool_confirmation pi_follow_up pi_provider_credentials
""")
REASONS = enum("""
    platform_supported platform_unsupported session_unavailable records_read records_busy
    records_unreadable stored_state active_request_unknown delivery_unconfirmed turn_in_progress
    no_active_turn recovery_required no_pending_completion request_index_incomplete
    launch_timeout launch_failed launch_uncertain terminal_selected terminal_unsupported
    terminal_selection_failed terminal_surface_present terminal_surface_missing
    terminal_surface_unverified terminal_consumed terminal_close_in_progress
    terminal_binding_unverified terminal_record_found terminal_record_missing
    terminal_record_unreadable terminal_tombstone_unreadable powershell_found
    powershell_unavailable provider_unrecognized executable_found executable_path_unverified
    executable_unavailable version_supported version_unsupported version_unrecognized
    version_probe_failed probe_not_requested workspace_present workspace_missing
    workspace_not_directory workspace_unreadable recorded_workspace_consent owner_exited owner_identity_mismatch
    owner_identity_observed owner_session_mismatch owner_unbound_legacy owner_unverified
    owner_unreadable not_held session_held hold_unreadable cancel_absent cancel_recorded
    cancel_unreadable origin_external origin_live origin_ended origin_ended_session_closed
    origin_unverified agy_tool_confirmation_observed agy_tool_confirmation_unobserved
    agy_tool_confirmation_unverified agy_workspace_trusted agy_workspace_untrusted
    agy_session_trust_unverified agy_trust_evidence_unreadable agy_trust_store_unlocated
    agy_log_unavailable agy_log_missing agy_log_unreadable agy_startup_markers_missing
    agy_no_input_receipt agy_input_receipt_observed agy_terminal_fallback pi_terminal_fallback
    pi_provider_credentials_ready pi_provider_credentials_not_ready pi_provider_credentials_unknown
    claude_inbound_configured claude_inbound_unverified claude_settings_unreadable
    claude_settings_unavailable claude_caller_markers_absent claude_caller_markers_removed_at_launch
    claude_runtime_gates_unverified claude_resumed_conversation_exclusive_now
    claude_resumed_conversation_shared claude_resumed_conversation_unverified
    claude_completion_mode_unreadable claude_completion_mode_malformed
    claude_completion_mode_unrecognized claude_completion_mode_absent claude_completion_legacy_stop
    claude_completion_readiness_recorded claude_completion_readiness_not_observed
    claude_completion_readiness_unrecognized claude_completion_readiness_malformed
    claude_completion_readiness_unreadable claude_completion_diagnostic_recorded
    claude_completion_diagnostic_not_recorded claude_completion_diagnostic_unrecognized
    claude_completion_diagnostic_malformed claude_completion_diagnostic_unreadable
    codex_hook_trust_unavailable codex_hook_trust_observed codex_hook_trust_attention
    codex_active_terminal_thread_unverified codex_current_queue_version_supported
    codex_current_queue_version_unknown codex_current_queue_version_unsupported
    codex_daemon_compatible codex_daemon_incompatible codex_daemon_probe_failed
    codex_daemon_unavailable codex_daemon_unverified codex_mcp_not_observed
    codex_notify_payload_near_limit codex_notify_payload_no_limit codex_notify_payload_not_required
    codex_notify_payload_over_limit codex_notify_payload_within_limit codex_pending_prompt_unavailable
    codex_queue_version_supported codex_queue_version_unknown codex_queue_version_unsupported
    codex_thread_invalid codex_thread_missing codex_thread_recorded codex_thread_unreadable
    codex_turn_not_pending codex_lifecycle_session_observations session_required workspace_unavailable
    reopen_marker_unreadable reopen_in_progress reopen_marker_consumed reopen_marker_reconcilable
    reopen_marker_retained reopen_marker_unverified
""")
ELAPSED_REASONS = enum("""
    no_published_result missing_receipt missing_receipt_time missing_result_time inverted_time unreadable_result
""")
STAGES = enum("""
    cancel_request request_receipt completion delivery_refusal event_index request_index
    launch_phase completion_journal status closed_status terminal_closing terminal_closed launch_log turn_claim
""")
RECORD_STATES = enum("observed missing unreadable unpublished")
QUESTION_OBSERVATIONS = enum("incomplete unreadable")
SOURCES = enum("""
    cancel.json manifest.json status.json turn.claim turn.completion.json launch.json launch.log
    closed.json terminal.closing.json terminal.closed.json requests/ events/
""")
ORIGIN_REASONS = enum("""
    recorded_closed recorded_failed recorded_exited owner_exited owner_reused owner_live
    owner_unverified origin_missing origin_unreadable origin_unverified origin_busy
    origin_budget_exhausted origin_external initial_request_unavailable initial_receipt_unreadable
""")
OMISSIONS = (
    "Only allowlisted, type-validated public fields are included. All identifiers, paths, titles, "
    "prompt/result/question text, errors, detail, next actions, commands, arbitrary evidence, "
    "environment and extension fields are omitted. Unknown categories are not copied."
)
PROVENANCE = (
    "Offline projection of supplied public JSON; not authenticated, refreshed, or runtime proof. "
    "Counts describe supplied arrays, not a complete session history. Request elapsed time is Bridge "
    "receipt-to-result wall time, not model or billing time."
)


class InputError(ValueError):
    """Messages are fixed categories and never contain supplied text."""


def bounded_integer(value):
    return type(value) is int and 0 <= value <= MAX_INTEGER


def boolean(value):
    return type(value) is bool


def version(value):
    # Retain only numeric components, including known CLI --version wrappers.
    if type(value) is not str or len(value) > 64:
        return None
    if value.startswith("codex-cli "):
        value = value[len("codex-cli "):]
    elif value.startswith("agy version "):
        value = value[len("agy version "):]
    elif value.endswith(" (Claude Code)"):
        value = value[:-len(" (Claude Code)")]
    component = r"(?:0|[1-9][0-9]{0,5})"
    return value if re.fullmatch(component + r"\." + component + r"\." + component, value) else None


def source_family(value):
    # Never return an input filename, even when it has a plausible public shape.
    if type(value) is not str:
        return None
    if value in SOURCES:
        return value
    for prefix, suffix, family in (("requests/", ".json", "requests/*.json"),
                                   ("events/", "", "events/*")):
        if not value.startswith(prefix):
            continue
        name = value[len(prefix):]
        if (len(name) <= 255 and len(name) > len(suffix) and name.endswith(suffix)
                and name not in (".", "..")
                and re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._-]*", name)):
            return family
    return None


def field_state(value):
    if value is MISSING:
        return "missing"
    if value is None:
        return "null"
    return "invalid"


def project(source, fields):
    """Never traverse or copy a key outside this projection's fixed schema."""
    if type(source) is not dict:
        return {"input_state": field_state(source)}
    result, unavailable = {}, {}
    for name, validator in fields.items():
        value = source.get(name, MISSING)
        if value is MISSING or value is None:
            unavailable[name] = field_state(value)
        elif isinstance(validator, frozenset):
            if type(value) is str and value in validator:
                result[name] = value
            else:
                unavailable[name] = "unrecognized" if type(value) is str else "invalid"
        elif validator is version:
            safe = version(value)
            if safe is None:
                unavailable[name] = "invalid"
            else:
                result[name] = safe
        elif validator is source_family:
            safe = source_family(value)
            if safe is None:
                unavailable[name] = "unrecognized" if type(value) is str else "invalid"
            else:
                result[name] = safe
        elif validator(value):
            result[name] = value
        else:
            unavailable[name] = "invalid"
    if unavailable:
        result["unavailable_fields"] = unavailable
    return result


def collection(value, render):
    if type(value) is not list:
        return {"input_state": field_state(value)}
    return {"input_count": len(value), "included_count": min(len(value), MAX_ITEMS),
            "omitted_count": max(0, len(value) - MAX_ITEMS),
            "items": [render(item) for item in value[:MAX_ITEMS]]}


def child(source, name, render):
    value = source.get(name, MISSING)
    return render(value)


def request(source):
    return project(source, {
        "request_state": REQUEST_STATES, "session_state": SESSION_STATES,
        "bridge_observed_elapsed_ms": bounded_integer,
        "bridge_observed_elapsed_reason": ELAPSED_REASONS,
        "recovery_required": boolean, "active": boolean, "question_emitted": boolean,
        "question_emission_count": bounded_integer,
        "question_observation": QUESTION_OBSERVATIONS,
        "delivery": enum("completion_recorded refused_not_sent unknown"),
        "derived_from": enum("result observation"),
    })


def origin(source):
    return project(source, {"kind": enum("managed external unknown"),
                            "stored_state": SESSION_STATES, "ended": boolean,
                            "reason": ORIGIN_REASONS, "derived_from": enum("records")})


def session(source, state_key="state"):
    result = project(source, {
        "provider": PROVIDERS, state_key: SESSION_STATES, "held": boolean,
        "generation": bounded_integer, "turn_claimed": boolean, "recovery_required": boolean,
        "unreadable_requests": bounded_integer, "derived_from": enum("observation"),
        "residual_surface": enum("unverified"),
        "question_observation": QUESTION_OBSERVATIONS,
    })
    if type(source) is dict:
        result["active_request"] = child(source, "active_request", request)
        result["latest_result"] = child(source, "latest_result", request)
        result["origin_observation"] = child(source, "origin_observation", origin)
        result["owner"] = child(source, "owner", lambda item: project(item, {
            "process_alive": boolean, "identity_matches": boolean}))
        result["attention"] = collection(source.get("attention", MISSING),
                                          lambda item: project({"category": item}, {"category": ATTENTION}))
    return result


def doctor(source):
    result = project(source, {"provider": PROVIDERS, "probe": boolean})
    result["scope"] = child(source, "scope", lambda item: project(item, {
        "provider": PROVIDERS, "current_version": version, "probe": boolean,
        "probe_budget_ms": bounded_integer}))
    result["configured"] = child(source, "configured", lambda item: project(item, {
        "provider_version_at_launch": version}))
    result["observations"] = child(source, "observations", lambda item: project(item, {
        "stored_state": SESSION_STATES, "generation": bounded_integer,
        "active_request_state": REQUEST_STATES, "recovery_required": boolean}))
    result["checks"] = collection(source.get("checks", MISSING), lambda item: project(item, {
        "id": CHECK_IDS, "availability": AVAILABILITY, "reason_code": REASONS}))
    times = project(source, {"started_unix_ms": bounded_integer, "finished_unix_ms": bounded_integer})
    if "unavailable_fields" in times:
        result["query_duration"] = {"unavailable_fields": times["unavailable_fields"]}
    elif times["finished_unix_ms"] < times["started_unix_ms"]:
        result["query_duration"] = {"unavailable_fields": {"elapsed_ms": "inverted_time"}}
    else:
        result["query_duration"] = {"elapsed_ms": times["finished_unix_ms"] - times["started_unix_ms"],
                                    "derived_from": "supplied_doctor_timestamps"}
    return result


def status(source):
    result = project(source, {"incomplete": boolean})
    result["scanned"] = child(source, "scanned", lambda item: project(item, {
        "sessions": bounded_integer, "listed": bounded_integer}))
    # Incomplete reasons can contain local paths and arbitrary error messages.
    reasons = source.get("incomplete_reasons", MISSING)
    result["incomplete_reasons"] = ({"input_count": len(reasons), "content": "omitted"}
                                    if type(reasons) is list else {"input_state": field_state(reasons)})
    result["sessions"] = collection(source.get("sessions", MISSING), session)
    return result


def timeline(source):
    result = project(source, {"session_state": SESSION_STATES, "recovery_required": boolean,
                              "incomplete": boolean, "unreadable_requests": bounded_integer})
    result["requests"] = collection(source.get("requests", MISSING), request)
    for key in ("entries", "session_entries"):
        result[key] = collection(source.get(key, MISSING), lambda item: project(item, {
            "stage": STAGES, "record_state": RECORD_STATES, "source": source_family}))
    return result


def inspect(source):
    result = session(source, "stored_state")
    result["event_counts"] = project(source, {"recorded_events": bounded_integer})
    result["requests"] = collection(source.get("requests", MISSING), request)
    result["configured"] = child(source, "configured", lambda item: project(item, {
        "provider_version_at_launch": version}))
    return result


def summarize(source):
    if type(source) is not dict:
        raise InputError("expected one public query JSON object")
    if type(source.get("schema_version")) is not int or source["schema_version"] != 1:
        raise InputError("unsupported public schema version")
    shapes = {"doctor": "checks" in source, "status": "sessions" in source,
              "timeline": "entries" in source or "session_entries" in source,
              "inspect": "stored_state" in source}
    kinds = [kind for kind, matches in shapes.items() if matches]
    if len(kinds) != 1:
        raise InputError("unrecognized or ambiguous public query shape")
    kind = kinds[0]
    result = {"summary_schema_version": 1, "source_kind": kind, "source_schema_version": 1,
              "provenance": PROVENANCE, "omissions": OMISSIONS,
              "limits": {"items_per_array": MAX_ITEMS, "input_bytes": MAX_INPUT_BYTES,
                         "maximum_integer": MAX_INTEGER}}
    result.update(project(source, {"ok": boolean}))
    result["summary"] = {"doctor": doctor, "status": status,
                         "timeline": timeline, "inspect": inspect}[kind](source)
    return result


def reject_constant(_value):
    raise InputError("non-finite JSON number")


def parse_integer(value):
    if len(value) > 21:
        raise InputError("oversized JSON integer")
    return int(value)


def parse_float(value):
    number = float(value)
    if not math.isfinite(number):
        raise InputError("non-finite JSON number")
    return number


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise InputError("duplicate JSON key")
        result[key] = value
    return result


def read_document(stream):
    raw = stream.read(MAX_INPUT_BYTES + 1)
    if len(raw) > MAX_INPUT_BYTES:
        raise InputError("input byte limit exceeded")
    try:
        text = raw.decode("utf-8", errors="strict")
    except UnicodeError:
        raise InputError("input is not strict UTF-8") from None
    # Bound nesting before invoking the recursive stdlib JSON decoder.
    depth, quoted, escaped = 0, False, False
    for char in text:
        if quoted:
            if escaped:
                escaped = False
            elif char == "\\":
                escaped = True
            elif char == '"':
                quoted = False
        elif char == '"':
            quoted = True
        elif char in "[{":
            depth += 1
            if depth > MAX_DEPTH:
                raise InputError("input nesting limit exceeded")
        elif char in "]}":
            depth -= 1
    try:
        source = json.loads(text, object_pairs_hook=unique_object, parse_int=parse_integer,
                            parse_float=parse_float, parse_constant=reject_constant)
    except (ValueError, RecursionError):
        raise InputError("invalid or unsupported JSON") from None
    pending, nodes = [source], 0
    while pending:
        value = pending.pop()
        nodes += 1
        if nodes > MAX_NODES:
            raise InputError("input node limit exceeded")
        if type(value) is dict:
            pending.extend(value.values())
        elif type(value) is list:
            pending.extend(value)
    return source


def render(summary, output_format):
    # The projection emits ASCII literals and validated numeric version components only.
    payload = json.dumps(summary, ensure_ascii=True, allow_nan=False, indent=2, sort_keys=True) + "\n"
    if output_format == "markdown":
        payload = "# Agent Bridge diagnostic summary\n\n```json\n" + payload + "```\n"
    if len(payload.encode("ascii")) > MAX_OUTPUT_BYTES:
        raise InputError("summary output limit exceeded")
    return payload


class Arguments(argparse.ArgumentParser):
    def error(self, _message):
        raise InputError("invalid command-line arguments")


def main(argv=None):
    parser = Arguments(prog="share_diagnostics.py", description=__doc__)
    parser.add_argument("--format", choices=("markdown", "json"), default="markdown")
    try:
        args = parser.parse_args(argv)
        payload = render(summarize(read_document(sys.stdin.buffer)), args.format)
        sys.stdout.write(payload)
    except InputError as error:
        print("share_diagnostics: " + str(error), file=sys.stderr)
        return 2
    except OSError:
        print("share_diagnostics: input/output unavailable", file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
