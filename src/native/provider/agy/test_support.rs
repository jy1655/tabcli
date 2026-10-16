use super::monitor::parse_planner_result;
use super::{PendingAgyTurn, install_pending_turn, terminal_correlated_prompt};
use crate::native::session::{Store, turn};

use anyhow::Result;
use std::path::Path;

#[cfg(test)]
pub(super) fn parse_transcript_line(line: &str) -> Option<(u64, String)> {
    let result = parse_planner_result(line)?;
    (!result.truncated).then_some((result.step, result.message))
}

pub(super) fn claim_pending_turn(directory: &Path) -> PendingAgyTurn {
    let claim = turn::claim(&Store::open_unchecked(directory), &[]).unwrap();
    let token = claim.token().to_owned();
    claim.retain();
    install_pending_turn(directory, &token).unwrap()
}

pub(super) fn marked(message: &str, pending: &PendingAgyTurn) -> String {
    format!("{message}\n{}", pending.marker)
}

pub(super) fn planner_line(step: u64, content: &str) -> String {
    serde_json::json!({
        "type": "PLANNER_RESPONSE",
        "status": "DONE",
        "source": "MODEL",
        "step_index": step,
        "content": content,
    })
    .to_string()
}

// Real log excerpts written by Agy 1.2.10 on 2026-09-24 (issue #43), taken from
// `%USERPROFILE%\.agent-bridge\native-sessions\<session>\agy.log`. Every kept line
// is verbatim; the listed line numbers are the file positions and the omitted lines
// are HTTP, auth, model and quota chatter (the auth lines carry the account email
// and are not reproduced). Timestamps are local time (KST).
//
// session-udT6uY (initial paste delivered): lines 91, 100, 101, 119-123, 134, 147
// and 153 are the startup. The main thread's hooks line (91) precedes the startup
// reload (120), whose own hooks completion (122) follows at once. That startup
// reload is stamped 0.5 ms after `CLI startup completed`, so it is the startup
// reload, not the trust reload, which this trusted workspace never logs.
pub(super) const REAL_SUCCESS_STARTUP: &str = r"I0924 16:42:24.068931       1 hooks_manager.go:53] loaded 0 named hooks from 0 hooks.json file(s)
I0924 16:42:24.073126       1 common.go:438] Starting CLI program
CLI ready for user input
I0924 16:42:24.080467       1 analytics.go:187] CLI startup completed (took 230.2044ms)
I0924 16:42:24.080986     215 manager.go:1331] Reloading system slash commands and skills
I0924 16:42:24.080986     215 manager.go:1308] Reloading system slash commands
I0924 16:42:24.081497     200 hooks_manager.go:53] loaded 0 named hooks from 0 hooks.json file(s)
I0924 16:42:24.127839     402 manager.go:934] Full redraw completed (rerenderAll) for conversation  (epoch 0, items 1)
I0924 16:42:25.848949     499 manager.go:1308] Reloading system slash commands
I0924 16:42:28.530216     508 manager.go:1308] Reloading system slash commands
I0924 16:42:29.104592     531 manager.go:1308] Reloading system slash commands
";

// session-udT6uY lines 157, 169 and 170: what followed the receipt.
pub(super) const REAL_SUCCESS_AFTER_RECEIPT: &str = r"I0924 16:42:50.213057     141 conversation_manager.go:512] Starting new conversation (agent=false)
I0924 16:42:50.228104     580 manager.go:1331] Reloading system slash commands and skills
I0924 16:42:50.228760     580 manager.go:1308] Reloading system slash commands
";

// session-fMqSQc (initial paste lost): lines 95, 104, 105, 114-116, 126, 127, 138,
// 150 and 151. The startup reload (114) precedes `CLI startup completed` and
// skipped its hooks pass, so the only hooks line before 16:41:20 is the main
// thread's (95). The fixed 12 second delay pasted at about 16:41:19, and the
// round-3 gate would have been ready at 16:41:17.24, 3.5 s after the 16:41:13
// reload; both precede the trust reload below, which the paste itself caused.
pub(super) const REAL_FAILURE_STARTUP: &str = r"I0924 16:41:07.819578       1 hooks_manager.go:53] loaded 0 named hooks from 0 hooks.json file(s)
I0924 16:41:07.823186       1 common.go:438] Starting CLI program
CLI ready for user input
I0924 16:41:07.826763     280 manager.go:1331] Reloading system slash commands and skills
I0924 16:41:07.826763     280 manager.go:1308] Reloading system slash commands
I0924 16:41:07.826763     280 manager.go:1312] Slash commands unchanged, skipping update
I0924 16:41:07.830345       1 analytics.go:187] CLI startup completed (took 226.5353ms)
I0924 16:41:07.876154     269 manager.go:934] Full redraw completed (rerenderAll) for conversation  (epoch 0, items 1)
I0924 16:41:10.732767     345 manager.go:1308] Reloading system slash commands
I0924 16:41:13.737805     383 manager.go:1308] Reloading system slash commands
I0924 16:41:13.739814     383 manager.go:1312] Slash commands unchanged, skipping update
";

// session-fMqSQc lines 153-156, the end of the file: the deferred skills reload
// that discarded the paste, 13.0 s after startup, and the first hooks completion
// after a skills reload.
// Nothing was logged after line 156 until the session was closed at 16:42:22: no
// HandleUserInput receipt and no line that shows the console input was drained.
const REAL_FAILURE_LATE_RELOAD: &str = r"I0924 16:41:20.813553     410 manager.go:1331] Reloading system slash commands and skills
I0924 16:41:20.813553     410 manager.go:1308] Reloading system slash commands
I0924 16:41:20.814059     406 hooks_manager.go:53] loaded 0 named hooks from 0 hooks.json file(s)
I0924 16:41:20.816140     410 manager.go:1312] Slash commands unchanged, skipping update
";

