//! Credential vault backed by macOS Keychain.
//!
//! Every non-Discord-bot channel persists its credentials here under a
//! consistent naming scheme: `augmentagent/<platform>/<account>`.
//!
//! - `platform` is lowercase ASCII (`linkedin`, `slack`, `whatsapp`, …).
//! - `account` is the platform-native identifier for the specific account
//!   (LinkedIn member URN, Slack workspace ID, phone number, etc.) or the
//!   [`DEFAULT_ACCOUNT`] sentinel when the channel is single-account.
//!
//! Payload is opaque bytes — each channel serializes its own credential
//! shape (typically JSON).
//!
//! #1284 adds an injectable seam, [`CredentialStore`], so callers (and their
//! tests) can swap the backend: [`KeychainCredentialStore`] is the platform
//! keyring, [`MemoryCredentialStore`] is for unit tests, and
//! [`FileCredentialStore`] is an explicitly insecure plaintext directory used
//! only when [`INSECURE_FILE_STORE_ENV`] is set (end-to-end CLI tests and
//! local QA against a mock Slack; never for real secrets). The static
//! [`Auth`] helpers go through [`default_store`], so every channel honours
//! the same override.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Once};

use thiserror::Error;
use tracing::debug;

/// When set to a non-empty directory path, [`default_store`] stores
/// credentials as plaintext files under it instead of the Keychain/keyring.
/// Test and QA use only: the files are owner-only but unencrypted.
pub const INSECURE_FILE_STORE_ENV: &str = "AUGMENTAGENT_INSECURE_CREDENTIAL_DIR";

/// Convention for single-account platforms (one LinkedIn profile, one
/// default Slack workspace). Multi-account channels pass their own
/// account identifiers.
pub const DEFAULT_ACCOUNT: &str = "default";

/// Service-name prefix. Combined with `<platform>` to form the Keychain
/// `service` attribute so entries sort under a single namespace.
const SERVICE_PREFIX: &str = "augmentagent/";

#[derive(Debug, Error)]
pub enum AuthError {
    #[error("not found: augmentagent/{platform}/{account}")]
    NotFound { platform: String, account: String },

    #[error("keyring: {0}")]
    Keyring(#[from] keyring::Error),
}

/// Credential store. Stateless — all methods take (platform, account) keys
/// and go through [`default_store`].
pub struct Auth;

impl Auth {
    /// Store opaque bytes at `service=augmentagent/<platform>`, `user=<account>`.
    /// Overwrites any existing entry.
    pub fn put(platform: &str, account: &str, payload: &[u8]) -> Result<(), AuthError> {
        default_store().put(platform, account, payload)
    }

    /// Retrieve bytes. Returns [`AuthError::NotFound`] when the entry is missing.
    pub fn get(platform: &str, account: &str) -> Result<Vec<u8>, AuthError> {
        default_store().get(platform, account)
    }

    /// Delete an entry. Idempotent — a missing entry is treated as success.
    pub fn delete(platform: &str, account: &str) -> Result<(), AuthError> {
        default_store().delete(platform, account)
    }

    /// True only when the entry is known to exist ([`Presence::Present`]).
    /// On macOS this reads the item (keyring 3 has no attribute-only
    /// lookup), which can show the Keychain "allow access" prompt for a
    /// binary the item does not trust. A store this process cannot read
    /// counts as absent; use [`Auth::presence`] to tell the two apart.
    pub fn exists(platform: &str, account: &str) -> bool {
        default_store().exists(platform, account)
    }

    /// #1299 — present, missing, or unreadable from this process.
    pub fn presence(platform: &str, account: &str) -> Presence {
        default_store().presence(platform, account)
    }
}

/// #1299 — whether a credential slot exists, as far as this process can
/// tell. `Unreadable` means the store refused or failed (a locked or
/// unavailable Keychain, a session without access to it); it says nothing
/// about whether the item exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Presence {
    Present,
    Missing,
    /// Sanitized reason from the platform store. Never secret bytes.
    Unreadable(String),
}

/// Longest `Presence::Unreadable` reason kept.
const MAX_REASON: usize = 200;

