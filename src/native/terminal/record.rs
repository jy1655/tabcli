//! Surface records retain their historical flat wire format. Kind-specific fields are
//! private here: adapters construct and read their own identity without spelling another
//! adapter's absent fields. Parsing remains structural, not proof of control authority;
//! even mixed-kind and incomplete legacy records retain every recognized field for equality.
use super::{LinuxBirth, TerminalKind, WezTermMux, WindowsProcessIdentity};
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

const fn legacy_iterm2_kind() -> TerminalKind {
    TerminalKind::Iterm2
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(in crate::native) struct TerminalSession {
    #[serde(default = "legacy_iterm2_kind", rename = "terminal")]
    pub(in crate::native) kind: TerminalKind,
    #[serde(alias = "iterm_session_id", rename = "session_id")]
    pub(in crate::native) id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(in crate::native) tab_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(in crate::native) window_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(in crate::native) managed_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    windows_process_identity: Option<WindowsProcessIdentity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    wezterm_mux: Option<WezTermMux>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    linux_host: Option<LinuxHostIdentity>,
    /// The socket of a tmux surface's own server, in its session directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tmux_socket: Option<String>,
    /// The Herdr Core that holds a Herdr surface and the program it started there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    herdr_core: Option<HerdrSurface>,
    // Older readers accept new top-level fields. Their flattened Herdr birth is strict.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    herdr_launch_nonce: Option<String>,
}

/// A Herdr terminal's creation identity: the core that answered on `socket` with its
/// permanent `core_id` (the address `t7` names one terminal forever on that core), when the
/// core created it, and the incarnation of the program it started, TAB's launch host.
/// Only the Herdr adapter constructs it from a creation it verified.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(in crate::native) struct HerdrSurface {
    pub(in crate::native) socket: String,
    pub(in crate::native) core_id: String,
    pub(in crate::native) created_ms: u64,
    pub(in crate::native) host_pid: u32,
    #[serde(flatten)]
    pub(in crate::native) birth: HerdrHostBirth,
}

// Keep the macOS wire fields written since 0.4.45. Linux ticks identify a
// process only on their own boot. A Windows creation time (100 ns since 1601, from
// `GetProcessTimes`) identifies the process holding its PID: Windows reuses a PID only
// after every handle to the earlier process is closed, and a later process starts later.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(untagged, deny_unknown_fields)]
pub(in crate::native) enum HerdrHostBirth {
    Macos {
        host_start_seconds: u64,
        host_start_microseconds: u64,
    },
    Linux {
        linux_birth: LinuxBirth,
    },
    Windows {
        host_creation_time: u64,
    },
}

/// Only a creation-proven adapter may publish this incarnation. Registration's
/// BridgeCreatedHost evidence grants authority over this whole host, independent of
/// owner/close intent. Endpoint reachability is deliberately not part of birth.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(in crate::native) struct LinuxHostIdentity {
    pub(in crate::native) pid: u32,
    pub(in crate::native) linux_birth: LinuxBirth,
}

impl TerminalSession {
    #[cfg(any(target_os = "macos", target_os = "linux", windows, test))]
    pub(in crate::native) fn new(kind: TerminalKind, id: String) -> Self {
        Self {
            kind,
            id,
            tab_id: None,
            window_id: None,
            managed_session_id: None,
            windows_process_identity: None,
            wezterm_mux: None,
            linux_host: None,
            tmux_socket: None,
            herdr_core: None,
            herdr_launch_nonce: None,
        }
    }

    #[cfg(any(target_os = "macos", target_os = "linux", test))]
    pub(in crate::native) fn with_location(
        mut self,
        tab: Option<String>,
        window: Option<String>,
    ) -> Self {
        self.tab_id = tab;
        self.window_id = window;
        self
    }

    #[cfg(test)]
    pub(in crate::native) fn with_binding(mut self, id: Option<String>) -> Self {
        self.managed_session_id = id;
        self
    }

    #[cfg(any(windows, test))]
    pub(in crate::native) fn with_windows_identity(
        mut self,
        identity: Option<WindowsProcessIdentity>,
    ) -> Self {
        self.windows_process_identity = identity;
        self
    }

    #[cfg(windows)]
    pub(in crate::native) fn windows_identity(&self) -> Option<&WindowsProcessIdentity> {
        self.windows_process_identity.as_ref()
    }

    #[cfg(any(target_os = "macos", test))]
    pub(in crate::native) fn with_wezterm_mux(mut self, mux: Option<WezTermMux>) -> Self {
        self.wezterm_mux = mux;
        self
    }