// session-IQHEwf (this machine, 2026-09-24 17:20, Agy 1.2.10; the round-1 gate
// waited the full deadline and never pasted): the startup verbatim after the ANSI
// strip, with the account email redacted. The startup reload precedes
// `CLI startup completed` and no hooks_manager.go line follows it, the deferred
// `... and skills` reload never came, and after three plain reloads within 6.4 s
// the log was silent for five minutes. A healthy session, so the reload/hooks
// pair cannot be the readiness discriminator, and the deferred reload cannot be
// required at all: the gate pastes one quiet period after the last plain reload.
pub(super) const REAL_QUIET_STARTUP: &str = r#"E0924 17:20:28.610124     222 errorreport.go:224] error getting token source: You are not logged into Antigravity.
W0924 17:20:28.610124     222 cache.go:135] Cache(userInfo): Singleflight refresh failed: failed to get load code assist response: error getting token source: You are not logged into Antigravity.
E0924 17:20:28.610124     222 errorreport.go:224] failed to get load code assist response: error getting token source: You are not logged into Antigravity.
I0924 17:20:28.613199     270 manager.go:1331] Reloading system slash commands and skills
I0924 17:20:28.613199     270 manager.go:1308] Reloading system slash commands
I0924 17:20:28.613199     270 manager.go:1312] Slash commands unchanged, skipping update
I0924 17:20:28.614019     232 keyring.go:64] keyringAuth: loaded token, expiry=2026-09-24 17:30:33.268465 +0900 KST expired=false
I0924 17:20:28.614522     231 auth.go:157] ChainedAuth: authenticated via keyring (effective: keyring)
I0924 17:20:28.614522     231 server_oauth.go:196] applyAuthResult: email=<email>, authMethod=consumer, quotaProject=
I0924 17:20:28.614522     231 server_oauth.go:201] OAuth: authenticated successfully as <email>
I0924 17:20:28.614522     231 server_oauth.go:207] b.codeAssistClient.AuthProvider (0x35cfc40fc0f0) is same as b.cliAuth (0x35cfc40fc0f0)
W0924 17:20:28.615030     239 cache.go:163] Failed to refresh cache in background: admin controls not applicable
I0924 17:20:28.613199     265 gemini_extensions.go:28] Detecting Gemini extensions in C:\Users\user\.gemini\extensions
I0924 17:20:28.615030     265 gemini_extensions.go:49] No extensions found
W0924 17:20:28.615030     204 cache.go:163] Failed to refresh cache in background: admin controls not applicable
I0924 17:20:28.616574       1 analytics.go:187] CLI startup completed (took 227.1451ms)
I0924 17:20:28.663151     342 manager.go:934] Full redraw completed (rerenderAll) for conversation  (epoch 0, items 1)
I0924 17:20:29.582022     238 http_helpers.go:305] URL: https://daily-cloudcode-pa.googleapis.com/v1internal:loadCodeAssist Trace: 0x1e5c99c8fa1790b0
I0924 17:20:30.023964     235 http_helpers.go:305] URL: https://daily-cloudcode-pa.googleapis.com/v1internal:fetchAvailableModels Trace: 0x524ece174cc9e1c6
W0924 17:20:30.061976     231 model_config_manager.go:67] Failed to resolve model flag "gemini-3.8-flash-high": --model gemini-3.8-flash-high conflicts with --effort=low
I0924 17:20:30.062976     231 model_config_manager.go:327] Propagating selected model override to backend: label="Gemini 3.8 Flash (High)"
I0924 17:20:30.063573     248 quota_manager.go:45] doRefreshQuota: starting reload (force=true)
I0924 17:20:30.063573     249 experiment_manager.go:66] Starting experiment refresh after login
I0924 17:20:30.480483     249 remote_agent.go:156] Remote agent fastpush pin gate changed: false -> true
I0924 17:20:30.481001     249 server.go:3769] [RemoteControl] Session toggle is off, staying disconnected
I0924 17:20:30.481001     249 server.go:3480] [RemoteControl] Resolved proxyServerURL: ""
I0924 17:20:30.481001     249 experiment_manager.go:70] Experiments refreshed after login
I0924 17:20:30.481001     332 manager.go:1308] Reloading system slash commands
I0924 17:20:31.485622     387 http_helpers.go:305] URL: https://daily-cloudcode-pa.googleapis.com/v1internal:loadCodeAssist Trace: 0xe68c0b26c4c162eb
I0924 17:20:32.802591     387 http_helpers.go:305] URL: https://daily-cloudcode-pa.googleapis.com/v1internal:loadCodeAssist Trace: 0x46f67b480998ff4b
W0924 17:20:33.025968     387 model_config_manager.go:67] Failed to resolve model flag "gemini-3.8-flash-high": --model gemini-3.8-flash-high conflicts with --effort=low
I0924 17:20:33.025968     387 model_config_manager.go:327] Propagating selected model override to backend: label="Gemini 3.8 Flash (High)"
I0924 17:20:33.025968     249 experiment_manager.go:66] Starting experiment refresh after login
I0924 17:20:33.025968     248 quota_manager.go:45] doRefreshQuota: starting reload (force=true)
W0924 17:20:33.025968     390 model_config_manager.go:67] Failed to resolve model flag "gemini-3.8-flash-high": --model gemini-3.8-flash-high conflicts with --effort=low
I0924 17:20:33.026470     390 model_config_manager.go:327] Propagating selected model override to backend: label="Gemini 3.8 Flash (High)"
I0924 17:20:33.155749     249 server.go:3769] [RemoteControl] Session toggle is off, staying disconnected
I0924 17:20:33.155749     249 server.go:3480] [RemoteControl] Resolved proxyServerURL: ""
I0924 17:20:33.155749     249 experiment_manager.go:70] Experiments refreshed after login
I0924 17:20:33.155749     249 experiment_manager.go:66] Starting experiment refresh after login
I0924 17:20:33.155749     422 manager.go:1308] Reloading system slash commands
I0924 17:20:33.161144     422 manager.go:1312] Slash commands unchanged, skipping update
I0924 17:20:34.044434     389 http_helpers.go:305] URL: https://daily-cloudcode-pa.googleapis.com/v1internal:loadCodeAssist Trace: 0x5dfcd4420938bfd
I0924 17:20:34.759982     248 quota_manager.go:45] doRefreshQuota: starting reload (force=true)
I0924 17:20:34.950075     249 server.go:3769] [RemoteControl] Session toggle is off, staying disconnected
I0924 17:20:34.950075     249 server.go:3480] [RemoteControl] Resolved proxyServerURL: ""
I0924 17:20:34.950075     249 experiment_manager.go:70] Experiments refreshed after login
I0924 17:20:34.950075     430 manager.go:1308] Reloading system slash commands
I0924 17:20:34.952107     430 manager.go:1312] Slash commands unchanged, skipping update
"#;

