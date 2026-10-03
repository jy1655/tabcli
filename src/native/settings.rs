//! Settings that a user changes with `agent-bridge settings`. They are one private
//! record in the state root; a missing record, or a missing key, means the default.
use super::*;

const FILE: &str = "settings.json";
const USAGE: &str = "settings takes no argument to show the settings, or macos-open-mode <tab-first|new-window> or windows-tab-window <dedicated|current> to change one; --json is accepted";

/// A creation preference, never cleanup authority. Each terminal handle records
/// the surface that was actually created, even if this setting changes later.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(super) enum MacosOpenMode {
    #[default]
    TabFirst,
    NewWindow,
}

impl MacosOpenMode {
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::TabFirst => "tab-first",
            Self::NewWindow => "new-window",
        }
    }
}

impl FromStr for MacosOpenMode {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "tab-first" => Ok(Self::TabFirst),
            "new-window" => Ok(Self::NewWindow),
            _ => bail!("macos-open-mode is tab-first or new-window, not {value:?}"),
        }
    }
}

/// The Windows Terminal window in which a managed console opens its tab on native
/// Windows. It has no effect on another platform.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(super) enum WindowsTabWindow {
    /// The window named `agent-bridge`, which Windows Terminal creates when it does not
    /// exist. No window the user works in is touched.
    #[default]
    Dedicated,
    /// The most recently used Windows Terminal window. Windows Terminal selects the new
    /// tab and offers no way to select the earlier one again, so the tab keeps the
    /// keyboard inside that window until the user switches back.
    Current,
}

impl WindowsTabWindow {
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::Dedicated => "dedicated",
            Self::Current => "current",
        }
    }
}