/// Map a keyring lookup result to [`Presence`]. `NoEntry` is missing; an
/// ambiguous match or a non-UTF-8 secret still proves an item is there;
/// every other error is unreadable, with the platform's message (bounded,
/// never the secret) as the reason.
pub fn presence_from_keyring(result: Result<(), keyring::Error>) -> Presence {
    let reason = match result {
        Ok(()) => return Presence::Present,
        Err(keyring::Error::NoEntry) => return Presence::Missing,
        Err(keyring::Error::Ambiguous(_)) | Err(keyring::Error::BadEncoding(_)) => {
            return Presence::Present
        }
        Err(keyring::Error::NoStorageAccess(e)) => {
            format!("no access to the credential store: {e}")
        }
        Err(keyring::Error::PlatformFailure(e)) => format!("credential store failure: {e}"),
        Err(keyring::Error::TooLong(attr, _)) => format!("invalid credential key: {attr} too long"),
        Err(keyring::Error::Invalid(attr, why)) => format!("invalid credential key {attr}: {why}"),
        Err(e) => format!("credential store error: {e}"),
    };
    Presence::Unreadable(bounded(&reason))
}

fn bounded(reason: &str) -> String {
    let one_line = reason.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.len() <= MAX_REASON {
        return one_line;
    }
    let mut end = MAX_REASON;
    while !one_line.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &one_line[..end])
}

/// Where credentials live. Object-safe so callers hold an
/// `Arc<dyn CredentialStore>` and tests inject [`MemoryCredentialStore`].
pub trait CredentialStore: Send + Sync {
    /// Short backend label for status output (`keychain`, `memory`,
    /// `insecure-file`). Never contains a secret.
    fn backend(&self) -> &'static str;
    fn put(&self, platform: &str, account: &str, payload: &[u8]) -> Result<(), AuthError>;
    fn get(&self, platform: &str, account: &str) -> Result<Vec<u8>, AuthError>;
    /// Idempotent: a missing entry is success.
    fn delete(&self, platform: &str, account: &str) -> Result<(), AuthError>;
    /// True only when the entry is known to exist. May read the item.
    fn exists(&self, platform: &str, account: &str) -> bool;
    /// #1299 — present, missing or unreadable. Stores that can tell an
    /// unreadable store from a missing entry override this; the default
    /// follows [`exists`](Self::exists).
    fn presence(&self, platform: &str, account: &str) -> Presence {
        if self.exists(platform, account) {
            Presence::Present
        } else {
            Presence::Missing
        }
    }
}

/// The store the process should use: [`FileCredentialStore`] when
/// [`INSECURE_FILE_STORE_ENV`] is set, otherwise the platform keyring.
pub fn default_store() -> Arc<dyn CredentialStore> {
    store_for_override(std::env::var_os(INSECURE_FILE_STORE_ENV).as_deref())
}

/// Pure selection rule behind [`default_store`] (testable without touching
/// the process environment). An empty value means "not set".
pub fn store_for_override(dir: Option<&OsStr>) -> Arc<dyn CredentialStore> {
    match dir.filter(|d| !d.is_empty()) {
        Some(dir) => {
            static WARN: Once = Once::new();
            WARN.call_once(|| {
                tracing::warn!(
                    "{INSECURE_FILE_STORE_ENV} is set: credentials are stored as plaintext \
                     files, not in the Keychain/keyring (tests and local QA only)"
                );
            });
            Arc::new(FileCredentialStore::new(Path::new(dir)))
        }
        None => Arc::new(KeychainCredentialStore),
    }
}

/// #1299 / #1325 — which credential backend a process uses, for `doctor` and
/// `status`. Never contains a secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendDescription {
    /// `macos-keychain`, `secret-service`, `keyutils`, `keyring-mock`,
    /// `insecure-file`, …
    pub backend: &'static str,
    /// Credentials outlive the process that stored them.
    pub persistent: bool,
    /// Plaintext files from [`INSECURE_FILE_STORE_ENV`].
    pub insecure: bool,
    /// Caveat for the operator, when there is one.
    pub note: Option<String>,
}

/// Describe the store [`default_store`] selects in this process.
pub fn describe_default_store() -> BackendDescription {
    describe_store_for_override(std::env::var_os(INSECURE_FILE_STORE_ENV).as_deref())
}