// session-IEKjtC (this machine, 2026-09-24 17:47, Agy 1.2.10; the round-3 build
// pasted at about 17:47:13.1 and lost the paste): the startup verbatim after the
// ANSI strip, with the account email redacted. The startup reload precedes `CLI
// startup completed`; three plain reloads follow within 6 s; the round-3 gate was
// ready at 17:47:13.12, 3.5 s after the last of them; and the deferred skills
// reload with its hooks line arrived at 17:47:13.468, 9.8 s after startup,
// clearing the composer. Only the two 17:53 lines were logged in the six minutes
// after it: no HandleUserInput receipt, so the adapter reported
// delivery-uncertain.
pub(super) const REAL_DEFERRED_RELOAD_STARTUP: &str = r#"I0924 17:47:03.616815     291 gemini_extensions.go:28] Detecting Gemini extensions in C:\Users\user\.gemini\extensions
I0924 17:47:03.616815     296 manager.go:1331] Reloading system slash commands and skills
I0924 17:47:03.617328     296 manager.go:1308] Reloading system slash commands
I0924 17:47:03.617328     296 manager.go:1312] Slash commands unchanged, skipping update
I0924 17:47:03.617328     291 gemini_extensions.go:49] No extensions found
I0924 17:47:03.617871     141 keyring.go:64] keyringAuth: loaded token, expiry=2026-09-24 18:30:33.2570659 +0900 KST expired=false
I0924 17:47:03.617871     140 auth.go:157] ChainedAuth: authenticated via keyring (effective: keyring)
I0924 17:47:03.617871     140 server_oauth.go:196] applyAuthResult: email=<email>, authMethod=consumer, quotaProject=
I0924 17:47:03.617871     140 server_oauth.go:201] OAuth: authenticated successfully as <email>
I0924 17:47:03.617871     140 server_oauth.go:207] b.codeAssistClient.AuthProvider (0x2246f4b581e0) is same as b.cliAuth (0x2246f4b581e0)
W0924 17:47:03.618378     327 cache.go:163] Failed to refresh cache in background: admin controls not applicable
W0924 17:47:03.618378     311 cache.go:163] Failed to refresh cache in background: admin controls not applicable
I0924 17:47:03.620082       1 analytics.go:187] CLI startup completed (took 223.5116ms)
I0924 17:47:03.666570     377 manager.go:934] Full redraw completed (rerenderAll) for conversation  (epoch 0, items 1)
I0924 17:47:05.152404     310 http_helpers.go:305] URL: https://daily-cloudcode-pa.googleapis.com/v1internal:loadCodeAssist Trace: 0xaf0c6d6cd1ce8304
I0924 17:47:05.675593     323 http_helpers.go:305] URL: https://daily-cloudcode-pa.googleapis.com/v1internal:fetchAvailableModels Trace: 0x4981a71d4a9d22c2
W0924 17:47:05.713908     140 model_config_manager.go:67] Failed to resolve model flag "gemini-3.8-flash-high": --model gemini-3.8-flash-high conflicts with --effort=low
I0924 17:47:05.713908     140 model_config_manager.go:327] Propagating selected model override to backend: label="Gemini 3.8 Flash (High)"
I0924 17:47:05.713908     253 quota_manager.go:45] doRefreshQuota: starting reload (force=true)
I0924 17:47:05.714422     254 experiment_manager.go:66] Starting experiment refresh after login
I0924 17:47:06.420977     254 remote_agent.go:156] Remote agent fastpush pin gate changed: false -> true
I0924 17:47:06.421536     254 server.go:3769] [RemoteControl] Session toggle is off, staying disconnected
I0924 17:47:06.421536     254 server.go:3480] [RemoteControl] Resolved proxyServerURL: ""
I0924 17:47:06.421536     254 experiment_manager.go:70] Experiments refreshed after login
I0924 17:47:06.421536     240 manager.go:1308] Reloading system slash commands
I0924 17:47:07.612954     409 http_helpers.go:305] URL: https://daily-cloudcode-pa.googleapis.com/v1internal:loadCodeAssist Trace: 0x873d189845330ceb
I0924 17:47:08.472485     409 http_helpers.go:305] URL: https://daily-cloudcode-pa.googleapis.com/v1internal:loadCodeAssist Trace: 0x25fb0d59725338a1
W0924 17:47:08.766467     409 model_config_manager.go:67] Failed to resolve model flag "gemini-3.8-flash-high": --model gemini-3.8-flash-high conflicts with --effort=low
I0924 17:47:08.766467     409 model_config_manager.go:327] Propagating selected model override to backend: label="Gemini 3.8 Flash (High)"
I0924 17:47:08.766467     254 experiment_manager.go:66] Starting experiment refresh after login
I0924 17:47:08.766467     253 quota_manager.go:45] doRefreshQuota: starting reload (force=true)
W0924 17:47:08.766467     433 model_config_manager.go:67] Failed to resolve model flag "gemini-3.8-flash-high": --model gemini-3.8-flash-high conflicts with --effort=low
I0924 17:47:08.766467     433 model_config_manager.go:327] Propagating selected model override to backend: label="Gemini 3.8 Flash (High)"
I0924 17:47:08.894558     254 server.go:3769] [RemoteControl] Session toggle is off, staying disconnected
I0924 17:47:08.894558     254 server.go:3480] [RemoteControl] Resolved proxyServerURL: ""
I0924 17:47:08.894558     254 experiment_manager.go:70] Experiments refreshed after login
I0924 17:47:08.894558     254 experiment_manager.go:66] Starting experiment refresh after login
I0924 17:47:08.894558     450 manager.go:1308] Reloading system slash commands
I0924 17:47:08.896639     450 manager.go:1312] Slash commands unchanged, skipping update
I0924 17:47:09.223158     253 quota_manager.go:45] doRefreshQuota: starting reload (force=true)
I0924 17:47:09.616209     254 server.go:3769] [RemoteControl] Session toggle is off, staying disconnected
I0924 17:47:09.616209     254 server.go:3480] [RemoteControl] Resolved proxyServerURL: ""
I0924 17:47:09.616209     254 experiment_manager.go:70] Experiments refreshed after login
I0924 17:47:09.616209     303 manager.go:1308] Reloading system slash commands
I0924 17:47:09.618298     303 manager.go:1312] Slash commands unchanged, skipping update
I0924 17:47:09.685316     432 http_helpers.go:305] URL: https://daily-cloudcode-pa.googleapis.com/v1internal:loadCodeAssist Trace: 0x83274a231aa9a1d5
I0924 17:47:13.468286     399 manager.go:1331] Reloading system slash commands and skills
I0924 17:47:13.468286     399 manager.go:1308] Reloading system slash commands
I0924 17:47:13.468806     478 hooks_manager.go:53] loaded 0 named hooks from 0 hooks.json file(s)
I0924 17:47:13.470456     399 manager.go:1312] Slash commands unchanged, skipping update
I0924 17:53:04.130429     656 http_helpers.go:305] URL: https://daily-cloudcode-pa.googleapis.com/v1internal:fetchAvailableModels Trace: 0xebc1b272b39c997d
I0924 17:53:04.677160     651 http_helpers.go:305] URL: https://daily-cloudcode-pa.googleapis.com/v1internal:loadCodeAssist Trace: 0xbcc1674415841087
"#;

