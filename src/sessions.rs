use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Session {
    /// Persistent identity for new/updated sessions; legacy files omit this.
    #[serde(default)]
    pub id: String,
    pub name: String,
    pub host: String,
    pub port: u16,
    #[serde(default)]
    pub username: Option<String>,
    pub password: String,
    /// SSH username for the SFTP file-transfer panel (optional; remembered per host).
    #[serde(default)]
    pub ssh_user: String,
}

impl Session {
    /// Identity of this row for an in-flight connection.
    ///
    /// A non-empty ID is that identity only when it appears once in `sessions`.
    /// A shared ID is not a row. Credential writeback is skipped for it: saving
    /// one copy assigns a new ID, and a recreated row with the same name and
    /// account would otherwise take the password. Legacy empty IDs still bind
    /// by name and account.
    pub fn identity(&self, sessions: &[Session]) -> SessionIdentity {
        let shared_id = id_is_duplicated(sessions, &self.id);
        SessionIdentity {
            id: if self.id.is_empty() || shared_id {
                String::new()
            } else {
                self.id.clone()
            },
            name: self.name.clone(),
            host: self.host.clone(),
            port: self.port,
            username: self.username.clone(),
            password: self.password.clone(),
            shared_id,
        }
    }

    pub fn display_label(&self) -> String {
        if self.name.is_empty() {
            format!("{}:{}", self.host, self.port)
        } else {
            format!("{} ({}:{})", self.name, self.host, self.port)
        }
    }
}

/// An immutable reference to the session that launched a connection. Legacy
/// sessions use their original name/endpoint/account until an ID is saved.
#[derive(Clone, PartialEq, Eq)]
pub struct SessionIdentity {
    id: String,
    name: String,
    host: String,
    port: u16,
    username: Option<String>,
    password: String,
    /// The source row's ID was shared. Name and account are not a substitute.
    shared_id: bool,
}

impl std::fmt::Debug for SessionIdentity {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SessionIdentity")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("username", &self.username)
            .field("password", &"[redacted]")
            .field("shared_id", &self.shared_id)
            .finish()
    }
}

impl SessionIdentity {
    fn matches(&self, session: &Session) -> bool {
        // A shared ID identifies no row. Matching the name and account would
        // write this connection's password into a recreated identical row.
        if self.shared_id {
            return false;
        }
        if !self.id.is_empty() {
            // A label can change while connecting. An endpoint or account
            // edit invalidates this in-flight connection's credential snapshot.
            // `id` is set only when it identified exactly one row at bind time.
            self.id == session.id
                && self.host == session.host
                && self.port == session.port
                && self.username == session.username
                && self.password == session.password
        } else {
            self.name == session.name
                && self.host == session.host
                && self.port == session.port
                && self.username == session.username
                && self.password == session.password
        }
    }
}

/// Host, port, and VNC username of the row a parent window asked to open.
/// The password stays off the command line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaunchAccount {
    pub host: String,
    pub port: u16,
    pub username: Option<String>,
}

fn account_matches(session: &Session, account: &LaunchAccount) -> bool {
    session.host == account.host
        && session.port == account.port
        && session.username == account.username
}

/// Resolve `--session-id` only when that ID is one row and still has the
/// name and account the parent window selected. A stale shared ID that now
/// belongs only to a sibling does not match.
pub fn find_launched_session<'a>(
    sessions: &'a [Session],
    id: &str,
    name: &str,
    account: &LaunchAccount,
) -> Result<&'a Session> {
    let session = find_by_id(sessions, id)?;
    anyhow::ensure!(
        session.name == name && account_matches(session, account),
        "Saved session ID does not match the requested name and account"
    );
    Ok(session)
}

/// Resolve `--session` and, when the parent supplied an account, require it.
/// An ambiguous name or host is an error.
pub fn find_named_account<'a>(
    sessions: &'a [Session],
    key: &str,
    account: Option<&LaunchAccount>,
) -> Result<&'a Session> {
    let session = find_named(sessions, key)?;
    if let Some(account) = account {
        anyhow::ensure!(
            account_matches(session, account),
            "Saved session does not match the requested account"
        );
    }
    Ok(session)
}

