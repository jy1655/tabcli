use super::store::{Environment, native_key};
use crate::native::session::primitives::{write_json_atomic, write_private};
use crate::native::session::{
    RecordStore,
    schema::{SESSION_SCHEMA, SessionManifest},
};
use std::fs;
use std::path::{Path, PathBuf};

// A caller whose `HOME` is the temporary root, and the four stores the adapters
// resolve under it.
pub(super) struct Fixture {
    pub(super) env: Environment,
    pub(super) codex: PathBuf,
    pub(super) claude: PathBuf,
    pub(super) agy: PathBuf,
    pub(super) pi: PathBuf,
}
pub(super) fn fixture(root: &Path) -> Fixture {
    for parent in [".codex", ".gemini/antigravity-cli", ".pi/agent"] {
        fs::create_dir_all(root.join(parent)).unwrap();
    }
    Fixture {
        env: Environment::at_home(root),
        codex: root.join(".codex").join("config.toml"),
        claude: root.join(".claude.json"),
        agy: root.join(".gemini/antigravity-cli/settings.json"),
        pi: root.join(".pi/agent").join("trust.json"),
    }
}
pub(super) fn setup() -> (tempfile::TempDir, PathBuf, Fixture) {
    let temp = tempfile::tempdir().unwrap();
    RecordStore::at(temp.path())
        .set_directory_private()
        .unwrap();
    let root = temp.path().canonicalize().unwrap();
    let workspace = root.join("workspace");
    fs::create_dir(&workspace).unwrap();
    (temp, workspace, fixture(&root))
}
pub(super) fn trust_pi(fixture: &Fixture, workspace: &Path) {
    if fixture.pi.exists() {
        fs::remove_file(&fixture.pi).unwrap();
    }
    write_private(
        &fixture.pi,
        serde_json::to_string(&serde_json::json!({native_key(workspace).unwrap(): true}))
            .unwrap()
            .as_bytes(),
    )
    .unwrap();
}
// A session directory in a state root of its own; a manifest is read only from a
// directory that bears its id.
pub(super) fn session(workspace: &Path, state: &str) -> (tempfile::TempDir, PathBuf) {
    let root = tempfile::tempdir().unwrap();
    RecordStore::at(root.path())
        .set_directory_private()
        .unwrap();
    let directory = root.path().join("session-trustwait");
    fs::create_dir(&directory).unwrap();
    write_json_atomic(
        &directory.join("manifest.json"),
        &SessionManifest {
            schema: SESSION_SCHEMA,
            id: "session-trustwait".to_owned(),
            provider: "codex".to_owned(),
            provider_path: PathBuf::from("codex"),
            provider_version: "0.159.3".to_owned(),
            workspace: workspace.to_owned(),
            title: "trust wait".to_owned(),
            model: None,
            effort: None,
            yolo: false,
            created_unix_ms: 1,
        },
    )
    .unwrap();
    write_private(
            &directory.join("status.json"),
            format!(
                r#"{{"state":"{state}","generation":1,"updated_unix_ms":1,"exit_code":null,"error":null}}"#
            ).as_bytes(),
        )
        .unwrap();
    (root, directory)
}