// session-uqraap (this machine, 2026-09-24 20:44, Agy 1.2.10, CPU idle, a second
// Agy launch overlapping; the round-8 build pasted at about 20:44:36.5, when its
// 35 s window ended, and lost the paste): lines 96-149 (the end of the file)
// verbatim after the ANSI strip, with the account email redacted. The startup
// reload precedes `CLI startup completed`; two plain reloads follow within 5 s;
// the quiet period ended at 20:44:09.93; the 35 s window ended at 20:44:36.44
// and the gate pasted; and the deferred skills reload with its hooks line
// arrived at 20:44:37.855815, 36.4 s after startup, clearing the composer.
// session-Cf1FBY (20:36, heavy CPU load) logged the same reload at +36.4 s and
// lost its paste the same way. Nothing was logged after it: no HandleUserInput
// receipt, so the adapter reported delivery-uncertain.
pub(super) const REAL_SECOND_CLUSTER_RELOAD_STARTUP: &str = r#"I0924 20:44:01.434843       1 common.go:438] Starting CLI program
CLI ready for user input
W0924 20:44:01.434843     187 cache.go:135] Cache(loadCodeAssistResponse): Singleflight refresh failed: error getting token source: You are not logged into Antigravity.
E0924 20:44:01.434843     187 errorreport.go:224] error getting token source: You are not logged into Antigravity.
W0924 20:44:01.434843     187 cache.go:135] Cache(userInfo): Singleflight refresh failed: failed to get load code assist response: error getting token source: You are not logged into Antigravity.
E0924 20:44:01.434843     187 errorreport.go:224] failed to get load code assist response: error getting token source: You are not logged into Antigravity.
W0924 20:44:01.434843     187 cache.go:135] Cache(loadCodeAssistResponse): Singleflight refresh failed: error getting token source: You are not logged into Antigravity.
E0924 20:44:01.434843     187 errorreport.go:224] error getting token source: You are not logged into Antigravity.
W0924 20:44:01.434843     187 cache.go:135] Cache(userInfo): Singleflight refresh failed: failed to get load code assist response: error getting token source: You are not logged into Antigravity.
E0924 20:44:01.434843     187 errorreport.go:224] failed to get load code assist response: error getting token source: You are not logged into Antigravity.
I0924 20:44:01.441059     296 manager.go:1331] Reloading system slash commands and skills
I0924 20:44:01.441059     296 manager.go:1308] Reloading system slash commands
I0924 20:44:01.441059     296 manager.go:1312] Slash commands unchanged, skipping update
I0924 20:44:01.441059     291 gemini_extensions.go:28] Detecting Gemini extensions in C:\Users\user\.gemini\extensions
I0924 20:44:01.441567     291 gemini_extensions.go:49] No extensions found
I0924 20:44:01.442107      75 keyring.go:64] keyringAuth: loaded token, expiry=2026-09-24 21:30:53.5504966 +0900 KST expired=false
I0924 20:44:01.442107      74 auth.go:157] ChainedAuth: authenticated via keyring (effective: keyring)
I0924 20:44:01.442107      74 server_oauth.go:196] applyAuthResult: email=<email>, authMethod=consumer, quotaProject=
I0924 20:44:01.442107      74 server_oauth.go:201] OAuth: authenticated successfully as <email>
I0924 20:44:01.442107      74 server_oauth.go:207] b.codeAssistClient.AuthProvider (0x2a7b00ba00f0) is same as b.cliAuth (0x2a7b00ba00f0)
W0924 20:44:01.442107     322 cache.go:163] Failed to refresh cache in background: admin controls not applicable
W0924 20:44:01.442107     343 cache.go:163] Failed to refresh cache in background: admin controls not applicable
I0924 20:44:01.443214       1 analytics.go:187] CLI startup completed (took 233.4541ms)
I0924 20:44:01.490907     375 manager.go:934] Full redraw completed (rerenderAll) for conversation  (epoch 0, items 1)
I0924 20:44:03.314578      81 http_helpers.go:305] URL: https://daily-cloudcode-pa.googleapis.com/v1internal:loadCodeAssist Trace: 0x64962e4d65df3a83
I0924 20:44:03.945369      78 http_helpers.go:305] URL: https://daily-cloudcode-pa.googleapis.com/v1internal:fetchAvailableModels Trace: 0x18e64b39b3fcc68a
I0924 20:44:03.987146      74 model_resolver.go:93] Resolving model gemini-3.8-flash-high
I0924 20:44:03.987146      74 model_config_manager.go:327] Propagating selected model override to backend: label="Gemini 3.8 Flash (High)"
I0924 20:44:03.987146     245 quota_manager.go:45] doRefreshQuota: starting reload (force=true)
I0924 20:44:03.987146     246 experiment_manager.go:66] Starting experiment refresh after login
I0924 20:44:04.176021     246 remote_agent.go:156] Remote agent fastpush pin gate changed: false -> true
I0924 20:44:04.176021     246 server.go:3769] [RemoteControl] Session toggle is off, staying disconnected
I0924 20:44:04.176021     246 server.go:3480] [RemoteControl] Resolved proxyServerURL: ""
I0924 20:44:04.176021     246 experiment_manager.go:70] Experiments refreshed after login
I0924 20:44:04.176592     429 manager.go:1308] Reloading system slash commands
I0924 20:44:05.022530     422 http_helpers.go:305] URL: https://daily-cloudcode-pa.googleapis.com/v1internal:loadCodeAssist Trace: 0xe8d8c908fb469f5d
I0924 20:44:06.141949     422 http_helpers.go:305] URL: https://daily-cloudcode-pa.googleapis.com/v1internal:loadCodeAssist Trace: 0xf0c9ea8cbfd8e96a
I0924 20:44:06.342958     422 model_resolver.go:93] Resolving model gemini-3.8-flash-high
I0924 20:44:06.342958     422 model_config_manager.go:327] Propagating selected model override to backend: label="Gemini 3.8 Flash (High)"
I0924 20:44:06.342958     245 quota_manager.go:45] doRefreshQuota: starting reload (force=true)
I0924 20:44:06.342958     439 model_resolver.go:93] Resolving model gemini-3.8-flash-high
I0924 20:44:06.342958     439 model_config_manager.go:327] Propagating selected model override to backend: label="Gemini 3.8 Flash (High)"
I0924 20:44:06.342958     246 experiment_manager.go:66] Starting experiment refresh after login
I0924 20:44:06.427564     246 server.go:3769] [RemoteControl] Session toggle is off, staying disconnected
I0924 20:44:06.427564     246 server.go:3480] [RemoteControl] Resolved proxyServerURL: ""
I0924 20:44:06.427564     246 experiment_manager.go:70] Experiments refreshed after login
I0924 20:44:06.427564     445 manager.go:1308] Reloading system slash commands
I0924 20:44:06.428584     445 manager.go:1312] Slash commands unchanged, skipping update
I0924 20:44:06.643486     245 quota_manager.go:45] doRefreshQuota: starting reload (force=true)
I0924 20:44:07.314437     438 http_helpers.go:305] URL: https://daily-cloudcode-pa.googleapis.com/v1internal:loadCodeAssist Trace: 0xcb0bf3ebb06517e2
I0924 20:44:37.855815     516 manager.go:1331] Reloading system slash commands and skills
I0924 20:44:37.855815     516 manager.go:1308] Reloading system slash commands
I0924 20:44:37.856320     355 hooks_manager.go:53] loaded 0 named hooks from 0 hooks.json file(s)
I0924 20:44:37.857983     516 manager.go:1312] Slash commands unchanged, skipping update
"#;

