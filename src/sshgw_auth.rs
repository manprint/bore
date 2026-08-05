//! Credential stores for the SSH gateway: an authorized-keys directory and an
//! argon2id password file. Both re-read the filesystem on every authentication
//! attempt (hot reload by construction, cached by mtime) so operators can add
//! or revoke credentials without restarting `bore server`. See
//! `docs/SSH_GATEWAY.md` §2.9/§2.10 and `docs/plans/plan_SshGateway/phase_03.md`.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use anyhow::{bail, Result};
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use russh::keys::ssh_key::authorized_keys::{AuthorizedKeys, Entry};
use russh::keys::ssh_key::public::KeyData;
use russh::keys::{HashAlg, PublicKey};
use tokio::sync::Semaphore;

/// Number of concurrent argon2id verifications allowed at once. Argon2id is
/// deliberately slow (memory-hard); an unbounded fan-out of concurrent
/// verifications under credential stuffing would let attackers burn CPU.
pub const PASSWORD_VERIFY_CONCURRENCY: usize = 2;

/// Grant returned for an offered public key that matches a stored
/// authorized-keys entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyGrant {
    /// The matching line's comment, or `SHA256:<fingerprint>` if it has none.
    pub identity: String,
    /// `permit="csv"` option values (e.g. `vhost/<glob>`, `secret/<glob>`,
    /// `port/<n>` or `port/<a>-<b>`), if the line set one.
    pub permit: Option<Vec<String>>,
    /// `max-conns=<n>` option value, if the line set one.
    pub max_conns: Option<usize>,
    /// `notes="..."` option value, if the line set one.
    pub notes: Option<String>,
}

/// Public-key authentication result with jump-only classic account metadata.
/// The legacy grant remains exactly the result used by every existing mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyAuthMatch {
    /// Existing comment/fingerprint identity and authorized-key options.
    pub grant: KeyGrant,
    /// Presented username when the matching key also lives in `<user>` or
    /// `<user>.pub`; `None` for a legacy-only username mismatch.
    pub jump_principal: Option<String>,
}

/// Password authentication result with jump-only classic account metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PasswordAuthMatch {
    /// Existing first matching password-file label.
    pub identity: String,
    /// Presented username when its exact label also verifies this password.
    pub jump_principal: Option<String>,
}

/// One parsed, cached authorized-keys line.
#[derive(Debug, Clone)]
struct ParsedKey {
    key_data: KeyData,
    identity: String,
    permit: Option<Vec<String>>,
    max_conns: Option<usize>,
    notes: Option<String>,
}

/// A candidate authorized-keys file: `*.pub`, or a file with no extension at
/// all (bare label files such as `alice`).
fn is_candidate_file(path: &Path) -> bool {
    match path.extension() {
        Some(ext) => ext == "pub",
        None => true,
    }
}

/// Strip one layer of surrounding double quotes, if present.
fn unquote(value: &str) -> String {
    let bytes = value.as_bytes();
    if bytes.len() >= 2 && bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"' {
        value[1..value.len() - 1].to_string()
    } else {
        value.to_string()
    }
}

/// Parse one successfully-tokenized `authorized_keys` entry. Unknown option
/// names are recorded (not dropped) so the caller can log a single warning —
/// an unknown option never invalidates the line (forward compatibility).
fn parse_entry(entry: &Entry, unknown_options: &mut Vec<String>) -> ParsedKey {
    let public_key = entry.public_key();
    let comment = public_key.comment();
    let identity = if comment.is_empty() {
        public_key.fingerprint(HashAlg::Sha256).to_string()
    } else {
        comment.to_string()
    };

    let mut permit = None;
    let mut max_conns = None;
    let mut notes = None;

    for token in entry.config_opts().iter() {
        let (name, raw_value) = match token.split_once('=') {
            Some((name, value)) => (name, Some(unquote(value))),
            None => (token, None),
        };
        match (name, raw_value) {
            ("permit", Some(value)) => {
                permit = Some(value.split(',').map(str::to_string).collect());
            }
            ("max-conns", Some(value)) => match value.parse() {
                Ok(n) => max_conns = Some(n),
                Err(_) => unknown_options.push(format!("max-conns={value}")),
            },
            ("notes", Some(value)) => notes = Some(value),
            (name, _) => unknown_options.push(name.to_string()),
        }
    }

    ParsedKey {
        key_data: public_key.key_data().clone(),
        identity,
        permit,
        max_conns,
        notes,
    }
}

