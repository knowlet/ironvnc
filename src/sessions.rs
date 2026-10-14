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
    pub fn identity(&self) -> SessionIdentity {
        SessionIdentity {
            id: self.id.clone(),
            name: self.name.clone(),
            host: self.host.clone(),
            port: self.port,
            username: self.username.clone(),
            password: self.password.clone(),
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
            .finish()
    }
}

impl SessionIdentity {
    fn matches(&self, session: &Session) -> bool {
        if !self.id.is_empty() {
            // A label can change while connecting. An endpoint or account
            // edit invalidates this in-flight connection's credential snapshot.
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

pub fn new_id() -> String {
    format!("{:032x}", rand::random::<u128>())
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
        let session = &mut sessions[i];
        let mut changed = false;
        if session.id.is_empty() {
            session.id = new_id();
            changed = true;
        }
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
        sessions.push(Session {
            id: new_id(),
            name: String::new(),
            host: host.to_string(),
            port,
            username,
            password: password.to_string(),
            ssh_user: String::new(),
        });
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
        let source = find_named(&sessions, "Bob").unwrap().identity();
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
        let source = bob.identity();
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
            let source = saved.identity();
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
        let source = bob.identity();
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
    }
}