// session-M8QFPp (this machine, 2026-09-24 20:37, Agy 1.2.10, heavy CPU load;
// the paste was accepted but reported delivery-uncertain): lines 123-157
// verbatim after the ANSI strip, with the account email redacted. The startup
// reload follows `CLI startup completed` by 1.6 ms with its hooks line, and the
// round-8 rule took it for the deferred reload, so the gate was ready 3.5 s
// after the last plain reload at 20:37:45.65 and pasted at about 20:37:49.15.
// The HandleUserInput receipt (line 160, not reproduced) was logged at
// 20:38:07.152165, 18.0 s after the paste and outside the former 15 s receipt
// window. No deferred skills reload was logged before it; the skills reload at
// 20:38:07.160926 is the conversation reload.
pub(super) const REAL_STARTUP_RELOAD_AFTER_STARTUP: &str = r#"I0924 20:37:40.653251       1 analytics.go:187] CLI startup completed (took 241.0837ms)
I0924 20:37:40.654897     338 manager.go:1331] Reloading system slash commands and skills
I0924 20:37:40.654897     338 manager.go:1308] Reloading system slash commands
I0924 20:37:40.656148      96 hooks_manager.go:53] loaded 0 named hooks from 0 hooks.json file(s)
I0924 20:37:40.700886     142 manager.go:934] Full redraw completed (rerenderAll) for conversation  (epoch 0, items 1)
I0924 20:37:42.065160     297 http_helpers.go:305] URL: https://daily-cloudcode-pa.googleapis.com/v1internal:loadCodeAssist Trace: 0x4e9df0103680b301
I0924 20:37:42.456755     297 http_helpers.go:305] URL: https://daily-cloudcode-pa.googleapis.com/v1internal:fetchAvailableModels Trace: 0x17d08bd806d2a0c2
I0924 20:37:42.496252     293 model_resolver.go:93] Resolving model gemini-3.8-flash-high
I0924 20:37:42.496252     293 model_config_manager.go:327] Propagating selected model override to backend: label="Gemini 3.8 Flash (High)"
I0924 20:37:42.496252     252 quota_manager.go:45] doRefreshQuota: starting reload (force=true)
I0924 20:37:42.496252     253 experiment_manager.go:66] Starting experiment refresh after login
I0924 20:37:42.678109     253 remote_agent.go:156] Remote agent fastpush pin gate changed: false -> true
I0924 20:37:42.678109     253 server.go:3769] [RemoteControl] Session toggle is off, staying disconnected
I0924 20:37:42.678109     253 server.go:3480] [RemoteControl] Resolved proxyServerURL: ""
I0924 20:37:42.678109     253 experiment_manager.go:70] Experiments refreshed after login
I0924 20:37:42.678621     411 manager.go:1308] Reloading system slash commands
I0924 20:37:43.588140     288 http_helpers.go:305] URL: https://daily-cloudcode-pa.googleapis.com/v1internal:loadCodeAssist Trace: 0xab7203296c667660
I0924 20:37:45.048591     288 http_helpers.go:305] URL: https://daily-cloudcode-pa.googleapis.com/v1internal:loadCodeAssist Trace: 0xe1422278db03c287
I0924 20:37:45.218409     288 model_resolver.go:93] Resolving model gemini-3.8-flash-high
I0924 20:37:45.218409     288 model_config_manager.go:327] Propagating selected model override to backend: label="Gemini 3.8 Flash (High)"
I0924 20:37:45.218409     252 quota_manager.go:45] doRefreshQuota: starting reload (force=true)
I0924 20:37:45.218409     253 experiment_manager.go:66] Starting experiment refresh after login
I0924 20:37:45.218409     483 model_resolver.go:93] Resolving model gemini-3.8-flash-high
I0924 20:37:45.218409     483 model_config_manager.go:327] Propagating selected model override to backend: label="Gemini 3.8 Flash (High)"
I0924 20:37:45.515277     253 server.go:3769] [RemoteControl] Session toggle is off, staying disconnected
I0924 20:37:45.515277     253 server.go:3480] [RemoteControl] Resolved proxyServerURL: ""
I0924 20:37:45.515277     253 experiment_manager.go:70] Experiments refreshed after login
I0924 20:37:45.515277     253 experiment_manager.go:66] Starting experiment refresh after login
I0924 20:37:45.515277     417 manager.go:1308] Reloading system slash commands
I0924 20:37:45.517352     417 manager.go:1312] Slash commands unchanged, skipping update
I0924 20:37:45.646941     253 server.go:3769] [RemoteControl] Session toggle is off, staying disconnected
I0924 20:37:45.646941     253 server.go:3480] [RemoteControl] Resolved proxyServerURL: ""
I0924 20:37:45.646941     253 experiment_manager.go:70] Experiments refreshed after login
I0924 20:37:45.646941     498 manager.go:1308] Reloading system slash commands
I0924 20:37:45.649546     498 manager.go:1312] Slash commands unchanged, skipping update
I0924 20:37:45.892979     252 quota_manager.go:45] doRefreshQuota: starting reload (force=true)
I0924 20:37:46.098266     482 http_helpers.go:305] URL: https://daily-cloudcode-pa.googleapis.com/v1internal:loadCodeAssist Trace: 0x7c00680e8820122f
"#;