/// Pure counterpart of [`describe_default_store`], same rule as
/// [`store_for_override`]. Reads nothing from the store.
pub fn describe_store_for_override(dir: Option<&OsStr>) -> BackendDescription {
    if dir.is_some_and(|d| !d.is_empty()) {
        return BackendDescription {
            backend: "insecure-file",
            persistent: true,
            insecure: true,
            note: Some(format!(
                "{INSECURE_FILE_STORE_ENV} is set: credentials are plaintext files (tests and \
                 local QA only)"
            )),
        };
    }
    use keyring::credential::CredentialPersistence;
    let persistence = keyring::default::default_credential_builder().persistence();
    let persistent = matches!(persistence, CredentialPersistence::UntilDelete);
    let backend = if cfg!(target_os = "macos") {
        "macos-keychain"
    } else {
        match persistence {
            CredentialPersistence::UntilDelete => "platform-keyring",
            CredentialPersistence::UntilReboot => "keyutils",
            _ => "keyring-mock",
        }
    };
    let note = (!persistent).then(|| {
        "credentials do not outlive the process that stored them: the keyring crate is built \
         without a persistent backend for this OS (#1325)"
            .to_string()
    });
    BackendDescription {
        backend,
        persistent,
        insecure: false,
        note,
    }
}

/// macOS Keychain / platform keyring via the `keyring` crate.
#[derive(Debug, Default, Clone, Copy)]
pub struct KeychainCredentialStore;

impl CredentialStore for KeychainCredentialStore {
    fn backend(&self) -> &'static str {
        "keychain"
    }

    fn put(&self, platform: &str, account: &str, payload: &[u8]) -> Result<(), AuthError> {
        let entry = entry_for(platform, account)?;
        entry.set_secret(payload)?;
        debug!(platform, account, bytes = payload.len(), "auth put");
        Ok(())
    }

    fn get(&self, platform: &str, account: &str) -> Result<Vec<u8>, AuthError> {
        let entry = entry_for(platform, account)?;
        match entry.get_secret() {
            Ok(bytes) => {
                debug!(platform, account, bytes = bytes.len(), "auth get");
                Ok(bytes)
            }
            Err(keyring::Error::NoEntry) => Err(not_found(platform, account)),
            Err(e) => Err(AuthError::Keyring(e)),
        }
    }

    fn delete(&self, platform: &str, account: &str) -> Result<(), AuthError> {
        let entry = entry_for(platform, account)?;
        match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => {
                debug!(platform, account, "auth delete");
                Ok(())
            }
            Err(e) => Err(AuthError::Keyring(e)),
        }
    }

    fn exists(&self, platform: &str, account: &str) -> bool {
        self.presence(platform, account) == Presence::Present
    }

    /// On macOS keyring 3's attribute lookup reads the item, so this is a
    /// read (which may prompt); any failure other than "no such item" is
    /// `Unreadable`, never `Present` (#1299).
    fn presence(&self, platform: &str, account: &str) -> Presence {
        let entry = match entry_for(platform, account) {
            Ok(e) => e,
            Err(AuthError::Keyring(e)) => return presence_from_keyring(Err(e)),
            Err(e) => return Presence::Unreadable(bounded(&e.to_string())),
        };
        presence_from_keyring(entry.get_attributes().map(|_| ()))
    }
}

type Slots = BTreeMap<(String, String), Vec<u8>>;

/// In-process store for tests. Slots are shared across clones.
#[derive(Debug, Default, Clone)]
pub struct MemoryCredentialStore {
    slots: Arc<Mutex<Slots>>,
}

impl MemoryCredentialStore {
    /// Every stored payload, for tests that assert what was (not) written.
    pub fn payloads(&self) -> Vec<Vec<u8>> {
        self.slots.lock().unwrap().values().cloned().collect()
    }
}

impl CredentialStore for MemoryCredentialStore {
    fn backend(&self) -> &'static str {
        "memory"
    }

    fn put(&self, platform: &str, account: &str, payload: &[u8]) -> Result<(), AuthError> {
        self.slots
            .lock()
            .unwrap()
            .insert((platform.into(), account.into()), payload.to_vec());
        Ok(())
    }

    fn get(&self, platform: &str, account: &str) -> Result<Vec<u8>, AuthError> {
        self.slots
            .lock()
            .unwrap()
            .get(&(platform.into(), account.into()))
            .cloned()
            .ok_or_else(|| not_found(platform, account))
    }

    fn delete(&self, platform: &str, account: &str) -> Result<(), AuthError> {
        self.slots
            .lock()
            .unwrap()
            .remove(&(platform.to_string(), account.to_string()));
        Ok(())
    }

    fn exists(&self, platform: &str, account: &str) -> bool {
        self.slots
            .lock()
            .unwrap()
            .contains_key(&(platform.into(), account.into()))
    }
}