/// Parse one authorized-keys-format file. Never aborts on a bad line or an
/// unreadable file — returns whatever it could parse, and warns once for the
/// whole file about anything it skipped.
fn parse_file(path: &Path) -> Vec<ParsedKey> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) => {
            tracing::warn!(
                path = %path.display(),
                %error,
                "ssh-gateway: unreadable authorized_keys file, skipping"
            );
            return Vec::new();
        }
    };

    let mut parsed = Vec::new();
    let mut malformed = false;
    let mut unknown_options = Vec::new();

    for result in AuthorizedKeys::new(&content) {
        match result {
            Ok(entry) => parsed.push(parse_entry(&entry, &mut unknown_options)),
            Err(_) => malformed = true,
        }
    }

    if malformed {
        tracing::warn!(
            path = %path.display(),
            "ssh-gateway: skipped malformed line(s) in authorized_keys file"
        );
    }
    if !unknown_options.is_empty() {
        tracing::warn!(
            path = %path.display(),
            options = ?unknown_options,
            "ssh-gateway: unknown authorized_keys option(s), keeping line(s) valid"
        );
    }

    parsed
}

type ParsedKeyCache = HashMap<PathBuf, (SystemTime, Vec<ParsedKey>)>;
type ParsedKeyCacheGuard<'a> = std::sync::MutexGuard<'a, ParsedKeyCache>;

/// Hot-reloading directory of `authorized_keys`-format files. [`KeyStore::check`]
/// re-scans the directory every call, re-parsing only files whose mtime
/// changed since the last check (an idle directory costs one `read_dir` plus
/// one `stat` per file per check).
pub struct KeyStore {
    dir: PathBuf,
    cache: Mutex<ParsedKeyCache>,
    /// Counts calls to [`parse_file`], test-only so `keystore_mtime_cache` can
    /// assert an unchanged mtime never triggers a re-parse.
    #[cfg(test)]
    parse_calls: AtomicUsize,
}

impl KeyStore {
    /// A store reading `*.pub` and extensionless files from `dir`.
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            cache: Mutex::new(HashMap::new()),
            #[cfg(test)]
            parse_calls: AtomicUsize::new(0),
        }
    }

    fn refresh(&self) -> Option<ParsedKeyCacheGuard<'_>> {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return None;
        };
        let mut cache = self.cache.lock().expect("keystore cache mutex");
        let mut seen = HashSet::new();

        for entry in entries.flatten() {
            let path = entry.path();
            if !is_candidate_file(&path) || !path.is_file() {
                continue;
            }
            let Ok(metadata) = std::fs::metadata(&path) else {
                continue;
            };
            let Ok(mtime) = metadata.modified() else {
                continue;
            };
            seen.insert(path.clone());

            let stale = cache
                .get(&path)
                .map(|(cached, _)| *cached != mtime)
                .unwrap_or(true);
            if stale {
                #[cfg(test)]
                self.parse_calls.fetch_add(1, Ordering::Relaxed);
                let parsed = parse_file(&path);
                cache.insert(path, (mtime, parsed));
            }
        }
        cache.retain(|path, _| seen.contains(path));

        Some(cache)
    }

    fn grant(key: &ParsedKey) -> KeyGrant {
        KeyGrant {
            identity: key.identity.clone(),
            permit: key.permit.clone(),
            max_conns: key.max_conns,
            notes: key.notes.clone(),
        }
    }

    fn filename_account(path: &Path) -> Option<&str> {
        let name = path.file_name()?.to_str()?;
        match path.extension() {
            Some(ext) if ext == "pub" => name.strip_suffix(".pub"),
            None => Some(name),
            _ => None,
        }
    }

    /// Check an offered public key against every entry in the directory.
    /// Returns the first matching grant, or `None` if nothing grants it.
    pub fn check(&self, offered: &PublicKey) -> Option<KeyGrant> {
        let cache = self.refresh()?;

        let offered_data = offered.key_data();
        cache
            .values()
            .flat_map(|(_, keys)| keys)
            .find(|key| &key.key_data == offered_data)
            .map(Self::grant)
    }

    /// Authenticate with legacy key semantics and independently determine
    /// whether the key is bound to the exact presented username for jump use.
    pub fn check_for_user(&self, offered: &PublicKey, username: &str) -> Option<KeyAuthMatch> {
        let cache = self.refresh()?;
        let offered_data = offered.key_data();
        let grant = cache
            .values()
            .flat_map(|(_, keys)| keys)
            .find(|key| &key.key_data == offered_data)
            .map(Self::grant)?;
        let bound = cache.iter().any(|(path, (_, keys))| {
            Self::filename_account(path) == Some(username)
                && keys.iter().any(|key| &key.key_data == offered_data)
        });
        Some(KeyAuthMatch {
            grant,
            jump_principal: bound.then(|| username.to_string()),
        })
    }
}