// session-QMFk6F (this Mac, macOS 26.6 + iTerm2, 2026-09-24 21:32, Agy 1.2.10; the
// live smoke lost its first `tell`): the startup and the argument-delivered initial
// turn verbatim from line 100 of agy.log, with the account email redacted and the
// HTTP trace and latency-breakdown lines omitted. Agy logs no `Full redraw
// completed` line at all on macOS. The startup reload precedes `CLI startup
// completed`; `Starting new conversation` (+2.9 s) is followed 7 ms later by the
// conversation reload, which the former rule took for the deferred reload; the
// trust reload and its hooks line come at +11.2 s. The smoke pasted its
// follow-up at 21:32:18.79 (+10.3 s) onto the workspace-trust dialog; its Enter
// approved the folder, Agy logged that reload 0.85 s later, and no
// `HandleUserInput` receipt followed.
pub(super) const REAL_MACOS_INITIAL_TURN: &str = r#"E0924 21:32:08.448630     204 errorreport.go:224] failed to get load code assist response: error getting token source: You are not logged into Antigravity.
W0924 21:32:08.448718     204 cache.go:135] Cache(loadCodeAssistResponse): Singleflight refresh failed: error getting token source: You are not logged into Antigravity.
E0924 21:32:08.448740     204 errorreport.go:224] error getting token source: You are not logged into Antigravity.
W0924 21:32:08.448803     204 cache.go:135] Cache(userInfo): Singleflight refresh failed: failed to get load code assist response: error getting token source: You are not logged into Antigravity.
E0924 21:32:08.448822     204 errorreport.go:224] failed to get load code assist response: error getting token source: You are not logged into Antigravity.
I0924 21:32:08.452158     261 manager.go:1331] Reloading system slash commands and skills
I0924 21:32:08.452191     261 manager.go:1308] Reloading system slash commands
I0924 21:32:08.452164     128 gemini_extensions.go:28] Detecting Gemini extensions in /Users/tester/.gemini/extensions
I0924 21:32:08.452226     128 gemini_extensions.go:49] No extensions found
I0924 21:32:08.452205     261 manager.go:1312] Slash commands unchanged, skipping update
I0924 21:32:08.458864     258 encoder_embed.go:85] Installing/updating embedded webm_encoder binary to /Users/tester/.gemini/antigravity-cli/bin/webm_encoder
I0924 21:32:08.459241       1 analytics.go:187] CLI startup completed (took 285.555917ms)
W0924 21:32:08.461411     204 cache.go:135] Cache(loadCodeAssistResponse): Singleflight refresh failed: error getting token source: You are not logged into Antigravity.
E0924 21:32:08.461511     204 errorreport.go:224] error getting token source: You are not logged into Antigravity.
W0924 21:32:08.461629     204 cache.go:135] Cache(userInfo): Singleflight refresh failed: failed to get load code assist response: error getting token source: You are not logged into Antigravity.
E0924 21:32:08.461662     204 errorreport.go:224] failed to get load code assist response: error getting token source: You are not logged into Antigravity.
I0924 21:32:08.478694     264 keyring.go:64] keyringAuth: loaded token, expiry=2026-09-24 22:02:58.773153 +0900 KST expired=false
I0924 21:32:08.666372     263 auth.go:157] ChainedAuth: authenticated via keyring (effective: keyring)
I0924 21:32:08.666556     263 server_oauth.go:196] applyAuthResult: email=<email>, authMethod=consumer, quotaProject=
I0924 21:32:08.666624     263 server_oauth.go:201] OAuth: authenticated successfully as <email>
I0924 21:32:08.666650     263 server_oauth.go:207] b.codeAssistClient.AuthProvider (0x2ea62b5ca0f0) is same as b.cliAuth (0x2ea62b5ca0f0)
W0924 21:32:08.667313      62 cache.go:163] Failed to refresh cache in background: admin controls not applicable
W0924 21:32:08.667432     340 cache.go:163] Failed to refresh cache in background: admin controls not applicable
I0924 21:32:11.027320     263 model_resolver.go:93] Resolving model gemini-3.8-flash-high
I0924 21:32:11.027444     263 model_config_manager.go:327] Propagating selected model override to backend: label="Gemini 3.8 Flash (High)"
I0924 21:32:11.027734     215 experiment_manager.go:66] Starting experiment refresh after login
I0924 21:32:11.027741     214 quota_manager.go:45] doRefreshQuota: starting reload (force=true)
I0924 21:32:11.355480     215 remote_agent.go:156] Remote agent fastpush pin gate changed: false -> true
I0924 21:32:11.355712     215 server.go:3769] [RemoteControl] Session toggle is off, staying disconnected
I0924 21:32:11.355726     215 server.go:3480] [RemoteControl] Resolved proxyServerURL: ""
I0924 21:32:11.355735     215 experiment_manager.go:70] Experiments refreshed after login
I0924 21:32:11.355788     321 manager.go:1308] Reloading system slash commands
I0924 21:32:11.355776     337 conversation_manager.go:512] Starting new conversation (agent=false)
I0924 21:32:11.355858     337 server.go:1204] Creating new cascade trajectory (agentScript=false)
I0924 21:32:11.355870     337 server.go:1207] Conversation using project ID: default-cli-project
I0924 21:32:11.361291     337 server.go:1239] Created conversation 087c73bd-a900-4e13-9231-183e7a901038
I0924 21:32:11.361349     337 server.go:3211] GetConversationDetail: found conversation 087c73bd-a900-4e13-9231-183e7a901038 (active=true)
I0924 21:32:11.362358     337 server.go:3211] GetConversationDetail: found conversation 087c73bd-a900-4e13-9231-183e7a901038 (active=true)
I0924 21:32:11.362421     337 conversation_manager.go:559] project: switching to conversation belonging to project ID: default-cli-project
I0924 21:32:11.362516     337 server.go:2211] Backend project ID updated dynamically to: default-cli-project
I0924 21:32:11.362528     337 cli_setting_manager.go:218] ApplyProjectPermissionGrants: no grants for project "CLI Project", cleared project permissions
I0924 21:32:11.362540     337 conversation_manager.go:605] project: synced active project to "CLI Project" (id=default-cli-project) from conversation switch
I0924 21:32:11.362551     337 conversation_manager.go:887] Streaming conversation 087c73bd-a900-4e13-9231-183e7a901038
I0924 21:32:11.362602     337 server.go:3211] GetConversationDetail: found conversation 087c73bd-a900-4e13-9231-183e7a901038 (active=true)
I0924 21:32:11.362782     423 manager.go:1331] Reloading system slash commands and skills
I0924 21:32:11.362834     423 manager.go:1308] Reloading system slash commands
I0924 21:32:11.363063     337 server.go:1248] Starting conversation update stream for 087c73bd-a900-4e13-9231-183e7a901038
I0924 21:32:11.363493     424 manager.go:538] Ignoring IDLE update because we are waiting for RUNNING
I0924 21:32:11.368788     423 manager.go:1312] Slash commands unchanged, skipping update
I0924 21:32:14.526713     352 model_resolver.go:93] Resolving model gemini-3.8-flash-high
I0924 21:32:14.526830     352 model_config_manager.go:327] Propagating selected model override to backend: label="Gemini 3.8 Flash (High)"
I0924 21:32:14.527039      76 model_resolver.go:93] Resolving model gemini-3.8-flash-high
I0924 21:32:14.527148      76 model_config_manager.go:327] Propagating selected model override to backend: label="Gemini 3.8 Flash (High)"
I0924 21:32:14.527201     215 experiment_manager.go:66] Starting experiment refresh after login
I0924 21:32:14.527908     214 quota_manager.go:45] doRefreshQuota: starting reload (force=true)
I0924 21:32:14.538154     337 conversation_manager.go:699] Forwarding user message to conversation 087c73bd-a900-4e13-9231-183e7a901038 (items=1, media=0)
I0924 21:32:14.538352     337 server.go:1846] Sending user message to conversation 087c73bd-a900-4e13-9231-183e7a901038 (items=1, media=0)
I0924 21:32:14.562896     466 monitor_config_manager_external.go:53] externalMonitorConfigManager: initialized with embedded config (2 monitors)
I0924 21:32:14.563249     466 monitoring.go:318] [Sonar] Resolved active tags: [policy_guardian] (detected surface: "")
I0924 21:32:14.563357     466 monitoring.go:318] [Sonar] Resolved active tags: [policy_guardian] (detected surface: "")
W0924 21:32:14.563381     466 declarative_config_loader.go:272] skipping component during resolution: empty component: prompt section "mcp_servers"
W0924 21:32:14.563439     466 declarative_config_loader.go:272] skipping component during resolution: empty component: prompt section "user_rules"
W0924 21:32:14.563451     466 declarative_config_loader.go:272] skipping component during resolution: empty component: prompt section "subagent_reminder"
W0924 21:32:14.563462     466 declarative_config_loader.go:272] skipping component during resolution: empty component: prompt section "terminal_sandbox"
W0924 21:32:14.563491     466 declarative_config_loader.go:272] skipping component during resolution: empty component: pre-tool hook "command_assessor" is empty
W0924 21:32:14.563508     466 declarative_config_loader.go:272] skipping component during resolution: empty component: post-tool hook "command_assessor" is empty
I0924 21:32:14.738775     215 server.go:3769] [RemoteControl] Session toggle is off, staying disconnected
I0924 21:32:14.738929     215 server.go:3480] [RemoteControl] Resolved proxyServerURL: ""
I0924 21:32:14.738961     215 experiment_manager.go:70] Experiments refreshed after login
I0924 21:32:14.739083     654 manager.go:1308] Reloading system slash commands
I0924 21:32:14.748952     654 manager.go:1312] Slash commands unchanged, skipping update
I0924 21:32:18.266684     214 quota_manager.go:41] doRefreshQuota: skipped (throttled)
I0924 21:32:19.637013     623 manager.go:1331] Reloading system slash commands and skills
I0924 21:32:19.637228     623 manager.go:1308] Reloading system slash commands
I0924 21:32:19.637963     616 hooks_manager.go:53] loaded 0 named hooks from 0 hooks.json file(s)
I0924 21:32:19.644263     623 manager.go:1312] Slash commands unchanged, skipping update
"#;