/// Plaintext files at `<dir>/<platform>/<account>` (percent-encoded), dir
/// `0700` and files `0600` on Unix. **Insecure** — for tests and local QA
/// only; selected by [`INSECURE_FILE_STORE_ENV`].
#[derive(Debug, Clone)]
pub struct FileCredentialStore {
    dir: PathBuf,
}

impl FileCredentialStore {
    pub fn new(dir: impl AsRef<Path>) -> Self {
        Self {
            dir: dir.as_ref().to_path_buf(),
        }
    }

    fn path_for(&self, platform: &str, account: &str) -> Result<PathBuf, AuthError> {
        Ok(self
            .dir
            .join(encode_component("platform", platform)?)
            .join(encode_component("account", account)?))
    }
}

fn encode_component(what: &str, raw: &str) -> Result<String, AuthError> {
    if raw.is_empty() || raw == "." || raw == ".." {
        return Err(AuthError::Keyring(keyring::Error::Invalid(
            what.into(),
            format!("{raw:?} is not a valid credential key"),
        )));
    }
    let mut out = String::with_capacity(raw.len());
    for b in raw.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    Ok(out)
}

fn io_err(e: std::io::Error) -> AuthError {
    AuthError::Keyring(keyring::Error::PlatformFailure(Box::new(e)))
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> std::io::Result<()> {
    Ok(())
}

impl CredentialStore for FileCredentialStore {
    fn backend(&self) -> &'static str {
        "insecure-file"
    }

    fn put(&self, platform: &str, account: &str, payload: &[u8]) -> Result<(), AuthError> {
        let path = self.path_for(platform, account)?;
        let parent = path.parent().expect("path has a platform directory");
        std::fs::create_dir_all(parent).map_err(io_err)?;
        set_mode(&self.dir, 0o700).map_err(io_err)?;
        set_mode(parent, 0o700).map_err(io_err)?;
        let tmp = parent.join(format!(
            ".{}.tmp-{}",
            path.file_name().unwrap().to_string_lossy(),
            std::process::id()
        ));
        {
            use std::io::Write;
            let mut opts = std::fs::OpenOptions::new();
            opts.write(true).create(true).truncate(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o600);
            }
            let mut f = opts.open(&tmp).map_err(io_err)?;
            f.write_all(payload).map_err(io_err)?;
            f.sync_all().map_err(io_err)?;
        }
        set_mode(&tmp, 0o600).map_err(io_err)?;
        std::fs::rename(&tmp, &path).map_err(io_err)?;
        debug!(platform, account, bytes = payload.len(), "auth put (file)");
        Ok(())
    }

    fn get(&self, platform: &str, account: &str) -> Result<Vec<u8>, AuthError> {
        let path = self.path_for(platform, account)?;
        match std::fs::read(&path) {
            Ok(bytes) => Ok(bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(not_found(platform, account)),
            Err(e) => Err(io_err(e)),
        }
    }

    fn delete(&self, platform: &str, account: &str) -> Result<(), AuthError> {
        let path = self.path_for(platform, account)?;
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(io_err(e)),
        }
    }

    fn exists(&self, platform: &str, account: &str) -> bool {
        self.presence(platform, account) == Presence::Present
    }

    fn presence(&self, platform: &str, account: &str) -> Presence {
        let path = match self.path_for(platform, account) {
            Ok(p) => p,
            Err(e) => return Presence::Unreadable(bounded(&e.to_string())),
        };
        match std::fs::metadata(&path) {
            Ok(m) if m.is_file() => Presence::Present,
            Ok(_) => Presence::Missing,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Presence::Missing,
            Err(e) => {
                Presence::Unreadable(bounded(&format!("credential file store: {}", e.kind())))
            }
        }
    }
}

fn not_found(platform: &str, account: &str) -> AuthError {
    AuthError::NotFound {
        platform: platform.into(),
        account: account.into(),
    }
}