/// Child-process arguments that open `session`.
///
/// A shared ID is not passed: after another window saves one copy, that ID
/// belongs only to the other row. A name or host that matches more than one
/// row, or a different account, does not launch.
pub fn session_window_args(sessions: &[Session], session: &Session) -> Option<Vec<String>> {
    let account_args = launch_account_args(session);
    if !session.id.is_empty() && !id_is_duplicated(sessions, &session.id) {
        let mut args = vec![
            "--session-id".to_string(),
            session.id.clone(),
            "--session-name".to_string(),
            session.name.clone(),
        ];
        args.extend(account_args);
        return Some(args);
    }
    let key = if session.name.is_empty() {
        session.host.as_str()
    } else {
        session.name.as_str()
    };
    let found = find_named(sessions, key).ok()?;
    if found.name != session.name
        || found.host != session.host
        || found.port != session.port
        || found.username != session.username
    {
        return None;
    }
    let mut args = vec!["--session".to_string(), key.to_string()];
    args.extend(account_args);
    Some(args)
}

fn launch_account_args(session: &Session) -> Vec<String> {
    let mut args = vec![
        "--session-host".to_string(),
        session.host.clone(),
        "--session-port".to_string(),
        session.port.to_string(),
    ];
    if let Some(username) = &session.username {
        args.push("--session-username".to_string());
        args.push(username.clone());
    }
    args
}

pub fn new_id() -> String {
    format!("{:032x}", rand::random::<u128>())
}

/// True when a non-empty ID belongs to more than one session.
pub fn id_is_duplicated(sessions: &[Session], id: &str) -> bool {
    if id.is_empty() {
        return false;
    }
    let mut matches = sessions.iter().filter(|session| session.id == id);
    matches.next().is_some() && matches.next().is_some()
}

/// Keep `preferred` when it is non-empty and unused.
/// `exempt` is a slot being replaced, so its current ID is not a conflict.
/// Empty IDs stay available for legacy records and are never returned.
pub fn unique_session_id(sessions: &[Session], exempt: Option<usize>, preferred: &str) -> String {
    let taken = |id: &str| {
        !id.is_empty()
            && sessions
                .iter()
                .enumerate()
                .any(|(index, session)| Some(index) != exempt && session.id == id)
    };
    if !preferred.is_empty() && !taken(preferred) {
        return preferred.to_string();
    }
    loop {
        let id = new_id();
        if !taken(&id) {
            return id;
        }
    }
}

/// Insert `session` without replacing any existing record.
/// A missing or already-used ID is replaced before the push, in this same call.
pub fn insert_session(sessions: &mut Vec<Session>, mut session: Session) -> usize {
    session.id = unique_session_id(sessions, None, &session.id);
    sessions.push(session);
    sessions.len() - 1
}

/// Write `session` into `index` only when that slot still has `expected_id`.
/// A different row that happens to share the ID is left untouched. If this
/// slot's ID is already used by another row, the stored copy gets a new ID.
/// The previous ID remains on that other row and is not this row's identity.
pub fn replace_session_instance(
    sessions: &mut [Session],
    index: usize,
    expected_id: &str,
    mut session: Session,
) -> bool {
    let Some(current) = sessions.get(index) else {
        return false;
    };
    if current.id != expected_id {
        return false;
    }
    session.id = unique_session_id(sessions, Some(index), &session.id);
    sessions[index] = session;
    true
}

/// Remove `index` only when it still holds `expected`.
/// Another session with the same ID stays in the list.
pub fn remove_indexed_instance(
    sessions: &mut Vec<Session>,
    index: usize,
    expected: &Session,
) -> bool {
    if sessions.get(index) != Some(expected) {
        return false;
    }
    sessions.remove(index);
    true
}

/// Resolve a saved name before considering the legacy host-name shortcut.
/// Ambiguous names/hosts require an explicit choice rather than first-match auth.
pub fn find_named<'a>(sessions: &'a [Session], key: &str) -> Result<&'a Session> {
    let mut named = sessions.iter().filter(|s| s.name == key);
    if let Some(session) = named.next() {
        anyhow::ensure!(
            named.next().is_none(),
            "More than one saved session is named {key:?}"
        );
        return Ok(session);
    }
    let mut by_host = sessions.iter().filter(|s| s.host == key);
    let session = by_host
        .next()
        .ok_or_else(|| anyhow::anyhow!("Saved session {key:?} was not found"))?;
    anyhow::ensure!(
        by_host.next().is_none(),
        "More than one saved session uses host {key:?}; choose its session name"
    );
    Ok(session)
}

