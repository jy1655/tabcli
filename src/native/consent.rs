//! Workspace consent is evidence, not a provider permission mode. Provider stores are
//! read only; their schemas and the way a managed CLI accepts trust belong to adapters.
use super::*;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(super) struct Identity {
    pub(super) path: PathBuf,
    volume: u64,
    file: u64,
    created: u128,
    owner: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(super) struct Evidence {
    pub(super) provider: String,
    pub(super) store: PathBuf,
    pub(super) key: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum Trust {
    Trusted(Evidence),
    Absent,
    Declined,
}

#[derive(Clone, Debug)]
pub(super) struct Homes {
    pub(super) codex: PathBuf,
    pub(super) claude: PathBuf,
    pub(super) agy: PathBuf,
    pub(super) pi: PathBuf,
}

impl Homes {
    pub(super) fn current() -> Result<Self> {
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from)
            .context("no user home for workspace trust lookup")?;
        Ok(Self {
            codex: std::env::var_os("CODEX_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".codex"))
                .join("config.toml"),
            claude: std::env::var_os("CLAUDE_CONFIG_DIR")
                .map(PathBuf::from)
                .map_or_else(|| home.join(".claude.json"), |p| p.join(".claude.json")),
            agy: home.join(".gemini/antigravity-cli/settings.json"),
            pi: std::env::var_os("PI_CODING_AGENT_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".pi/agent"))
                .join("trust.json"),
        })
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct Record {
    schema: u32,
    identity: Identity,
    source: Option<Evidence>,
    revoked: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct Assessment {
    schema: u32,
    identity: Identity,
    pub(super) source: Option<Evidence>,
    pub(super) state: String,
    pub(super) detail: String,
    pub(super) applied: Option<String>,
}

const SESSION_FILE: &str = "workspace-consent.json";

pub(super) fn identity(path: &Path) -> Result<Identity> {
    let path = path.canonicalize().context("cannot identify workspace")?;
    let meta = fs::metadata(&path)?;
    if !meta.is_dir() {
        bail!("workspace is not a directory");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok(Identity {
            path,
            volume: meta.dev(),
            file: meta.ino(),
            created: meta
                .created()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map_or(0, |t| t.as_nanos()),
            owner: meta.uid().to_string(),
        })
    }
    #[cfg(windows)]
    {
        use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
        use windows_sys::Win32::Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS, GetFileInformationByHandle,
        };
        let handle = OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(&path)?;
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        if unsafe { GetFileInformationByHandle(handle.as_raw_handle(), &mut info) } == 0 {
            return Err(std::io::Error::last_os_error())
                .context("cannot identify workspace directory handle");
        }
        Ok(Identity {
            owner: windows_owner(&path)?,
            path,
            volume: u64::from(info.dwVolumeSerialNumber),
            file: (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
            created: (u128::from(info.ftCreationTime.dwHighDateTime) << 32)
                | u128::from(info.ftCreationTime.dwLowDateTime),
        })
    }
}

// A provider key may use the ordinary Windows spelling, but consent never compares
// provider strings as directory identities. Unix spelling remains case-sensitive.
pub(super) fn native_key(path: &Path) -> Result<String> {
    let text = path.to_str().context("workspace path is not UTF-8")?;
    #[cfg(windows)]
    return Ok(text
        .strip_prefix(r"\\?\UNC\")
        .map(|p| format!(r"\\{p}"))
        .unwrap_or_else(|| text.strip_prefix(r"\\?\").unwrap_or(text).to_owned()));
    #[cfg(not(windows))]
    Ok(text.to_owned())
}

fn alias_free(requested: &Path, canonical: &Path) -> Result<bool> {
    let absolute = std::path::absolute(requested)?;
    // Inspect every component: canonicalizing first would erase a symlink alias.
    for ancestor in absolute.ancestors() {
        let metadata = fs::symlink_metadata(ancestor)?;
        if metadata.file_type().is_symlink() {
            return Ok(false);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
            if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                return Ok(false);
            }
        }
    }
    Ok(absolute.canonicalize()? == canonical)
}

pub(super) fn read_store(path: &Path) -> Result<Option<String>> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    if !meta.is_file() || meta.file_type().is_symlink() {
        bail!("trust store is not a regular file");
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(path)?;
    let opened_meta = file.metadata()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if opened_meta.uid() != unsafe { libc::geteuid() } || opened_meta.mode() & 0o022 != 0 {
            bail!("trust store is not owned and writable only by the current user");
        }
    }
    // Both are read from the handle that the content is read from, so that they
    // describe the same file.
    #[cfg(windows)]
    {
        let (owner, writers) = windows_store_access(&file)?;
        windows_store_is_private(&owner, &writers, &windows_current_owners()?)?;
    }
    if !opened_meta.is_file() || opened_meta.len() > 8 * 1024 * 1024 {
        bail!("trust store exceeds 8 MiB");
    }
    let mut text = String::new();
    file.take(8 * 1024 * 1024 + 1).read_to_string(&mut text)?;
    if text.len() > 8 * 1024 * 1024 {
        bail!("trust store exceeds 8 MiB");
    }
    Ok(Some(text))
}

fn record_path(root: &Path, workspace: &Path) -> PathBuf {
    // Collisions cannot grant consent: every read also checks the full path and identity.
    let hash = workspace
        .as_os_str()
        .as_encoded_bytes()
        .iter()
        .fold(0xcbf29ce484222325u64, |h, b| {
            (h ^ u64::from(*b)).wrapping_mul(0x100000001b3)
        });
    root.join("workspace-consent")
        .join(format!("{hash:016x}.json"))
}

fn with_record<T>(
    root: &Path,
    workspace: &Path,
    f: impl FnOnce(&Path, Option<Record>) -> Result<T>,
) -> Result<T> {
    let path = record_path(root, workspace);
    let parent = path.parent().unwrap();
    if fs::symlink_metadata(parent).is_ok_and(|m| !m.is_dir() || m.file_type().is_symlink()) {
        bail!("invalid workspace consent directory");
    }
    fs::create_dir_all(parent)?;
    set_private_directory_permissions(parent)?;
    let lock_path = path.with_extension("lock");
    if fs::symlink_metadata(&lock_path).is_ok_and(|m| !m.is_file() || m.file_type().is_symlink()) {
        bail!("invalid workspace consent lock");
    }
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(lock_path)?;
    set_private_file_permissions(&lock)?;
    lock.lock()?;
    let record = read_store(&path)?
        .map(|s| serde_json::from_str(&s))
        .transpose()?;
    f(&path, record)
}

fn assess_with(root: &Path, requested: &Path, homes: &Homes) -> Result<Assessment> {
    let identity = identity(requested)?;
    let mut assessment = Assessment {
        schema: 1,
        identity: identity.clone(),
        source: None,
        state: "unverified".into(),
        detail:
            "No exact provider workspace trust was verified; use the provider's own trust prompt."
                .into(),
        applied: None,
    };
    if !alias_free(requested, &identity.path)? {
        assessment.state = "alias".into();
        assessment.detail =
            "Workspace aliases do not inherit shared consent. Use the canonical path.".into();
        return Ok(assessment);
    }
    with_record(root, &identity.path, |path, existing| {
        let mut record = existing.unwrap_or_else(|| Record {
            schema: 1,
            identity: identity.clone(),
            source: None,
            revoked: false,
        });
        if record.schema != 1 || record.identity != identity {
            assessment.state = "identity-changed".into();
            assessment.detail =
                "Directory identity or owner changed; shared consent requires an explicit reset."
                    .into();
        } else if record.revoked {
            assessment.state = "revoked".into();
            assessment.detail =
                "Shared workspace consent is revoked; provider-owned trust is unchanged.".into();
        } else if let Some(source) = &record.source {
            let cli = FirstPartyCli::from_str(&source.provider).map_err(anyhow::Error::msg)?;
            let mut source_homes = homes.clone();
            match cli {
                FirstPartyCli::Codex => source_homes.codex = source.store.clone(),
                FirstPartyCli::Claude => source_homes.claude = source.store.clone(),
                FirstPartyCli::Agy => source_homes.agy = source.store.clone(),
                FirstPartyCli::Pi => source_homes.pi = source.store.clone(),
            }
            match provider::workspace_trust(cli, &identity.path, &source_homes) {
                Ok(Trust::Trusted(now)) if now == *source => {
                    assessment.source = Some(now);
                }
                _ => {
                    assessment.state = "source-reset".into();
                    assessment.detail = "The original provider trust is absent, declined, or unreadable. No other provider is silently substituted; reset consent after approving again.".into();
                }
            }
        } else {
            for cli in [
                FirstPartyCli::Codex,
                FirstPartyCli::Claude,
                FirstPartyCli::Agy,
                FirstPartyCli::Pi,
            ] {
                if let Ok(Trust::Trusted(source)) =
                    provider::workspace_trust(cli, &identity.path, homes)
                {
                    assessment.source = Some(source.clone());
                    record.source = Some(source);
                    break;
                }
            }
            write_json_atomic(path, &record)?;
        }
        if assessment.source.is_some() {
            assessment.state = "verified".into();
            assessment.detail = "Exact provider trust and canonical directory identity verified. Tool permissions are unchanged.".into();
        }
        Ok(assessment)
    })
}

pub(super) fn prepare(directory: &Path, requested: &Path) -> Result<()> {
    let assessment = assess_with(
        directory.parent().context("session has no state root")?,
        requested,
        &Homes::current()?,
    )?;
    write_json_atomic(&directory.join(SESSION_FILE), &assessment)
}

pub(super) fn authorized(directory: &Path, target: FirstPartyCli) -> Result<bool> {
    let Some(text) = read_regular_text_if_present(&directory.join(SESSION_FILE))? else {
        return Ok(false);
    };
    let assessment: Assessment = serde_json::from_str(&text)?;
    if assessment.schema != 1
        || assessment.state != "verified"
        || identity(&assessment.identity.path)? != assessment.identity
    {
        return Ok(false);
    }
    let homes = Homes::current()?;
    if matches!(
        provider::workspace_trust(target, &assessment.identity.path, &homes),
        Err(_) | Ok(Trust::Declined)
    ) {
        return Ok(false);
    }
    // Re-read under the consent lock immediately before applying trust. A session file
    // is diagnostic history, never a capability that survives revoke or source reset.
    let now = assess_with(
        directory.parent().context("session has no state root")?,
        &assessment.identity.path,
        &homes,
    )?;
    Ok(now.state == "verified"
        && now.identity == assessment.identity
        && now.source == assessment.source)
}

pub(super) fn applied(directory: &Path, method: &str) -> Result<()> {
    let path = directory.join(SESSION_FILE);
    let mut assessment: Assessment = read_json(&path)?;
    assessment.applied = Some(method.to_owned());
    write_json_atomic(&path, &assessment)
}

pub(super) fn complete_launch(
    directory: &Path,
    target: FirstPartyCli,
    session: &terminal::TerminalSession,
    deadline: Instant,
) -> Result<()> {
    if !matches!(target, FirstPartyCli::Claude | FirstPartyCli::Agy)
        || !authorized(directory, target)?
    {
        return Ok(());
    }
    let manifest = read_manifest(directory)?;
    if target == FirstPartyCli::Claude && manifest.yolo {
        return Ok(());
    }
    let homes = Homes::current()?;
    if matches!(
        provider::workspace_trust(target, &manifest.workspace, &homes)?,
        Trust::Trusted(_)
    ) {
        return Ok(());
    }
    let mut responded = false;
    while Instant::now() < deadline {
        if matches!(
            provider::workspace_trust(target, &manifest.workspace, &homes)?,
            Trust::Trusted(_)
        ) {
            if responded {
                applied(directory, "provider-workspace-dialog")?;
            }
            return Ok(());
        }
        if !responded {
            if !authorized(directory, target)? {
                bail!("workspace consent changed before trust dialog response; no response sent");
            }
            verify_terminal_surface_ownership_until(
                directory,
                &manifest.id,
                session,
                deadline,
                deadline.saturating_duration_since(Instant::now()),
            )?;
            let screen = terminal::read_screen(session, deadline)?;
            if let Some(key) = provider::workspace_trust_key(target, &screen, &manifest.workspace) {
                // Persist intent before the one response. Never retry an input call
                // that errors: it may already have reached the target dialog.
                applied(directory, "workspace-dialog-response-pending")?;
                responded = terminal::guarded_dialog_input(
                    session,
                    &terminal::GuardedDialogInput { screen, key },
                    deadline,
                )?;
            }
        }
        thread::sleep(
            Duration::from_millis(200).min(deadline.saturating_duration_since(Instant::now())),
        );
    }
    bail!(
        "workspace trust was not confirmed before the deadline; inspect workspace_consent and the managed terminal; no input was retried"
    )
}

// Native Windows paste must not select a workspace-trust button. A provider
// adapter calls this only where its official input API/startup event is absent.
pub(super) fn wait_for_native_trust(
    directory: &Path,
    target: FirstPartyCli,
    deadline: Instant,
) -> Result<()> {
    let manifest = read_manifest(directory)?;
    let homes = Homes::current()?;
    loop {
        let assessment = observe(directory);
        let applied_here = matches!(
            assessment.get("applied").and_then(|s| s.as_str()),
            Some("codex-config-override" | "pi-approve-once")
        );
        if (applied_here && authorized(directory, target)?)
            || matches!(
                provider::workspace_trust(target, &manifest.workspace, &homes),
                Ok(Trust::Trusted(_))
            )
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!(
                "workspace trust is not verified; no initial console input was sent. Approve the exact workspace in the managed provider and start a new request after a timeout"
            );
        }
        thread::sleep(
            Duration::from_millis(100).min(deadline.saturating_duration_since(Instant::now())),
        );
    }
}

pub(super) fn observe(directory: &Path) -> serde_json::Value {
    match read_regular_text_if_present(&directory.join(SESSION_FILE)) {
        Ok(Some(text)) => serde_json::from_str::<serde_json::Value>(&text)
            .unwrap_or_else(|e| serde_json::json!({"state":"unreadable","detail":e.to_string()})),
        Ok(None) => serde_json::Value::Null,
        Err(e) => serde_json::json!({"state":"unreadable","detail":e.to_string()}),
    }
}

pub(super) fn run(args: &[String]) -> Result<()> {
    let [action, workspace, rest @ ..] = args else {
        bail!("consent requires inspect|revoke|reset PATH [--json]");
    };
    if !matches!(action.as_str(), "inspect" | "revoke" | "reset")
        || !matches!(rest, [] | [_] if rest.first().is_none_or(|s| s == "--json"))
    {
        bail!("consent requires inspect|revoke|reset PATH [--json]");
    }
    let workspace = Path::new(workspace);
    let identity = identity(workspace)?;
    let root = state_root()?;
    if action == "inspect" {
        let path = record_path(&root, &identity.path);
        let record = read_store(&path)?
            .map(|s| serde_json::from_str::<serde_json::Value>(&s))
            .transpose()?;
        println!(
            "{}",
            serde_json::to_string_pretty(
                &serde_json::json!({"workspace":identity.path,"current_identity":identity,"record":record})
            )?
        );
        return Ok(());
    }
    with_record(&root, &identity.path.clone(), |path, existing| {
        let source = existing
            .filter(|r| r.identity == identity)
            .and_then(|r| r.source);
        let record = Record {
            schema: 1,
            identity,
            source: if action == "reset" { None } else { source },
            revoked: action == "revoke",
        };
        write_json_atomic(path, &record)?;
        println!("{}", serde_json::to_string_pretty(&record)?);
        Ok(())
    })
}

/// The Windows counterpart of the Unix owner and mode check: a trust store counts as this
/// user's own only when one of this process's own owners owns it and no access rule lets
/// another account change it. `writers` are the accounts that such a rule names.
///
/// The system, the Administrators group and the owner (`OWNER RIGHTS`) may be among them:
/// they can change any file of this user whatever its access list says. Every other
/// account, such as `Authenticated Users` on a folder outside the user profile, could put
/// a workspace into the store that this user never approved.
#[cfg(windows)]
fn windows_store_is_private(owner: &str, writers: &[String], own: &[String]) -> Result<()> {
    const SYSTEM_WRITERS: [&str; 3] = ["S-1-5-18", "S-1-5-32-544", "S-1-3-4"];
    if !own.iter().any(|sid| sid == owner) {
        bail!("trust store owner differs from current user");
    }
    if let Some(other) = writers
        .iter()
        .find(|sid| !own.contains(sid) && !SYSTEM_WRITERS.contains(&sid.as_str()))
    {
        bail!("trust store can be changed by another account ({other})");
    }
    Ok(())
}

/// The owner of an open trust store, and every account that an access rule of the store
/// allows to change its content or its access list.
///
/// A store without an access list is open to everyone, and a rule of a kind other than a
/// plain allow or deny cannot be evaluated here; both are refused. A deny rule only takes
/// access away, and an inherit-only rule does not apply to the store itself.
#[cfg(windows)]
fn windows_store_access(file: &File) -> Result<(String, Vec<String>)> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::{
        Foundation::{GENERIC_ALL, GENERIC_WRITE, LocalFree},
        Security::{
            ACCESS_ALLOWED_ACE, ACE_HEADER, ACL,
            Authorization::{GetSecurityInfo, SE_FILE_OBJECT},
            DACL_SECURITY_INFORMATION, GetAce, INHERIT_ONLY_ACE, OWNER_SECURITY_INFORMATION,
        },
        Storage::FileSystem::{FILE_APPEND_DATA, FILE_WRITE_DATA, WRITE_DAC, WRITE_OWNER},
    };
    // `ACCESS_ALLOWED_ACE_TYPE` and `ACCESS_DENIED_ACE_TYPE` of `winnt.h`.
    const ALLOWED: u8 = 0;
    const DENIED: u8 = 1;
    const CHANGE: u32 =
        FILE_WRITE_DATA | FILE_APPEND_DATA | WRITE_DAC | WRITE_OWNER | GENERIC_WRITE | GENERIC_ALL;

    let mut owner = std::ptr::null_mut();
    let mut access_list: *mut ACL = std::ptr::null_mut();
    let mut descriptor = std::ptr::null_mut();
    let error = unsafe {
        GetSecurityInfo(
            file.as_raw_handle(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            std::ptr::null_mut(),
            &mut access_list,
            std::ptr::null_mut(),
            &mut descriptor,
        )
    };
    if error != 0 {
        return Err(std::io::Error::from_raw_os_error(error as i32))
            .context("cannot read the trust store's owner and access list");
    }
    let result = (|| {
        let owner = windows_sid(owner)?;
        if access_list.is_null() {
            bail!("trust store has no access list, so every account can change it");
        }
        let mut writers = Vec::new();
        for index in 0..u32::from(unsafe { (*access_list).AceCount }) {
            let mut rule = std::ptr::null_mut();
            if unsafe { GetAce(access_list, index, &mut rule) } == 0 {
                return Err(std::io::Error::last_os_error())
                    .context("cannot read an access rule of the trust store");
            }
            let header = unsafe { *rule.cast::<ACE_HEADER>() };
            if u32::from(header.AceFlags) & INHERIT_ONLY_ACE != 0 || header.AceType == DENIED {
                continue;
            }
            if header.AceType != ALLOWED
                || usize::from(header.AceSize) < std::mem::size_of::<ACCESS_ALLOWED_ACE>()
            {
                bail!("trust store has an access rule that cannot be evaluated");
            }
            let allowed = rule.cast::<ACCESS_ALLOWED_ACE>();
            if unsafe { (*allowed).Mask } & CHANGE != 0 {
                let sid =
                    windows_sid(unsafe { std::ptr::addr_of_mut!((*allowed).SidStart) }.cast())?;
                if !writers.contains(&sid) {
                    writers.push(sid);
                }
            }
        }
        Ok((owner, writers))
    })();
    unsafe { LocalFree(descriptor) };
    result
}

// The owners that a file written by this user's own processes can have: the user, and the
// default owner of this process's token. For an elevated process that is the
// Administrators group, so a provider that ran elevated left its trust store owned by
// that group, and the files that a job on a GitHub Windows runner writes are owned by it.
// A store with any other owner was written by someone else.
#[cfg(windows)]
fn windows_current_owners() -> Result<Vec<String>> {
    use std::os::windows::io::FromRawHandle;
    use windows_sys::Win32::{
        Security::{GetTokenInformation, PSID, TOKEN_QUERY, TokenOwner, TokenUser},
        System::Threading::{GetCurrentProcess, OpenProcessToken},
    };
    let mut raw = std::ptr::null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw) } == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let _handle = unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(raw) };
    let mut owners = Vec::new();
    // `TOKEN_USER` and `TOKEN_OWNER` both begin with the pointer to their SID.
    for class in [TokenUser, TokenOwner] {
        let mut needed = 0;
        unsafe { GetTokenInformation(raw, class, std::ptr::null_mut(), 0, &mut needed) };
        let mut buffer = vec![0usize; (needed as usize).div_ceil(std::mem::size_of::<usize>())];
        if unsafe {
            GetTokenInformation(raw, class, buffer.as_mut_ptr().cast(), needed, &mut needed)
        } == 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        let sid = windows_sid(unsafe { *buffer.as_ptr().cast::<PSID>() })?;
        if !owners.contains(&sid) {
            owners.push(sid);
        }
    }
    Ok(owners)
}

#[cfg(windows)]
fn windows_owner(path: &Path) -> Result<String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::{
        Foundation::LocalFree,
        Security::{
            Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT},
            OWNER_SECURITY_INFORMATION,
        },
    };
    let path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut owner = std::ptr::null_mut();
    let mut descriptor = std::ptr::null_mut();
    let error = unsafe {
        GetNamedSecurityInfoW(
            path.as_ptr(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &mut owner,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut descriptor,
        )
    };
    if error != 0 {
        return Err(std::io::Error::from_raw_os_error(error as i32).into());
    }
    let result = windows_sid(owner);
    unsafe { LocalFree(descriptor) };
    result
}

#[cfg(windows)]
fn windows_sid(sid: windows_sys::Win32::Security::PSID) -> Result<String> {
    use windows_sys::Win32::{
        Foundation::LocalFree, Security::Authorization::ConvertSidToStringSidW,
    };
    let mut text = std::ptr::null_mut();
    if unsafe { ConvertSidToStringSidW(sid, &mut text) } == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let mut len = 0;
    unsafe {
        while *text.add(len) != 0 {
            len += 1;
        }
    }
    let result =
        String::from_utf16(unsafe { std::slice::from_raw_parts(text, len) }).map_err(Into::into);
    unsafe { LocalFree(text.cast()) };
    result
}

#[cfg(test)]
pub(super) fn fixture_homes(root: &Path) -> Homes {
    Homes {
        codex: root.join("codex.toml"),
        claude: root.join("claude.json"),
        agy: root.join("agy.json"),
        pi: root.join("pi.json"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (tempfile::TempDir, PathBuf, Homes) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let workspace = root.join("workspace");
        fs::create_dir(&workspace).unwrap();
        let homes = fixture_homes(&root);
        (temp, workspace, homes)
    }

    fn trust_pi(homes: &Homes, workspace: &Path) {
        write_private(
            &homes.pi,
            serde_json::to_string(&serde_json::json!({native_key(workspace).unwrap(): true}))
                .unwrap()
                .as_bytes(),
        )
        .unwrap();
    }

    // Replaces the access list of a fixture with the one that `sddl` describes.
    #[cfg(windows)]
    fn set_access_list(path: &Path, sddl: &str) {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::{
            Foundation::LocalFree,
            Security::{
                Authorization::{
                    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
                    SE_FILE_OBJECT, SetNamedSecurityInfoW,
                },
                DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl,
                PROTECTED_DACL_SECURITY_INFORMATION,
            },
        };
        let wide = |text: &std::ffi::OsStr| text.encode_wide().chain(Some(0)).collect::<Vec<_>>();
        let (sddl_text, path_text) = (wide(sddl.as_ref()), wide(path.as_os_str()));
        let mut descriptor = std::ptr::null_mut();
        let converted = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl_text.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                std::ptr::null_mut(),
            )
        };
        assert_ne!(converted, 0, "{sddl}: {}", std::io::Error::last_os_error());
        let (mut present, mut defaulted) = (0, 0);
        let mut access_list = std::ptr::null_mut();
        let extracted = unsafe {
            GetSecurityDescriptorDacl(descriptor, &mut present, &mut access_list, &mut defaulted)
        };
        assert!(extracted != 0 && present != 0, "{sddl}");
        let status = unsafe {
            SetNamedSecurityInfoW(
                path_text.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                access_list,
                std::ptr::null_mut(),
            )
        };
        unsafe { LocalFree(descriptor) };
        assert_eq!(status, 0, "{sddl}");
    }

    // A provider that ran elevated leaves its store owned by the Administrators group,
    // which is the default owner of an elevated token; the files that a job on a GitHub
    // Windows runner writes have that owner. Whichever owner Windows gives a file that
    // this process writes, it is one of this process's own.
    #[cfg(windows)]
    #[test]
    fn a_store_written_by_this_user_is_read_whoever_windows_made_its_owner() {
        let (_temp, workspace, homes) = setup();
        trust_pi(&homes, &workspace);

        let owners = windows_current_owners().unwrap();
        assert!((1..=2).contains(&owners.len()), "{owners:?}");
        assert!(
            owners.contains(&windows_owner(&homes.pi).unwrap()),
            "{owners:?}"
        );
        assert!(read_store(&homes.pi).unwrap().is_some());
    }

    // The owner alone does not say who can change a store: outside the user profile a
    // file usually inherits a rule that lets every signed-in account modify it.
    #[cfg(windows)]
    #[test]
    fn a_store_that_another_account_can_change_is_refused() {
        let (_temp, workspace, homes) = setup();
        trust_pi(&homes, &workspace);
        let user = windows_current_owners().unwrap().remove(0);
        let refusal = |sddl: String| {
            set_access_list(&homes.pi, &sddl);
            read_store(&homes.pi).unwrap_err().to_string()
        };

        // Everyone may write the content.
        assert_eq!(
            refusal(format!("D:P(A;;FA;;;{user})(A;;FW;;;WD)")),
            "trust store can be changed by another account (S-1-1-0)"
        );
        // Authenticated Users may rewrite the access list, and so give themselves the rest.
        assert_eq!(
            refusal(format!("D:P(A;;FA;;;{user})(A;;WD;;;AU)")),
            "trust store can be changed by another account (S-1-5-11)"
        );
        // Appending is enough to add a workspace, and a new owner can do everything. The
        // account need not exist: the rule is what counts.
        for rights in ["0x4", "WO"] {
            assert_eq!(
                refusal(format!(
                    "D:P(A;;FA;;;{user})(A;;{rights};;;S-1-5-21-1-2-3-4444)"
                )),
                "trust store can be changed by another account (S-1-5-21-1-2-3-4444)",
                "{rights}"
            );
        }
        assert_eq!(
            refusal("D:NO_ACCESS_CONTROL".to_owned()),
            "trust store has no access list, so every account can change it"
        );
        // A conditional rule grants what its condition decides, which is not evaluated.
        assert_eq!(
            refusal(format!(
                "D:P(A;;FA;;;{user})(XA;;FR;;;WD;(@User.Title==\"PM\"))"
            )),
            "trust store has an access rule that cannot be evaluated"
        );
    }

    #[cfg(windows)]
    #[test]
    fn a_store_that_others_can_only_read_stays_this_users_own() {
        let (_temp, workspace, homes) = setup();
        trust_pi(&homes, &workspace);
        let user = windows_current_owners().unwrap().remove(0);

        for sddl in [
            // Everyone may read it.
            format!("D:P(A;;FA;;;{user})(A;;FR;;;WD)"),
            // The system and the Administrators group may change it, as in a user profile.
            format!("D:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FA;;;{user})"),
            // A rule that only takes write access away from an account.
            format!("D:P(D;;0x6;;;AN)(A;;FA;;;{user})"),
        ] {
            set_access_list(&homes.pi, &sddl);
            assert!(read_store(&homes.pi).unwrap().is_some(), "{sddl}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn only_this_processs_owners_and_the_system_may_change_a_store() {
        let sids = |sids: &[&str]| sids.iter().map(|sid| sid.to_string()).collect::<Vec<_>>();
        let verdict = |owner: &str, writers: &[&str], own: &[&str]| {
            windows_store_is_private(owner, &sids(writers), &sids(own))
                .map_err(|error| error.to_string())
        };
        let (user, other) = ("S-1-5-21-1-2-3-1001", "S-1-5-21-1-2-3-1002");
        let (system, administrators) = ("S-1-5-18", "S-1-5-32-544");
        let differs = Err("trust store owner differs from current user".to_owned());

        // A store in the user profile, read without elevation.
        assert_eq!(
            verdict(user, &[system, administrators, user], &[user]),
            Ok(())
        );
        // A store that an elevated process wrote, read by an elevated process.
        assert_eq!(
            verdict(administrators, &[user, "S-1-3-4"], &[user, administrators]),
            Ok(())
        );
        // The same store read without elevation: the group is not this process's owner.
        assert_eq!(verdict(administrators, &[user], &[user]), differs);
        assert_eq!(verdict(other, &[], &[user]), differs);
        assert_eq!(
            verdict(user, &[user, other], &[user]),
            Err(format!(
                "trust store can be changed by another account ({other})"
            ))
        );
        // Being the default owner of an elevated process does not make the Users group one.
        assert_eq!(
            verdict(administrators, &["S-1-5-32-545"], &[user, administrators]),
            Err("trust store can be changed by another account (S-1-5-32-545)".to_owned())
        );
    }

    #[test]
    fn unknown_workspace_is_not_approved_and_exact_native_trust_can_be_imported() {
        let (temp, workspace, homes) = setup();
        let first = assess_with(temp.path(), &workspace, &homes).unwrap();
        assert_eq!(first.state, "unverified");
        assert!(first.source.is_none());
        trust_pi(&homes, &workspace);
        let next = assess_with(temp.path(), &workspace, &homes).unwrap();
        assert_eq!(next.state, "verified");
        assert_eq!(next.source.unwrap().provider, "pi");
        let child = workspace.join("child");
        fs::create_dir(&child).unwrap();
        assert!(
            assess_with(temp.path(), &child, &homes)
                .unwrap()
                .source
                .is_none()
        );
        let sibling = temp.path().join("workspace2");
        fs::create_dir(&sibling).unwrap();
        assert!(
            assess_with(temp.path(), &sibling, &homes)
                .unwrap()
                .source
                .is_none()
        );
    }

    #[test]
    fn replaced_directory_or_owner_does_not_reimport_old_path_trust() {
        let (temp, workspace, homes) = setup();
        trust_pi(&homes, &workspace);
        assert_eq!(
            assess_with(temp.path(), &workspace, &homes).unwrap().state,
            "verified"
        );
        fs::rename(&workspace, temp.path().join("old-workspace")).unwrap();
        fs::create_dir(&workspace).unwrap();
        assert_eq!(
            assess_with(temp.path(), &workspace, &homes).unwrap().state,
            "identity-changed"
        );
        with_record(temp.path(), &workspace, |path, record| {
            let mut record = record.unwrap();
            record.identity = identity(&workspace)?;
            record.identity.owner = "another-owner".into();
            write_json_atomic(path, &record)
        })
        .unwrap();
        assert_eq!(
            assess_with(temp.path(), &workspace, &homes).unwrap().state,
            "identity-changed"
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_alias_and_unsafe_store_do_not_supply_consent() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let (temp, workspace, homes) = setup();
        trust_pi(&homes, &workspace);
        let alias = temp.path().join("alias");
        symlink(&workspace, &alias).unwrap();
        assert_eq!(
            assess_with(temp.path(), &alias, &homes).unwrap().state,
            "alias"
        );
        fs::set_permissions(&homes.pi, fs::Permissions::from_mode(0o666)).unwrap();
        assert!(provider::workspace_trust(FirstPartyCli::Pi, &workspace, &homes).is_err());
        assert!(
            assess_with(temp.path(), &workspace, &homes)
                .unwrap()
                .source
                .is_none()
        );
    }

    #[test]
    fn source_reset_does_not_reimport_an_automatically_trusted_target() {
        let (temp, workspace, homes) = setup();
        trust_pi(&homes, &workspace);
        assess_with(temp.path(), &workspace, &homes).unwrap();
        write_json_atomic(
            &homes.agy,
            &serde_json::json!({"trustedWorkspaces":[native_key(&workspace).unwrap()]}),
        )
        .unwrap();
        fs::remove_file(&homes.pi).unwrap();
        let assessment = assess_with(temp.path(), &workspace, &homes).unwrap();
        assert_eq!(assessment.state, "source-reset");
        assert!(assessment.source.is_none());
    }

    #[test]
    fn revocation_is_serialized_with_import_and_reset_requires_fresh_evidence() {
        let (temp, workspace, homes) = setup();
        trust_pi(&homes, &workspace);
        assess_with(temp.path(), &workspace, &homes).unwrap();
        // Deterministic lock contention: the importer must see the revoke written
        // while it waits, rather than replace it using a pre-lock snapshot.
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let root = temp.path().to_owned();
        let child_workspace = workspace.clone();
        let child_homes = homes.clone();
        let child_barrier = barrier.clone();
        let worker = with_record(temp.path(), &workspace, |path, record| {
            let worker = thread::spawn(move || {
                child_barrier.wait();
                assess_with(&root, &child_workspace, &child_homes).unwrap()
            });
            barrier.wait();
            let mut record = record.unwrap();
            record.revoked = true;
            write_json_atomic(path, &record)?;
            Ok(worker)
        })
        .unwrap();
        assert_eq!(worker.join().unwrap().state, "revoked");
        with_record(temp.path(), &workspace, |path, record| {
            let mut record = record.unwrap();
            record.revoked = false;
            record.source = None;
            write_json_atomic(path, &record)
        })
        .unwrap();
        fs::remove_file(&homes.pi).unwrap();
        assert_eq!(
            assess_with(temp.path(), &workspace, &homes).unwrap().state,
            "unverified"
        );
        trust_pi(&homes, &workspace);
        assert_eq!(
            assess_with(temp.path(), &workspace, &homes).unwrap().state,
            "verified"
        );
    }
}