pub(super) fn glog(time: &str, thread: u32, source: &str, message: &str) -> String {
    format!("I0924 {time} {thread:>7} {source}] {message}\n")
}

pub(super) const HOOKS_LOADED: &str = "loaded 0 named hooks from 0 hooks.json file(s)";

pub(super) fn successful_startup_log() -> String {
    REAL_SUCCESS_STARTUP.to_owned()
}

pub(super) fn late_reload_startup_log() -> String {
    REAL_FAILURE_STARTUP.to_owned()
}

pub(super) fn late_reload_completion() -> String {
    REAL_FAILURE_LATE_RELOAD.to_owned()
}

// session-fMqSQc with its deferred reload logged: the deferred-reload condition
// is settled, so the gate is ready one quiet period after the newest activity
// line (the reload's hooks line). The tests about the quiet period, continuity,
// and the paste offset use this shape, which is ready long before the window.
pub(super) fn settled_startup_log() -> String {
    late_reload_startup_log() + &late_reload_completion()
}

// Go `%q` formatting as observed in the HandleUserInput lines.
pub(super) fn go_quoted(text: &str) -> String {
    let mut quoted = String::from("\"");
    for character in text.chars() {
        match character {
            '"' => quoted.push_str("\\\""),
            '\\' => quoted.push_str("\\\\"),
            '\n' => quoted.push_str("\\n"),
            '\t' => quoted.push_str("\\t"),
            _ => quoted.push(character),
        }
    }
    quoted.push('"');
    quoted
}