/// Verify `password` against one argon2id PHC hash string.
fn verify_argon2id(password: &str, hash: &str) -> bool {
    match PasswordHash::new(hash) {
        Ok(parsed) => Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok(),
        Err(_) => false,
    }
}

/// Hot-reloading argon2id password file: `label:$argon2id$...` per line,
/// blank lines and `#`-prefixed comments ignored. [`PasswordStore::check`]
/// re-reads the file when its mtime changes and verifies under a
/// concurrency-capped `spawn_blocking` (argon2id is CPU-bound; the cap bounds
/// CPU usage under credential-stuffing).
pub struct PasswordStore {
    path: PathBuf,
    cache: Mutex<(SystemTime, Vec<(String, String)>)>,
    verify_permits: Arc<Semaphore>,
    /// In-flight / peak concurrent verifying `check()` calls, test-only so
    /// `password_verify_concurrency_capped` can assert the cap is respected.
    #[cfg(test)]
    active_verifies: AtomicUsize,
    #[cfg(test)]
    peak_verifies: AtomicUsize,
}

impl PasswordStore {
    /// A store reading `label:hash` lines from `path`.
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            cache: Mutex::new((SystemTime::UNIX_EPOCH, Vec::new())),
            verify_permits: Arc::new(Semaphore::new(PASSWORD_VERIFY_CONCURRENCY)),
            #[cfg(test)]
            active_verifies: AtomicUsize::new(0),
            #[cfg(test)]
            peak_verifies: AtomicUsize::new(0),
        }
    }

    /// Reload the password file if its mtime changed; `None` if it cannot be
    /// read at all.
    fn load(&self) -> Option<Vec<(String, String)>> {
        let mtime = std::fs::metadata(&self.path)
            .and_then(|m| m.modified())
            .ok()?;
        let mut cache = self.cache.lock().expect("password store cache mutex");
        if cache.0 != mtime {
            let content = std::fs::read_to_string(&self.path).ok()?;
            let mut parsed = Vec::new();
            let mut skipped = false;
            for line in content.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                match line.split_once(':') {
                    Some((label, hash)) if hash.starts_with("$argon2id$") => {
                        parsed.push((label.to_string(), hash.to_string()));
                    }
                    _ => skipped = true,
                }
            }
            if skipped {
                tracing::warn!(
                    path = %self.path.display(),
                    "ssh-gateway: skipped non-argon2id or malformed line(s) in password file"
                );
            }
            *cache = (mtime, parsed);
        }
        Some(cache.1.clone())
    }

    async fn verify_lines<R, F>(&self, verify: F) -> Option<R>
    where
        R: Send + 'static,
        F: FnOnce(Vec<(String, String)>) -> R + Send + 'static,
    {
        let lines = self.load()?;
        let _permit = self.verify_permits.acquire().await.ok()?;

        #[cfg(test)]
        {
            let active = self.active_verifies.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak_verifies.fetch_max(active, Ordering::SeqCst);
        }

        let result = tokio::task::spawn_blocking(move || verify(lines))
            .await
            .ok();

        #[cfg(test)]
        self.active_verifies.fetch_sub(1, Ordering::SeqCst);

        result
    }

    /// Verify `password` against every stored hash; returns the label of the
    /// first match, or `None`.
    pub async fn check(&self, password: &str) -> Option<String> {
        let password = password.to_string();
        self.verify_lines(move |lines| {
            lines
                .into_iter()
                .find(|(_, hash)| verify_argon2id(&password, hash))
                .map(|(label, _)| label)
        })
        .await
        .flatten()
    }

    /// Authenticate with legacy password semantics and independently determine
    /// whether the exact presented username label verifies for jump use.
    pub async fn check_for_user(
        &self,
        password: &str,
        username: &str,
    ) -> Option<PasswordAuthMatch> {
        let password = password.to_string();
        let username = username.to_string();
        self.verify_lines(move |lines| {
            let mut identity = None;
            let mut bound = false;
            for (label, hash) in lines {
                let needs_legacy_match = identity.is_none();
                let needs_exact_match = !bound && label == username;
                if (needs_legacy_match || needs_exact_match) && verify_argon2id(&password, &hash) {
                    if identity.is_none() {
                        identity = Some(label.clone());
                    }
                    if label == username {
                        bound = true;
                    }
                }
                if identity.is_some() && bound {
                    break;
                }
            }
            identity.map(|identity| PasswordAuthMatch {
                identity,
                jump_principal: bound.then_some(username),
            })
        })
        .await
        .flatten()
    }
}