impl FromStr for WindowsTabWindow {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "dedicated" => Ok(Self::Dedicated),
            "current" => Ok(Self::Current),
            _ => bail!("windows-tab-window is dedicated or current, not {value:?}"),
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
struct Record {
    schema: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    windows_tab_window: Option<WindowsTabWindow>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    macos_open_mode: Option<MacosOpenMode>,
}

fn read(root: &Path) -> Result<Record> {
    let path = root.join(FILE);
    let Some(text) = read_regular_text_if_present(&path)? else {
        return Ok(Record {
            schema: 1,
            windows_tab_window: None,
            macos_open_mode: None,
        });
    };
    let record: Record = serde_json::from_str(&text)
        .with_context(|| format!("invalid settings record {}", path.display()))?;
    if record.schema != 1 {
        bail!(
            "unsupported settings schema {} in {}",
            record.schema,
            path.display()
        );
    }
    Ok(record)
}

pub(super) fn windows_tab_window(root: &Path) -> Result<WindowsTabWindow> {
    Ok(read(root)?.windows_tab_window.unwrap_or_default())
}

pub(super) fn macos_open_mode(root: &Path) -> Result<MacosOpenMode> {
    Ok(read(root)?.macos_open_mode.unwrap_or_default())
}

pub(super) fn run(args: &[String]) -> Result<()> {
    let settings = apply(&state_root()?, args)?;
    println!("{}", serde_json::to_string_pretty(&settings)?);
    Ok(())
}

// Changes the named setting, if one is named, and reports every setting as it now is.
fn apply(root: &Path, args: &[String]) -> Result<serde_json::Value> {
    let named: Vec<&str> = args
        .iter()
        .map(String::as_str)
        .filter(|argument| *argument != "--json")
        .collect();
    if args.len() - named.len() > 1 {
        bail!(USAGE);
    }
    match named.as_slice() {
        [] => {}
        ["macos-open-mode", value] => {
            let value = value.parse::<MacosOpenMode>()?;
            create_state_root(root, &home_directories())?;
            let mut record = read(root)?;
            record.macos_open_mode = Some(value);
            write_json_atomic(&root.join(FILE), &record)?;
        }
        ["windows-tab-window", value] => {
            let value = value.parse::<WindowsTabWindow>()?;
            create_state_root(root, &home_directories())?;
            let mut record = read(root)?;
            record.windows_tab_window = Some(value);
            write_json_atomic(&root.join(FILE), &record)?;
        }
        _ => bail!(USAGE),
    }
    Ok(serde_json::json!({
        "settings_file": root.join(FILE),
        "windows_tab_window": windows_tab_window(root)?.as_str(),
        "macos_open_mode": macos_open_mode(root)?.as_str(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arguments(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn the_tab_window_is_dedicated_until_the_user_chooses_the_current_one() {
        let directory = tempfile::tempdir().unwrap();
        // The state root is created by the first change, not by a look at the settings.
        let root = directory.path().join("native-sessions");
        assert_eq!(
            windows_tab_window(&root).unwrap(),
            WindowsTabWindow::Dedicated
        );
        let shown = apply(&root, &arguments(&["--json"])).unwrap();
        assert_eq!(shown["windows_tab_window"], "dedicated");
        assert!(!root.exists());

        let changed = apply(&root, &arguments(&["windows-tab-window", "current"])).unwrap();
        assert_eq!(changed["windows_tab_window"], "current");
        assert_eq!(changed["settings_file"], serde_json::json!(root.join(FILE)));
        assert_eq!(
            windows_tab_window(&root).unwrap(),
            WindowsTabWindow::Current
        );
        assert_eq!(
            read_json::<serde_json::Value>(&root.join(FILE)).unwrap(),
            serde_json::json!({ "schema": 1, "windows_tab_window": "current" })
        );

        let restored = apply(
            &root,
            &arguments(&["windows-tab-window", "dedicated", "--json"]),
        )
        .unwrap();
        assert_eq!(restored["windows_tab_window"], "dedicated");
        assert_eq!(
            windows_tab_window(&root).unwrap(),
            WindowsTabWindow::Dedicated
        );
    }

    #[test]
    fn an_unknown_setting_or_value_changes_nothing() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("native-sessions");
        for invalid in [
            &["macos-open-mode"][..],
            &["macos-open-mode", "current"],
            &["macos-open-mode", "tab-first", "extra"],
            &["windows-tab-window"][..],
            &["windows-tab-window", "new"],
            &["windows-tab-window", "current", "extra"],
            &["tab-window", "current"],
            &["--json", "--json"],
        ] {
            assert!(apply(&root, &arguments(invalid)).is_err(), "{invalid:?}");
        }
        assert!(!root.exists());
    }

    #[test]
    fn a_settings_record_that_cannot_be_understood_is_an_error_not_a_default() {
        let directory = tempfile::tempdir().unwrap();
        for record in [
            r#"{"schema":2,"windows_tab_window":"current"}"#,
            r#"{"schema":1,"windows_tab_window":"somewhere"}"#,
            r#"{"schema":1,"macos_open_mode":"somewhere"}"#,
            "{",
        ] {
            fs::write(directory.path().join(FILE), record).unwrap();
            assert!(windows_tab_window(directory.path()).is_err(), "{record}");
            assert!(macos_open_mode(directory.path()).is_err(), "{record}");
            assert!(apply(directory.path(), &[]).is_err(), "{record}");
        }
    }

    #[test]
    fn macos_defaults_to_tabs_and_both_settings_survive_independent_updates() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("native-sessions");
        assert_eq!(macos_open_mode(&root).unwrap(), MacosOpenMode::TabFirst);
        assert_eq!(apply(&root, &[]).unwrap()["macos_open_mode"], "tab-first");
        assert!(!root.exists(), "reading defaults must not write settings");

        apply(&root, &arguments(&["windows-tab-window", "current"])).unwrap();
        assert_eq!(macos_open_mode(&root).unwrap(), MacosOpenMode::TabFirst);
        apply(&root, &arguments(&["macos-open-mode", "new-window"])).unwrap();
        assert_eq!(macos_open_mode(&root).unwrap(), MacosOpenMode::NewWindow);
        assert_eq!(
            windows_tab_window(&root).unwrap(),
            WindowsTabWindow::Current
        );
        apply(&root, &arguments(&["windows-tab-window", "dedicated"])).unwrap();
        assert_eq!(macos_open_mode(&root).unwrap(), MacosOpenMode::NewWindow);
        apply(&root, &arguments(&["macos-open-mode", "tab-first"])).unwrap();
        let reloaded = apply(&root, &[]).unwrap();
        assert_eq!(reloaded["macos_open_mode"], "tab-first");
        assert_eq!(reloaded["windows_tab_window"], "dedicated");
    }
}