/// Internal window selector: IDs must identify exactly one saved session.
pub fn find_by_id<'a>(sessions: &'a [Session], id: &str) -> Result<&'a Session> {
    anyhow::ensure!(!id.is_empty(), "Saved session ID must not be empty");
    let mut matches = sessions.iter().filter(|s| s.id == id);
    let session = matches
        .next()
        .ok_or_else(|| anyhow::anyhow!("Saved session ID was not found"))?;
    anyhow::ensure!(
        matches.next().is_none(),
        "More than one saved session has this ID"
    );
    Ok(session)
}

/// Reuse credentials only when the endpoint/account selects exactly one session.
pub fn for_endpoint<'a>(
    sessions: &'a [Session],
    host: &str,
    port: u16,
    username: Option<&str>,
) -> Option<&'a Session> {
    let mut matches = sessions.iter().filter(|s| {
        s.host == host
            && s.port == port
            && username.map_or(true, |u| s.username.as_deref() == Some(u))
    });
    let session = matches.next()?;
    matches.next().is_none().then_some(session)
}

/// Update only the selected session. Ad-hoc connections may update a unique
/// endpoint/account match, or create a distinct saved account.
pub fn remember_connection(
    sessions: &mut Vec<Session>,
    host: &str,
    port: u16,
    username: Option<String>,
    password: &str,
    source: Option<&SessionIdentity>,
) -> bool {
    let mut matches = sessions.iter().enumerate().filter(|(_, s)| {
        source.map_or_else(
            || s.host == host && s.port == port && s.username == username,
            |identity| identity.matches(s),
        )
    });
    let index = matches.next().map(|(i, _)| i);
    if matches.next().is_some() {
        return false;
    }
    if let Some(i) = index {
        let mut changed = false;
        if sessions[i].id.is_empty() {
            let id = unique_session_id(sessions, Some(i), "");
            sessions[i].id = id;
            changed = true;
        }
        let session = &mut sessions[i];
        if session.username != username {
            session.username = username;
            changed = true;
        }
        if !password.is_empty() && session.password != password {
            session.password = password.to_string();
            changed = true;
        }
        changed
    } else if source.is_none() {
        insert_session(
            sessions,
            Session {
                id: String::new(),
                name: String::new(),
                host: host.to_string(),
                port,
                username,
                password: password.to_string(),
                ssh_user: String::new(),
            },
        );
        true
    } else {
        // The selected session was deleted/changed while connecting.
        false
    }
}

#[derive(Default, Serialize, Deserialize)]
struct Store {
    sessions: Vec<Session>,
}

/// Primary location: `sessions.json` right next to the executable. This is a
/// fixed, predictable path that resolves identically regardless of how the app
/// is launched or which user context resolves it — unlike `config_dir()`,
/// which can differ between interactive and non-interactive sessions on
/// domain/redirected profiles.
fn exe_path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    Some(exe.parent()?.join("sessions.json"))
}

/// Legacy location under the user's config dir (kept only for one-way migration
/// so previously-saved sessions aren't lost).
fn config_path() -> PathBuf {
    let dir = dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("ironvnc");
    let _ = std::fs::create_dir_all(&dir);
    dir.join("sessions.json")
}

/// The path the app reads from and writes to (shown in the UI).
pub fn path() -> PathBuf {
    exe_path().unwrap_or_else(config_path)
}

pub fn display_path() -> String {
    path().display().to_string()
}

pub fn load() -> Vec<Session> {
    // Primary (next-to-exe) is authoritative when present.
    if let Some(p) = exe_path() {
        if let Ok(content) = std::fs::read_to_string(&p) {
            if let Ok(store) = serde_json::from_str::<Store>(&content) {
                return store.sessions;
            }
        }
    }
    // Migration fallback: read the old config-dir file if the primary is absent.
    if let Ok(content) = std::fs::read_to_string(config_path()) {
        if let Ok(store) = serde_json::from_str::<Store>(&content) {
            return store.sessions;
        }
    }
    Vec::new()
}