/// Hash a password with argon2id (default params, fresh random salt) for use
/// in a [`PasswordStore`] password-file line (`<label>:<hash>`).
pub fn hash_password(password: &str) -> Result<String> {
    if password.is_empty() {
        bail!("empty password");
    }
    let salt = SaltString::generate(&mut argon2::password_hash::rand_core::OsRng);
    let hash = Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|error| anyhow::anyhow!("argon2 hash failed: {error}"))?
        .to_string();
    Ok(hash)
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY_WITH_COMMENT: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIIgb/HlWS8uHpiobSm01har7Rq9zHceSet95iZUVd/+b alice@example.com";
    const KEY_NO_COMMENT: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIJ3Quk+mTUc8DGetajqdEMGJCJPBPr0MqCrpc3Fl0wv+";

    #[test]
    fn keystore_matches_known_key() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("authorized_keys"),
            format!("{KEY_WITH_COMMENT}\n"),
        )
        .unwrap();

        let store = KeyStore::new(dir.path().to_path_buf());
        let offered = PublicKey::from_openssh(KEY_WITH_COMMENT).unwrap();
        let grant = store.check(&offered).expect("key should match");
        assert_eq!(grant.identity, "alice@example.com");
    }

    #[test]
    fn keystore_identity_falls_back_to_fingerprint() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("authorized_keys"),
            format!("{KEY_NO_COMMENT}\n"),
        )
        .unwrap();

        let store = KeyStore::new(dir.path().to_path_buf());
        let offered = PublicKey::from_openssh(KEY_NO_COMMENT).unwrap();
        let grant = store.check(&offered).expect("key should match");
        assert!(
            grant.identity.starts_with("SHA256:"),
            "identity was {}",
            grant.identity
        );
    }

    #[test]
    fn keystore_options_parsed() {
        let dir = tempfile::tempdir().unwrap();
        let line = format!(
            r#"permit="vhost/my-*,port/9000-9010",max-conns=3,notes="ci" {KEY_WITH_COMMENT}"#
        );
        std::fs::write(dir.path().join("authorized_keys"), format!("{line}\n")).unwrap();

        let store = KeyStore::new(dir.path().to_path_buf());
        let offered = PublicKey::from_openssh(KEY_WITH_COMMENT).unwrap();
        let grant = store.check(&offered).expect("key should match");
        assert_eq!(
            grant.permit,
            Some(vec!["vhost/my-*".to_string(), "port/9000-9010".to_string()])
        );
        assert_eq!(grant.max_conns, Some(3));
        assert_eq!(grant.notes, Some("ci".to_string()));
    }

    #[test]
    fn keystore_hot_reload_add_and_remove() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("authorized_keys");
        let store = KeyStore::new(dir.path().to_path_buf());
        let offered = PublicKey::from_openssh(KEY_WITH_COMMENT).unwrap();

        assert!(store.check(&offered).is_none(), "no file yet, must miss");

        std::fs::write(&path, format!("{KEY_WITH_COMMENT}\n")).unwrap();
        assert!(store.check(&offered).is_some(), "file present, must hit");

        std::fs::remove_file(&path).unwrap();
        assert!(store.check(&offered).is_none(), "file removed, must miss");
    }

    #[test]
    fn keystore_mtime_cache() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("authorized_keys"),
            format!("{KEY_WITH_COMMENT}\n"),
        )
        .unwrap();

        let store = KeyStore::new(dir.path().to_path_buf());
        let offered = PublicKey::from_openssh(KEY_WITH_COMMENT).unwrap();

        store.check(&offered);
        assert_eq!(store.parse_calls.load(Ordering::Relaxed), 1);

        store.check(&offered);
        assert_eq!(
            store.parse_calls.load(Ordering::Relaxed),
            1,
            "unchanged mtime must not trigger a re-parse"
        );
    }

    #[test]
    fn keystore_malformed_line_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let content = format!("this is not a valid key line\n{KEY_WITH_COMMENT}\n");
        std::fs::write(dir.path().join("authorized_keys"), content).unwrap();

        let store = KeyStore::new(dir.path().to_path_buf());
        let offered = PublicKey::from_openssh(KEY_WITH_COMMENT).unwrap();
        assert!(
            store.check(&offered).is_some(),
            "valid key must still match"
        );
    }

    #[test]
    fn keystore_unknown_option_kept() {
        let dir = tempfile::tempdir().unwrap();
        let line = format!("no-touch-required {KEY_WITH_COMMENT}");
        std::fs::write(dir.path().join("authorized_keys"), format!("{line}\n")).unwrap();

        let store = KeyStore::new(dir.path().to_path_buf());
        let offered = PublicKey::from_openssh(KEY_WITH_COMMENT).unwrap();
        let grant = store
            .check(&offered)
            .expect("unknown option must not reject line");
        assert_eq!(grant.identity, "alice@example.com");
    }

    #[test]
    fn ssh_jump_key_binding_requires_exact_username_filename() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("fabio"), format!("{KEY_WITH_COMMENT}\n")).unwrap();

        let store = KeyStore::new(dir.path().to_path_buf());
        let offered = PublicKey::from_openssh(KEY_WITH_COMMENT).unwrap();
        let matched = store
            .check_for_user(&offered, "fabio")
            .expect("legacy key authentication must succeed");
        assert_eq!(matched.grant.identity, "alice@example.com");
        assert_eq!(matched.jump_principal.as_deref(), Some("fabio"));

        let mismatch = store
            .check_for_user(&offered, "wrong")
            .expect("legacy key authentication must ignore username");
        assert_eq!(mismatch.grant.identity, "alice@example.com");
        assert_eq!(mismatch.jump_principal, None);
    }

    #[test]
    fn ssh_jump_key_binding_supports_pub_suffix_and_multiple_keys() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("fabio.pub"),
            format!("{KEY_WITH_COMMENT}\n{KEY_NO_COMMENT}\n"),
        )
        .unwrap();
        let store = KeyStore::new(dir.path().to_path_buf());

        for raw in [KEY_WITH_COMMENT, KEY_NO_COMMENT] {
            let offered = PublicKey::from_openssh(raw).unwrap();
            let matched = store.check_for_user(&offered, "fabio").unwrap();
            assert_eq!(matched.jump_principal.as_deref(), Some("fabio"));
        }
    }

    #[test]
    fn ssh_jump_generic_key_file_remains_legacy_only_and_hot_reloads() {
        let dir = tempfile::tempdir().unwrap();
        let generic = dir.path().join("team.pub");
        std::fs::write(&generic, format!("{KEY_WITH_COMMENT}\n")).unwrap();
        let store = KeyStore::new(dir.path().to_path_buf());
        let offered = PublicKey::from_openssh(KEY_WITH_COMMENT).unwrap();

        let legacy = store.check_for_user(&offered, "fabio").unwrap();
        assert_eq!(legacy.jump_principal, None);

        std::fs::remove_file(generic).unwrap();
        std::fs::write(dir.path().join("fabio"), format!("{KEY_WITH_COMMENT}\n")).unwrap();
        let rebound = store.check_for_user(&offered, "fabio").unwrap();
        assert_eq!(rebound.jump_principal.as_deref(), Some("fabio"));
    }

    #[tokio::test]
    async fn hash_password_roundtrip() {
        let hash = hash_password("correct horse battery staple").unwrap();
        assert!(hash.starts_with("$argon2id$"));

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("passwords");
        std::fs::write(&path, format!("alice:{hash}\n")).unwrap();

        let store = PasswordStore::new(path);
        assert_eq!(
            store.check("correct horse battery staple").await,
            Some("alice".to_string())
        );
    }

    #[tokio::test]
    async fn password_any_of_multiple_lines_matches() {
        let hash1 = hash_password("password-one").unwrap();
        let hash2 = hash_password("password-two").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("passwords");
        std::fs::write(&path, format!("alice:{hash1}\nbob:{hash2}\n")).unwrap();

        let store = PasswordStore::new(path);
        assert_eq!(store.check("password-one").await, Some("alice".to_string()));
        assert_eq!(store.check("password-two").await, Some("bob".to_string()));
    }

    #[tokio::test]
    async fn password_wrong_rejected() {
        let hash = hash_password("correct-password").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("passwords");
        std::fs::write(&path, format!("alice:{hash}\n")).unwrap();

        let store = PasswordStore::new(path);
        assert_eq!(store.check("wrong-password").await, None);
    }

    #[tokio::test]
    async fn password_hot_reload() {
        let hash = hash_password("late-password").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("passwords");
        std::fs::write(&path, "").unwrap();

        let store = PasswordStore::new(path.clone());
        assert_eq!(store.check("late-password").await, None);

        std::fs::write(&path, format!("carol:{hash}\n")).unwrap();
        assert_eq!(
            store.check("late-password").await,
            Some("carol".to_string())
        );
    }

    #[tokio::test]
    async fn password_non_argon2_line_skipped() {
        let hash = hash_password("real-password").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("passwords");
        let content =
            format!("legacy:plain:x\nbcrypt:$2b$12$abcdefghijklmnopqrstuv\nreal:{hash}\n");
        std::fs::write(&path, content).unwrap();

        let store = PasswordStore::new(path);
        assert_eq!(store.check("real-password").await, Some("real".to_string()));
        assert_eq!(store.check("x").await, None);
    }

    #[tokio::test]
    async fn password_verify_concurrency_capped() {
        let hash = hash_password("shared-password").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("passwords");
        std::fs::write(&path, format!("user:{hash}\n")).unwrap();

        let store = Arc::new(PasswordStore::new(path));
        let mut tasks = Vec::new();
        for _ in 0..8 {
            let store = store.clone();
            tasks.push(tokio::spawn(
                async move { store.check("shared-password").await },
            ));
        }
        for task in tasks {
            assert_eq!(task.await.unwrap(), Some("user".to_string()));
        }

        let peak = store.peak_verifies.load(Ordering::SeqCst);
        assert!(peak >= 1, "expected at least one verify to run");
        assert!(
            peak <= PASSWORD_VERIFY_CONCURRENCY,
            "peak concurrency {peak} exceeded cap {PASSWORD_VERIFY_CONCURRENCY}"
        );
    }

    #[tokio::test]
    async fn ssh_jump_password_binding_requires_exact_username_label() {
        let hash = hash_password("correct-password").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("passwords");
        std::fs::write(&path, format!("fabio:{hash}\n")).unwrap();
        let store = PasswordStore::new(path);

        let matched = store
            .check_for_user("correct-password", "fabio")
            .await
            .expect("legacy password authentication must succeed");
        assert_eq!(matched.identity, "fabio");
        assert_eq!(matched.jump_principal.as_deref(), Some("fabio"));

        let mismatch = store
            .check_for_user("correct-password", "wrong")
            .await
            .expect("legacy password authentication must ignore username");
        assert_eq!(mismatch.identity, "fabio");
        assert_eq!(mismatch.jump_principal, None);
    }

    #[tokio::test]
    async fn ssh_jump_password_binding_hot_reloads_without_new_store() {
        let hash = hash_password("correct-password").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("passwords");
        std::fs::write(&path, format!("legacy:{hash}\n")).unwrap();
        let store = PasswordStore::new(path.clone());

        assert_eq!(
            store
                .check_for_user("correct-password", "fabio")
                .await
                .unwrap()
                .jump_principal,
            None,
        );

        std::fs::write(&path, format!("fabio:{hash}\n# reloaded\n")).unwrap();
        assert_eq!(
            store
                .check_for_user("correct-password", "fabio")
                .await
                .unwrap()
                .jump_principal
                .as_deref(),
            Some("fabio"),
        );
    }
}