    #[cfg(any(target_os = "macos", test))]
    pub(in crate::native) fn wezterm_mux(&self) -> Option<&WezTermMux> {
        self.wezterm_mux.as_ref()
    }

    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    pub(in crate::native) fn with_herdr_core(
        mut self,
        core: HerdrSurface,
        launch_nonce: &str,
    ) -> Self {
        self.herdr_core = Some(core);
        self.herdr_launch_nonce = Some(launch_nonce.to_owned());
        self
    }

    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    pub(in crate::native) fn herdr_core(&self) -> Option<&HerdrSurface> {
        self.herdr_core.as_ref()
    }

    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    pub(in crate::native::terminal) fn herdr_launch_nonce(&self) -> Option<&str> {
        self.herdr_launch_nonce.as_deref()
    }

    #[cfg(target_os = "linux")]
    pub(in crate::native::terminal) fn with_tmux_server(
        mut self,
        socket: String,
        host: LinuxHostIdentity,
    ) -> Self {
        self.tmux_socket = Some(socket);
        self.linux_host = Some(host);
        self
    }

    // Close must also work for a retained server that never reported its pane/location.
    #[cfg(target_os = "linux")]
    pub(in crate::native::terminal) fn tmux_server(&self) -> Option<&LinuxHostIdentity> {
        self.linux_host.as_ref()
    }

    #[cfg(target_os = "linux")]
    pub(in crate::native::terminal) fn tmux_socket(&self) -> Option<&str> {
        self.tmux_socket.as_deref()
    }

