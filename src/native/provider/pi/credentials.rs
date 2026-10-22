use super::PI_PROVIDER_CREDENTIALS;
use anyhow::Result;
use serde::Deserialize;
use std::{
    path::Path,
    time::{Duration, Instant},
};

// This observation changes nothing in turn correlation. Remove it when Pi emits an
// official prompt-rejection or completion signal identifying the submitted request
// before the agent turn starts. Configuration is not authentication acceptance.
pub(super) fn credential_diagnosis(
    context: crate::native::diagnostics::Context<'_>,
    mut probe: impl FnMut(&Path, &[&str], &Path, Instant) -> Result<std::process::Output>,
) -> crate::native::diagnostics::Check {
    let target = (
        context.manifest.and_then(|m| m.model.as_deref()),
        context.executable,
    );
    let unknown = |reason| credential_check(None, "unknown", reason, None, None, target);
    let Some(model) = context.manifest.and_then(|m| m.model.as_deref()) else {
        return unknown(
            "No model is known; a session with an explicit provider/model is required.",
        );
    };
    let Some(provider) = model_provider(model) else {
        return unknown("Provider not derivable from this model; use an explicit provider/model.");
    };
    if !context.probe {
        return credential_check(
            Some(&provider),
            "unknown",
            "Credential probe not requested; use doctor SESSION --probe.",
            None,
            None,
            target,
        );
    }
    let Some(executable) = context.executable else {
        return credential_check(
            Some(&provider),
            "unknown",
            "Pi command is missing or unavailable.",
            None,
            None,
            target,
        );
    };
    // Pi 1.0.0's resolver can prefer a raw model id on another provider even when
    // the prefix is known. Let Pi confirm the prefix, without refreshing or a turn;
    // never duplicate its catalog or infer a provider from an unqualified name.
    // Both commands share this budget, inside self-test's five-second doctor budget.
    let deadline = context
        .deadline
        .min(Instant::now() + Duration::from_secs(3));
    let resolved = probe(
        executable,
        &["auth", "check", "--model", model, "--no-refresh", "--json"],
        context.workspace,
        deadline,
    );
    let resolution = parse_credentials(&provider, resolved, target);
    if resolution.availability == crate::native::diagnostics::Availability::Unknown {
        return resolution;
    }
    parse_credentials(
        &provider,
        probe(
            executable,
            &[
                "auth",
                "check",
                "--provider",
                &provider,
                "--no-refresh",
                "--json",
            ],
            context.workspace,
            deadline,
        ),
        target,
    )
}

fn model_provider(model: &str) -> Option<String> {
    let (provider, model) = model.split_once('/')?;
    if provider.is_empty()
        || model.is_empty()
        || model.chars().any(char::is_whitespace)
        || !provider
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return None;
    }
    Some(provider.to_ascii_lowercase())
}

fn credential_check(
    provider: Option<&str>,
    status: &str,
    reason: &str,
    auth_type: Option<&str>,
    exit_code: Option<i32>,
    target: (Option<&str>, Option<&Path>),
) -> crate::native::diagnostics::Check {
    use crate::native::diagnostics::{Availability, Check};
    let (availability, code) = match status {
        "ready" => (Availability::Available, "pi_provider_credentials_ready"),
        "not_ready" => (
            Availability::Unavailable,
            "pi_provider_credentials_not_ready",
        ),
        _ => (Availability::Unknown, "pi_provider_credentials_unknown"),
    };
    Check::new(PI_PROVIDER_CREDENTIALS, availability, code,
        format!("Pi provider credentials {status}{}: {reason}", provider.map(|p| format!(" for {p}")).unwrap_or_default()),
        "Inspect Pi's authentication setup. This observation does not fail, cancel, or resend a request.")
        .evidence(serde_json::json!({"status": status, "provider": provider, "authType": auth_type, "exit_code": exit_code, "model": target.0, "executable": target.1}))
}

fn parse_credentials(
    provider: &str,
    output: Result<std::process::Output>,
    target: (Option<&str>, Option<&Path>),
) -> crate::native::diagnostics::Check {
    let output = match output {
        Ok(output) => output,
        Err(error) => {
            // Never echo command output or arbitrary error strings that may contain secrets.
            let reason = if error.to_string().contains("timed out")
                || error.to_string().contains("deadline")
            {
                "Credential check timed out."
            } else {
                "Credential check could not run; Pi command missing or unavailable."
            };
            return credential_check(Some(provider), "unknown", reason, None, None, target);
        }
    };
    let exit = output.status.code();
    #[derive(Deserialize)]
    struct AuthStatus {
        status: String,
        provider: String,
        #[serde(rename = "authType")]
        auth_type: Option<String>,
        reason: Option<String>,
    }
    let parsed = serde_json::from_slice::<AuthStatus>(&output.stdout);
    let Ok(value) = parsed else {
        return credential_check(
            Some(provider),
            "unknown",
            "Credential check returned malformed or unsupported JSON.",
            None,
            exit,
            target,
        );
    };
    if value.provider != provider {
        return credential_check(
            Some(provider),
            "unknown",
            "Provider not derivable: Pi did not confirm the model's provider.",
            None,
            exit,
            target,
        );
    }
    let (status, reason, auth_type) = match (
        value.status.as_str(),
        value.reason.as_deref(),
        value.auth_type.as_deref(),
        exit,
    ) {
        // Pi 1.0.0 dist/cli/auth-check.js:24-30 gates ready on a checkAuth result,
        // then copies auth.type as metadata; dist/main.js:162 derives exit from status.
        // pi-ai/dist/models.js:263-294 delegates API-key checks and returns their result.
        // Although auth/types.d.ts:94-98 currently lists two types, the label does not
        // gate readiness. Preserve it as evidence rather than imposing our own gate.
        ("ready", _, kind, Some(0)) => (
            "ready",
            "Credentials are configured, not verified valid or accepted by the provider.",
            kind,
        ),
        ("not_ready", Some("credentials_not_configured"), None, Some(1)) => (
            "not_ready",
            "Pi has no credentials configured for this provider.",
            None,
        ),
        ("not_ready", Some("provider_not_found"), None, Some(1)) => (
            "unknown",
            "Provider not derivable: Pi does not recognize this provider (provider_not_found).",
            None,
        ),
        ("invalid", Some("invalid_state"), None, Some(2)) => (
            "unknown",
            "Pi reports invalid authentication state (invalid_state).",
            None,
        ),
        _ => (
            "unknown",
            "Credential check returned an unsupported status or exit code.",
            None,
        ),
    };
    credential_check(Some(provider), status, reason, auth_type, exit, target)
}

#[cfg(test)]
mod tests;