/// Replace the on-disk session list with `sessions`.
///
/// The file is one snapshot of the whole list. Two windows can each load,
/// edit, and save; the later save replaces the earlier snapshot, including
/// rows that window did not change. That whole-list race is a known limit
/// and is out of scope for the per-row identity checks.
pub fn save(sessions: &[Session]) -> Result<()> {
    let store = Store {
        sessions: sessions.to_vec(),
    };
    let json = serde_json::to_string_pretty(&store)?;
    // Prefer next-to-exe; fall back to config dir only if that write fails
    // (e.g. the exe lives in a read-only directory).
    if let Some(p) = exe_path() {
        if std::fs::write(&p, &json).is_ok() {
            return Ok(());
        }
    }
    std::fs::write(config_path(), json)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(name: &str, username: &str, password: &str) -> Session {
        Session {
            id: String::new(),
            name: name.to_string(),
            host: "desktop.example".to_string(),
            port: 5900,
            username: Some(username.to_string()),
            password: password.to_string(),
            ssh_user: String::new(),
        }
    }

    #[test]
    fn legacy_sessions_deserialize_without_vnc_username_or_identity() {
        let json = r#"{"sessions":[{"name":"Old desktop","host":"desktop.example","port":5900,"password":"legacy"}]}"#;
        let store: Store = serde_json::from_str(json).unwrap();
        let session = &store.sessions[0];
        assert_eq!(session.username, None);
        assert!(session.id.is_empty());
        assert!(session.ssh_user.is_empty());
        let roundtrip: Store =
            serde_json::from_str(&serde_json::to_string(&store).unwrap()).unwrap();
        assert_eq!(roundtrip.sessions, store.sessions);
    }

    #[test]
    fn named_bob_connection_updates_only_bob_on_the_same_endpoint() {
        let mut sessions = vec![
            account("Alice", "alice", "alice secret"),
            account("Bob", "bob", "old bob"),
        ];
        let alice = sessions[0].clone();
        let source = find_named(&sessions, "Bob").unwrap().identity(&sessions);
        assert!(remember_connection(
            &mut sessions,
            "desktop.example",
            5900,
            Some("bob".to_string()),
            "new bob",
            Some(&source)
        ));
        assert_eq!(sessions[0], alice);
        assert_eq!(sessions[1].username.as_deref(), Some("bob"));
        assert_eq!(sessions[1].password, "new bob");
        assert!(!sessions[1].id.is_empty());
        let roundtrip: Vec<Session> =
            serde_json::from_str(&serde_json::to_string(&sessions).unwrap()).unwrap();
        assert_eq!(roundtrip, sessions);
    }

    #[test]
    fn persistent_identity_survives_session_label_edits() {
        let mut bob = account("Bob", "bob", "old");
        bob.id = "stable-bob-id".to_string();
        let source = bob.identity(std::slice::from_ref(&bob));
        bob.name = "Work login".to_string();
        let mut sessions = vec![account("Alice", "alice", "alice secret"), bob];
        assert!(remember_connection(
            &mut sessions,
            "desktop.example",
            5900,
            Some("bob.updated".to_string()),
            "updated",
            Some(&source)
        ));
        assert_eq!(sessions[0].password, "alice secret");
        assert_eq!(sessions[1].id, "stable-bob-id");
        assert_eq!(sessions[1].name, "Work login");
        assert_eq!(sessions[1].password, "updated");
    }

    #[test]
    fn in_flight_login_cannot_replace_edited_credentials() {
        for edit in ["host", "port", "username", "password"] {
            let mut saved = account("Work", "alice", "old");
            saved.id = "same-id".into();
            let source = saved.identity(std::slice::from_ref(&saved));
            match edit {
                "host" => saved.host = "new-host.example".into(),
                "port" => saved.port = 5901,
                "username" => saved.username = Some("bob".into()),
                "password" => saved.password = "new secret".into(),
                _ => unreachable!(),
            }
            let expected = saved.clone();
            let mut sessions = vec![saved];
            assert!(!remember_connection(
                &mut sessions,
                "desktop.example",
                5900,
                Some("alice".into()),
                "old",
                Some(&source),
            ));
            assert_eq!(sessions, vec![expected]);
        }
    }

    #[test]
    fn endpoint_lookup_requires_an_unambiguous_account() {
        let mut sessions = vec![
            account("Alice", "alice", "alice secret"),
            account("Bob", "bob", "bob secret"),
        ];
        assert!(for_endpoint(&sessions, "desktop.example", 5900, None).is_none());
        assert_eq!(
            for_endpoint(&sessions, "desktop.example", 5900, Some("bob"))
                .unwrap()
                .name,
            "Bob"
        );
        assert!(for_endpoint(&sessions, "desktop.example", 5900, Some("charlie")).is_none());
        sessions.push(account("Other Bob", "bob", "other secret"));
        assert!(for_endpoint(&sessions, "desktop.example", 5900, Some("bob")).is_none());
    }

    #[test]
    fn exact_id_selects_an_unnamed_account_on_a_shared_endpoint() {
        let mut alice = account("", "alice", "alice secret");
        alice.id = "alice-id".to_string();
        let mut bob = account("", "bob", "bob secret");
        bob.id = "bob-id".to_string();
        let mut sessions = vec![alice, bob];
        let selected = find_by_id(&sessions, "bob-id").unwrap();
        assert_eq!(selected.username.as_deref(), Some("bob"));
        assert_eq!(selected.password, "bob secret");
        assert!(find_named(&sessions, "desktop.example").is_err());
        assert!(find_by_id(&sessions, "missing-id").is_err());
        assert!(find_by_id(&sessions, "").is_err());
        sessions.push(sessions[1].clone());
        assert!(find_by_id(&sessions, "bob-id").is_err());
    }

    #[test]
    fn missing_or_ambiguous_source_never_updates_another_account() {
        let bob = account("Bob", "bob", "old");
        let source = bob.identity(std::slice::from_ref(&bob));
        let mut sessions = vec![account("Alice", "alice", "alice secret")];
        let original = sessions.clone();
        assert!(!remember_connection(
            &mut sessions,
            "desktop.example",
            5900,
            Some("bob".to_string()),
            "new",
            Some(&source)
        ));
        assert_eq!(sessions, original);
        sessions = vec![bob.clone(), bob];
        let original = sessions.clone();
        assert!(!remember_connection(
            &mut sessions,
            "desktop.example",
            5900,
            Some("bob".to_string()),
            "new",
            Some(&source)
        ));
        assert_eq!(sessions, original);
    }

    #[test]
    fn ad_hoc_account_save_does_not_replace_other_endpoint_accounts() {
        let mut sessions = vec![
            account("Alice", "alice", "alice secret"),
            account("Bob", "bob", "bob secret"),
        ];
        let original = sessions.clone();
        assert!(remember_connection(
            &mut sessions,
            "desktop.example",
            5900,
            Some("charlie".to_string()),
            "charlie secret",
            None
        ));
        assert_eq!(&sessions[..2], &original);
        assert_eq!(sessions[2].username.as_deref(), Some("charlie"));
        assert_eq!(sessions[2].password, "charlie secret");
        assert_ne!(sessions[2].id, sessions[0].id);
        assert_ne!(sessions[2].id, sessions[1].id);
    }

    #[test]
    fn duplicate_id_insert_keeps_both_payloads() {
        let mut existing = account("Alice", "alice", "alice secret");
        existing.id = "shared-id".into();
        let mut incoming = account("Bob", "bob", "bob secret");
        incoming.id = "shared-id".into();
        let mut sessions = vec![existing.clone()];
        insert_session(&mut sessions, incoming);
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0], existing);
        assert_eq!(sessions[1].name, "Bob");
        assert_eq!(sessions[1].username.as_deref(), Some("bob"));
        assert_eq!(sessions[1].password, "bob secret");
        assert_ne!(sessions[1].id, "shared-id");
        assert!(!sessions[1].id.is_empty());
    }

    #[test]
    fn replace_and_remove_touch_only_the_matching_instance() {
        let mut alice = account("Alice", "alice", "alice secret");
        alice.id = "shared-id".into();
        let mut bob = account("Bob", "bob", "bob secret");
        bob.id = "shared-id".into();
        let mut sessions = vec![alice.clone(), bob.clone()];
        let mut edited = bob.clone();
        edited.name = "Bob edited".into();
        edited.password = "new bob".into();
        edited.id = "shared-id".into();
        assert!(replace_session_instance(
            &mut sessions,
            1,
            "shared-id",
            edited
        ));
        assert_eq!(sessions[0], alice);
        assert_eq!(sessions[1].name, "Bob edited");
        assert_eq!(sessions[1].password, "new bob");
        assert_eq!(sessions[1].host, bob.host);
        assert_ne!(sessions[1].id, sessions[0].id);
        assert!(!remove_indexed_instance(&mut sessions, 0, &bob));
        assert!(remove_indexed_instance(&mut sessions, 0, &alice));
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].name, "Bob edited");
        assert_eq!(sessions[0].password, "new bob");
    }

    fn shared_account(name: &str) -> Session {
        let mut session = account(name, "shared-user", "old secret");
        session.id = "shared-id".into();
        session
    }

    /// `--session A` while two rows share an ID, then one row is saved.
    /// Saving gives that row a new ID and leaves `shared-id` on the sibling.
    fn session_a_after_shared_id_save(
        edit_saved_password: bool,
    ) -> (Vec<Session>, SessionIdentity) {
        let first = shared_account("A");
        let second = shared_account("B");
        let mut sessions = vec![first.clone(), second];
        let source = find_named(&sessions, "A").unwrap().identity(&sessions);
        let mut saved = first;
        if edit_saved_password {
            saved.password = "form password".into();
        }
        assert!(replace_session_instance(
            &mut sessions,
            0,
            "shared-id",
            saved
        ));
        assert_ne!(sessions[0].id, "shared-id");
        assert_eq!(sessions[1].id, "shared-id");
        (sessions, source)
    }

    #[test]
    fn shared_id_save_does_not_write_password_into_the_reidentified_row() {
        let (mut sessions, source) = session_a_after_shared_id_save(false);
        let before = sessions.clone();
        assert!(!remember_connection(
            &mut sessions,
            "desktop.example",
            5900,
            Some("shared-user".into()),
            "password from --session A",
            Some(&source),
        ));
        assert_eq!(sessions, before);
    }

    #[test]
    fn shared_id_save_does_not_bind_session_password_to_the_sibling() {
        let (mut sessions, source) = session_a_after_shared_id_save(true);
        let before = sessions.clone();
        assert!(!remember_connection(
            &mut sessions,
            "desktop.example",
            5900,
            Some("shared-user".into()),
            "password from --session A",
            Some(&source),
        ));
        assert_eq!(sessions, before);
    }

    #[test]
    fn shared_id_does_not_write_password_into_a_recreated_row() {
        let first = shared_account("A");
        let second = shared_account("B");
        let mut sessions = vec![first.clone(), second];
        let source = find_named(&sessions, "A").unwrap().identity(&sessions);
        assert!(remove_indexed_instance(&mut sessions, 0, &first));
        let mut recreated = first;
        recreated.id.clear();
        insert_session(&mut sessions, recreated);
        let before = sessions.clone();
        assert!(!remember_connection(
            &mut sessions,
            "desktop.example",
            5900,
            Some("shared-user".into()),
            "password from --session A",
            Some(&source),
        ));
        assert_eq!(sessions, before);
        assert_eq!(sessions[0].password, "old secret");
    }

    #[test]
    fn stale_shared_id_is_not_a_window_argument() {
        let first = shared_account("A");
        let second = shared_account("B");
        let sessions = vec![first.clone(), second];
        let args = session_window_args(&sessions, &first).unwrap();
        assert!(!args
            .iter()
            .any(|arg| arg == "--session-id" || arg == "shared-id"));
        assert_eq!(
            &args[..2],
            ["--session".to_string(), "A".to_string()].as_slice()
        );
        assert!(args
            .windows(2)
            .any(|pair| pair == ["--session-host", "desktop.example"]));
        assert!(args
            .windows(2)
            .any(|pair| pair == ["--session-port", "5900"]));
        assert!(args
            .windows(2)
            .any(|pair| pair == ["--session-username", "shared-user"]));
    }

    #[test]
    fn ambiguous_shared_id_does_not_launch_a_window() {
        let first = shared_account("A");
        let mut twin = first.clone();
        twin.password = "other secret".into();
        assert!(session_window_args(&[first.clone(), twin], &first).is_none());
    }

    #[test]
    fn unique_id_launch_carries_name_and_account() {
        let mut session = account("A", "alice", "secret");
        session.id = "only-a".into();
        let args = session_window_args(std::slice::from_ref(&session), &session).unwrap();
        assert!(args
            .windows(2)
            .any(|pair| pair == ["--session-id", "only-a"]));
        assert!(args.windows(2).any(|pair| pair == ["--session-name", "A"]));
        assert!(args
            .windows(2)
            .any(|pair| pair == ["--session-host", "desktop.example"]));
        assert!(args
            .windows(2)
            .any(|pair| pair == ["--session-username", "alice"]));
    }

    #[test]
    fn launched_id_must_still_match_name_and_account() {
        let mut saved_a = shared_account("A");
        saved_a.id = "new-id".into();
        let saved_b = shared_account("B");
        let saved = vec![saved_a.clone(), saved_b];
        let account = LaunchAccount {
            host: saved_a.host.clone(),
            port: saved_a.port,
            username: saved_a.username.clone(),
        };
        assert!(find_launched_session(&saved, "shared-id", "A", &account).is_err());
        assert_eq!(
            find_launched_session(&saved, "new-id", "A", &account)
                .unwrap()
                .name,
            "A"
        );
        let named = find_named_account(&saved, "A", Some(&account)).unwrap();
        assert_eq!(named.id, "new-id");
        let other_account = LaunchAccount {
            host: "other.example".into(),
            port: 5901,
            username: Some("other".into()),
        };
        assert!(find_named_account(&saved, "A", Some(&other_account)).is_err());
    }

    fn insert_locked(sessions: &std::sync::Mutex<Vec<Session>>, session: Session) -> usize {
        let mut guard = sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        insert_session(&mut guard, session)
    }

    fn remove_matching_locked(
        sessions: &std::sync::Mutex<Vec<Session>>,
        expected: &Session,
    ) -> bool {
        let mut guard = sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(index) = guard.iter().position(|session| session == expected) else {
            return false;
        };
        remove_indexed_instance(&mut guard, index, expected)
    }

    #[test]
    fn concurrent_duplicate_ids_do_not_overwrite_or_remove_the_other_instance() {
        use std::sync::{Arc, Barrier, Mutex};
        use std::thread;

        let sessions = Arc::new(Mutex::new(Vec::new()));
        let writers = 8;
        let start = Arc::new(Barrier::new(writers));
        let mut handles = Vec::new();
        for index in 0..writers {
            let sessions = Arc::clone(&sessions);
            let start = Arc::clone(&start);
            handles.push(thread::spawn(move || {
                start.wait();
                insert_locked(
                    &sessions,
                    Session {
                        id: "shared-id".into(),
                        name: format!("account-{index}"),
                        host: "desktop.example".into(),
                        port: 5900,
                        username: Some(format!("user-{index}")),
                        password: format!("secret-{index}"),
                        ssh_user: String::new(),
                    },
                )
            }));
        }
        for handle in handles {
            handle.join().expect("insert thread");
        }

        let stored = sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        assert_eq!(stored.len(), writers);
        let mut ids: Vec<&str> = stored.iter().map(|session| session.id.as_str()).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(
            ids.len(),
            writers,
            "duplicate IDs were stored or overwritten"
        );
        assert_eq!(
            stored
                .iter()
                .filter(|session| session.id == "shared-id")
                .count(),
            1
        );
        for index in 0..writers {
            let name = format!("account-{index}");
            let username = format!("user-{index}");
            let session = stored
                .iter()
                .find(|session| session.name == name)
                .unwrap_or_else(|| panic!("missing {name}"));
            assert_eq!(session.password, format!("secret-{index}"));
            assert_eq!(session.username.as_deref(), Some(username.as_str()));
        }

        let alice = stored[0].clone();
        let bob = stored[1].clone();
        assert_ne!(alice, bob);
        let removed_names = [alice.name.clone(), bob.name.clone()];
        let start = Arc::new(Barrier::new(3));
        let mut handles = Vec::new();
        for expected in [alice.clone(), alice.clone(), bob.clone()] {
            let sessions = Arc::clone(&sessions);
            let start = Arc::clone(&start);
            handles.push(thread::spawn(move || {
                start.wait();
                remove_matching_locked(&sessions, &expected)
            }));
        }
        let removed: Vec<bool> = handles
            .into_iter()
            .map(|handle| handle.join().expect("remove thread"))
            .collect();
        assert_eq!(removed.iter().filter(|removed| **removed).count(), 2);
        let stored = sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        assert_eq!(stored.len(), writers - 2);
        assert!(stored
            .iter()
            .all(|session| !removed_names.contains(&session.name)));
        for session in &stored {
            assert_eq!(
                stored.iter().filter(|other| other.id == session.id).count(),
                1
            );
        }
    }
}