pub(super) fn receipt_line(quoted: &str) -> String {
    format!(
        "I0924 16:42:50.212542     595 input_loop.go:107] HandleUserInput called with text: {quoted}\n"
    )
}

pub(super) fn framed_receipt_line(prompt: &str, pending: &PendingAgyTurn) -> String {
    let framed = terminal_correlated_prompt(prompt, pending, true).unwrap();
    receipt_line(&go_quoted(&framed))
}

pub(super) use crate::native::consent::FakeClock;

pub(super) fn log_sequence(
    logs: Vec<Result<Option<Vec<u8>>>>,
) -> impl FnMut() -> Result<Option<Vec<u8>>> {
    let mut logs = std::collections::VecDeque::from(logs);
    move || match logs.len() {
        0 => panic!("the log sequence was exhausted"),
        1 => match logs.front().unwrap() {
            Ok(log) => Ok(log.clone()),
            Err(error) => Err(anyhow::anyhow!("{error:#}")),
        },
        _ => logs.pop_front().unwrap(),
    }
}

pub(super) fn some_log(text: &str) -> Result<Option<Vec<u8>>> {
    Ok(Some(text.as_bytes().to_vec()))
}

// Issue #48, reproduced 2026-10-01 with Agy 1.2.14 (session-U2yPxX, macOS, iTerm2):
// the workspace was not in Agy's trust store, the dialog covered the composer, and
// the follow-up was pasted onto it. These four lines are everything Agy logged
// afterwards, verbatim: the trust reload, 356 bytes, and no receipt. That is the
// exact failure text of the issue, and it can only end delivery-uncertain.
pub(super) const REAL_TRUST_RELOAD_AFTER_PASTE: &str = r"I1001 16:29:08.527232     958 manager.go:1333] Reloading system slash commands and skills
I1001 16:29:08.527267     958 manager.go:1310] Reloading system slash commands
I1001 16:29:08.528761     954 hooks_manager.go:53] loaded 1 named hooks from 1 hooks.json file(s)
I1001 16:29:08.529657     958 manager.go:1314] Slash commands unchanged, skipping update
";