    pub(in crate::native) fn verify_managed_session(&self, expected: &str) -> Result<()> {
        match self.managed_session_id.as_deref() {
            Some(actual) if actual == expected => Ok(()),
            Some(actual) => {
                bail!("terminal handle belongs to managed session {actual}, not {expected}")
            }
            None if self.kind == TerminalKind::Iterm2 => Ok(()),
            None => bail!("terminal handle is missing its managed session binding"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn mixed_record() -> Value {
        json!({
            "terminal": "tmux", "session_id": "%0", "tab_id": "@0", "window_id": "$0",
            "managed_session_id": "session-record",
            "windows_process_identity": {"creation_time": 7, "executable_path": "provider.exe"},
            "wezterm_mux": {"socket": "/mux", "pid": 8, "start_seconds": 9,
                            "start_microseconds": 10, "owns_gui": false},
            "linux_host": {"pid": 11, "linux_birth": {
                "boot_id": "00112233-4455-6677-8899-aabbccddeeff", "starttime_ticks": 12}},
            "tmux_socket": "/session-record/tmux.sock",
            "herdr_launch_nonce": "0123456789abcdef0123456789abcdef",
            "herdr_core": {"socket": "/state/default.sock", "core_id": "core", "created_ms": 13,
                           "host_pid": 14, "host_start_seconds": 15, "host_start_microseconds": 16}
        })
    }

    #[test]
    fn recognized_fields_round_trip_even_on_a_different_kind_and_affect_equality() {
        for kind in TerminalKind::ALL {
            let mut wire = mixed_record();
            wire["terminal"] = serde_json::to_value(kind).unwrap();
            let record: TerminalSession = serde_json::from_value(wire.clone()).unwrap();
            assert_eq!(serde_json::to_value(&record).unwrap(), wire);
            // A decoder must not discard unrelated kind fields: equality is used by
            // close intents and screen's before/after binding comparison.
            for field in [
                "tab_id",
                "window_id",
                "managed_session_id",
                "windows_process_identity",
                "wezterm_mux",
                "linux_host",
                "tmux_socket",
                "herdr_core",
                "herdr_launch_nonce",
            ] {
                let mut changed = wire.clone();
                changed.as_object_mut().unwrap().remove(field);
                let other: TerminalSession = serde_json::from_value(changed).unwrap();
                assert_ne!(record, other, "{kind:?}: {field}");
            }
            record.verify_managed_session("session-record").unwrap();
            assert!(record.verify_managed_session("session-other").is_err());
        }
    }

    #[test]
    fn legacy_alias_nulls_and_incomplete_server_keep_their_wire_and_binding_rules() {
        let legacy: TerminalSession = serde_json::from_value(json!({
            "iterm_session_id": "legacy", "tab_id": null, "linux_host": null
        }))
        .unwrap();
        assert_eq!(
            serde_json::to_value(&legacy).unwrap(),
            json!({
                "terminal": "iterm2", "session_id": "legacy"
            })
        );
        legacy.verify_managed_session("session-record").unwrap();
        assert!(
            serde_json::from_value::<TerminalSession>(json!({
                "session_id": "one", "iterm_session_id": "two"
            }))
            .is_err()
        );
        let mut wire = mixed_record();
        wire["session_id"] = json!("");
        for field in [
            "tab_id",
            "window_id",
            "windows_process_identity",
            "wezterm_mux",
        ] {
            wire.as_object_mut().unwrap().remove(field);
        }
        let retained: TerminalSession = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(serde_json::to_value(&retained).unwrap(), wire);
        retained.verify_managed_session("session-record").unwrap();
        wire.as_object_mut().unwrap().remove("managed_session_id");
        let unbound: TerminalSession = serde_json::from_value(wire).unwrap();
        assert!(unbound.verify_managed_session("session-record").is_err());
        for field in [
            "windows_process_identity",
            "wezterm_mux",
            "linux_host",
            "tmux_socket",
            "herdr_core",
        ] {
            let mut malformed = mixed_record();
            malformed[field] = json!(42);
            assert!(
                serde_json::from_value::<TerminalSession>(malformed).is_err(),
                "{field}"
            );
        }
    }

    // Each platform's host birth keeps its own wire fields, so a handle written on one
    // platform is read back as the same birth anywhere and never as another platform's.
    #[test]
    fn herdr_host_births_keep_their_platform_wire_fields() {
        let surface = |birth: Value| {
            let mut wire = json!({"terminal": "herdr", "session_id": "t7",
                "managed_session_id": "session-record",
                "herdr_core": {"socket": r"\\.\pipe\herdr-core-u-default.sock",
                               "core_id": "core", "created_ms": 13, "host_pid": 14}});
            for (field, value) in birth.as_object().unwrap() {
                wire["herdr_core"][field] = value.clone();
            }
            wire
        };
        for (birth, expected) in [
            (
                json!({"host_creation_time": 133_000_000_000_000_000_u64}),
                HerdrHostBirth::Windows {
                    host_creation_time: 133_000_000_000_000_000,
                },
            ),
            (
                json!({"host_start_seconds": 15, "host_start_microseconds": 16}),
                HerdrHostBirth::Macos {
                    host_start_seconds: 15,
                    host_start_microseconds: 16,
                },
            ),
        ] {
            let wire = surface(birth);
            let record: TerminalSession = serde_json::from_value(wire.clone()).unwrap();
            assert_eq!(record.herdr_core.as_ref().unwrap().birth, expected);
            assert_eq!(serde_json::to_value(&record).unwrap(), wire);
        }
        // A birth carrying another platform's field as well is no birth at all.
        let mixed = surface(json!({"host_creation_time": 1, "host_start_seconds": 2,
            "host_start_microseconds": 3}));
        assert!(serde_json::from_value::<TerminalSession>(mixed).is_err());
        let wrong = surface(json!({"host_creation_time": "1"}));
        assert!(serde_json::from_value::<TerminalSession>(wrong).is_err());
    }

    #[test]
    fn construction_writes_only_the_selected_adapter_identity() {
        let win = WindowsProcessIdentity {
            creation_time: 7,
            executable_path: "provider.exe".into(),
        };
        let mux = WezTermMux {
            socket: "/mux".into(),
            pid: 8,
            start_seconds: 9,
            start_microseconds: 10,
            owns_gui: false,
        };
        let cases = [
            (
                TerminalSession::new(TerminalKind::Ghostty, "pane".into()),
                json!({
                "terminal": "ghostty", "session_id": "pane"}),
            ),
            (
                TerminalSession::new(TerminalKind::WindowsConsole, "7".into())
                    .with_windows_identity(Some(win.clone())),
                json!({
                "terminal": "windows-console", "session_id": "7", "windows_process_identity": win}),
            ),
            (
                TerminalSession::new(TerminalKind::WezTerm, "0".into())
                    .with_wezterm_mux(Some(mux.clone())),
                json!({
                "terminal": "wezterm", "session_id": "0", "wezterm_mux": mux}),
            ),
        ];
        for (surface, wire) in cases {
            assert_eq!(serde_json::to_value(&surface).unwrap(), wire);
            assert_eq!(
                serde_json::from_value::<TerminalSession>(wire).unwrap(),
                surface
            );
        }
    }
}