fn entry_for(platform: &str, account: &str) -> Result<keyring::Entry, AuthError> {
    let service = format!("{SERVICE_PREFIX}{platform}");
    Ok(keyring::Entry::new(&service, account)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use keyring::{mock, set_default_credential_builder};
    use std::sync::Once;

    static INIT: Once = Once::new();

    /// Install the in-memory mock backend once per process. `keyring::mock`
    /// entries are isolated per `Entry::new` call (each Entry gets its own
    /// in-memory credential; there is no shared store), so we use the mock
    /// only to exercise the error-mapping paths in `Auth` without touching
    /// the real Keychain. Round-trip put→get coverage lives in the
    /// `#[ignore]`-gated integration tests below.
    fn install_mock() {
        INIT.call_once(|| {
            set_default_credential_builder(mock::default_credential_builder());
        });
    }

    #[test]
    fn default_account_is_plain_string() {
        assert_eq!(DEFAULT_ACCOUNT, "default");
    }

    #[test]
    fn service_prefix_is_stable() {
        // Silent drift on this prefix would orphan every previously-written
        // Keychain entry. The const is load-bearing across builds.
        assert_eq!(SERVICE_PREFIX, "augmentagent/");
    }

    #[test]
    fn get_missing_returns_not_found() {
        install_mock();
        let err = Auth::get("testplat", "missing-user").unwrap_err();
        let AuthError::NotFound { platform, account } = err else {
            panic!("expected NotFound, got {err:?}");
        };
        assert_eq!(platform, "testplat");
        assert_eq!(account, "missing-user");
    }

    #[test]
    fn delete_is_idempotent_when_missing() {
        install_mock();
        // Never written — must not error.
        Auth::delete("testplat", "never-existed").unwrap();
    }

    #[test]
    fn entry_for_builds_prefixed_service() {
        // Construct an Entry through the same helper Auth uses; read its
        // attributes and confirm the service attribute carries the namespace
        // prefix. `get_attributes` returns NoEntry on the mock (nothing was
        // written), which is the only thing we care to assert here.
        install_mock();
        let entry = entry_for("linkedin", DEFAULT_ACCOUNT).unwrap();
        assert!(matches!(
            entry.get_attributes(),
            Err(keyring::Error::NoEntry)
        ));
    }

    // ---------------------------------------------------------------------
    // Integration tests — hit the real macOS Keychain. Opt-in via
    //   cargo test -p augmentagent-auth -- --ignored
    // Each test uses a unique service/account pair so parallel runs don't
    // collide, and every test deletes its entry in a scope-end guard.
    // ---------------------------------------------------------------------

    /// Cleans up a Keychain entry on drop so a panicking test doesn't leak.
    struct Cleanup {
        platform: String,
        account: String,
    }

    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = Auth::delete(&self.platform, &self.account);
        }
    }

    fn unique(prefix: &str) -> String {
        // Nanos since epoch — plenty of entropy for parallel test runs on
        // one machine; avoids pulling in uuid as a dev-dep for this alone.
        let ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        format!("{prefix}-{ns}")
    }

    #[test]
    #[ignore = "hits real macOS Keychain; run with --ignored"]
    fn integration_put_get_round_trip() {
        let platform = unique("augmentagent-test-roundtrip");
        let account = "default";
        let _c = Cleanup {
            platform: platform.clone(),
            account: account.into(),
        };

        Auth::put(&platform, account, b"secret-bytes").unwrap();
        assert_eq!(Auth::get(&platform, account).unwrap(), b"secret-bytes");
    }

    #[test]
    #[ignore = "hits real macOS Keychain; run with --ignored"]
    fn integration_overwrite_replaces_value() {
        let platform = unique("augmentagent-test-overwrite");
        let account = "default";
        let _c = Cleanup {
            platform: platform.clone(),
            account: account.into(),
        };

        Auth::put(&platform, account, b"first").unwrap();
        Auth::put(&platform, account, b"second").unwrap();
        assert_eq!(Auth::get(&platform, account).unwrap(), b"second");
    }

    #[test]
    #[ignore = "hits real macOS Keychain; run with --ignored"]
    fn integration_delete_removes_entry() {
        let platform = unique("augmentagent-test-delete");
        let account = "default";
        let _c = Cleanup {
            platform: platform.clone(),
            account: account.into(),
        };

        Auth::put(&platform, account, b"x").unwrap();
        Auth::delete(&platform, account).unwrap();
        assert!(matches!(
            Auth::get(&platform, account),
            Err(AuthError::NotFound { .. })
        ));
    }
}
