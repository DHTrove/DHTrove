//! Persistent accounts and hashed, revocable browser sessions.
//! The first committed account is always administrator (SQLite IMMEDIATE
//! transaction); later self-registrations are regular users.

use argon2::{
    password_hash::{rand_core::{OsRng, RngCore}, PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Argon2,
};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fs,
    path::Path,
    sync::Mutex,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const SESSION_AGE_SECONDS: i64 = 7 * 24 * 60 * 60;
const RATE_WINDOW: Duration = Duration::from_secs(5 * 60);
const MAX_FAILED_ATTEMPTS: u32 = 10;
const MAX_ACCOUNTS: i64 = 10_000;

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct User {
    pub id: i64,
    pub username: String,
    pub is_admin: bool,
}

#[derive(Debug)]
pub enum AuthError {
    Invalid(&'static str),
    Conflict(&'static str),
    Forbidden(&'static str),
    Unauthorized,
    RateLimited,
    Storage(String),
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(message) | Self::Conflict(message) | Self::Forbidden(message) => write!(f, "{message}"),
            Self::Unauthorized => write!(f, "Invalid username or password"),
            Self::RateLimited => write!(f, "Too many sign-in attempts; try again later"),
            Self::Storage(message) => write!(f, "Account storage error: {message}"),
        }
    }
}
impl std::error::Error for AuthError {}

fn db_error(error: rusqlite::Error) -> AuthError {
    AuthError::Storage(error.to_string())
}

fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() as i64
}

fn normalize_username(username: &str) -> Result<String, AuthError> {
    let username = username.trim();
    if !(3..=32).contains(&username.len())
        || !username.as_bytes().iter().all(|byte| byte.is_ascii_alphanumeric() || b"_-.".contains(byte))
        || !username.bytes().any(|byte| byte.is_ascii_alphanumeric())
    {
        return Err(AuthError::Invalid("Username must be 3–32 letters, numbers, dots, dashes or underscores"));
    }
    Ok(username.to_ascii_lowercase())
}

fn validate_password(password: &str) -> Result<(), AuthError> {
    if !(12..=128).contains(&password.chars().count())
        || password.len() > 256
        || password.chars().any(char::is_control)
    {
        return Err(AuthError::Invalid("Password must be 12–128 characters without control characters"));
    }
    Ok(())
}

fn hash_password(password: &str) -> Result<String, AuthError> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|_| AuthError::Storage("could not hash password".into()))
}

fn verify_password(password: &str, encoded: &str) -> bool {
    PasswordHash::new(encoded)
        .ok()
        .is_some_and(|hash| Argon2::default().verify_password(password.as_bytes(), &hash).is_ok())
}

fn session_hash(token: &str) -> Vec<u8> {
    Sha256::digest(token.as_bytes()).to_vec()
}

fn generate_token() -> String {
    let mut random = [0u8; 32];
    OsRng.fill_bytes(&mut random);
    let mut output = String::with_capacity(64);
    use std::fmt::Write;
    for byte in random {
        write!(&mut output, "{byte:02x}").expect("String formatting does not fail");
    }
    output
}

struct FailedAttempts {
    start: Instant,
    count: u32,
}

pub struct Accounts {
    db: Mutex<Connection>,
    failed_attempts: Mutex<HashMap<String, FailedAttempts>>,
    dummy_hash: String,
}

impl Accounts {
    pub fn open(directory: &Path) -> Result<Self, AuthError> {
        let file = directory.join("accounts.sqlite3");
        let conn = Connection::open(&file).map_err(db_error)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&file, fs::Permissions::from_mode(0o600))
                .map_err(|error| AuthError::Storage(error.to_string()))?;
        }
        conn.busy_timeout(Duration::from_secs(5)).map_err(db_error)?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA foreign_keys=ON;
             PRAGMA secure_delete=ON;
             CREATE TABLE IF NOT EXISTS users (
               id INTEGER PRIMARY KEY AUTOINCREMENT,
               username TEXT NOT NULL UNIQUE COLLATE NOCASE,
               password_hash TEXT NOT NULL,
               is_admin INTEGER NOT NULL CHECK (is_admin IN (0, 1)),
               created_at INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS sessions (
               token_hash BLOB PRIMARY KEY,
               user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
               created_at INTEGER NOT NULL,
               expires_at INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS sessions_by_user ON sessions(user_id, created_at);
             CREATE TABLE IF NOT EXISTS options (
               key TEXT PRIMARY KEY,
               value TEXT NOT NULL
             );",
        ).map_err(db_error)?;
        Ok(Self {
            db: Mutex::new(conn),
            failed_attempts: Mutex::new(HashMap::new()),
            dummy_hash: hash_password("dhtrove_missing_account_dummy_password")?,
        })
    }

    pub fn setup_required(&self) -> Result<bool, AuthError> {
        let conn = self.db.lock().expect("account database mutex poisoned");
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM users", [], |row| row.get(0)).map_err(db_error)?;
        Ok(count == 0)
    }

    fn registration_open_conn(conn: &Connection) -> Result<bool, AuthError> {
        let value: Option<String> = conn.query_row(
            "SELECT value FROM options WHERE key='registration_enabled'",
            [],
            |row| row.get(0),
        ).optional().map_err(db_error)?;
        Ok(value.as_deref().unwrap_or("1") == "1")
    }

    pub fn registration_open(&self) -> Result<bool, AuthError> {
        let conn = self.db.lock().expect("account database mutex poisoned");
        Self::registration_open_conn(&conn)
    }

    pub fn set_registration(&self, enabled: bool) -> Result<(), AuthError> {
        let conn = self.db.lock().expect("account database mutex poisoned");
        conn.execute(
            "INSERT INTO options(key,value) VALUES('registration_enabled', ?1)
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![if enabled { "1" } else { "0" }],
        ).map_err(db_error)?;
        Ok(())
    }

    pub fn register(&self, username: &str, password: &str) -> Result<User, AuthError> {
        let username = normalize_username(username)?;
        validate_password(password)?;
        // Hash before opening the write transaction to avoid holding the
        // SQLite writer lock while Argon2id performs its expensive work.
        let encrypted = hash_password(password)?;
        let mut conn = self.db.lock().expect("account database mutex poisoned");
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(db_error)?;
        let count: i64 = tx.query_row("SELECT COUNT(*) FROM users", [], |row| row.get(0)).map_err(db_error)?;
        if count != 0 && !Self::registration_open_conn(&tx)? {
            return Err(AuthError::Forbidden("Registration is disabled by the administrator"));
        }
        if count >= MAX_ACCOUNTS {
            return Err(AuthError::Forbidden("Account limit reached"));
        }
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM users WHERE username=?1)", params![username],
            |row| row.get(0),
        ).map_err(db_error)?;
        if exists {
            return Err(AuthError::Conflict("Username is already taken"));
        }
        let is_admin = count == 0;
        tx.execute(
            "INSERT INTO users(username,password_hash,is_admin,created_at) VALUES(?1,?2,?3,?4)",
            params![username, encrypted, is_admin, now()],
        ).map_err(db_error)?;
        let id = tx.last_insert_rowid();
        tx.commit().map_err(db_error)?;
        Ok(User { id, username, is_admin })
    }

    fn record_failed(&self, username: &str) -> Result<(), AuthError> {
        let mut failures = self.failed_attempts.lock().expect("failed login mutex poisoned");
        if failures.len() > 2_048 {
            failures.retain(|_, entry| entry.start.elapsed() < RATE_WINDOW);
            if failures.len() > 2_048 {
                failures.clear();
            }
        }
        let entry = failures.entry(username.to_owned()).or_insert(FailedAttempts {
            start: Instant::now(),
            count: 0,
        });
        if entry.start.elapsed() >= RATE_WINDOW {
            entry.start = Instant::now();
            entry.count = 0;
        }
        entry.count = entry.count.saturating_add(1);
        Err(if entry.count >= MAX_FAILED_ATTEMPTS { AuthError::RateLimited } else { AuthError::Unauthorized })
    }

    pub fn login(&self, username: &str, password: &str) -> Result<User, AuthError> {
        let username = normalize_username(username).map_err(|_| AuthError::Unauthorized)?;
        if password.len() > 256 { return Err(AuthError::Unauthorized); }
        {
            let mut failed = self.failed_attempts.lock().expect("failed login mutex poisoned");
            if let Some(entry) = failed.get(&username) {
                if entry.start.elapsed() < RATE_WINDOW && entry.count >= MAX_FAILED_ATTEMPTS {
                    return Err(AuthError::RateLimited);
                }
                if entry.start.elapsed() >= RATE_WINDOW { failed.remove(&username); }
            }
        }
        let account: Option<(i64, String, bool, String)> = {
            let conn = self.db.lock().expect("account database mutex poisoned");
            conn.query_row(
                "SELECT id, username, is_admin, password_hash FROM users WHERE username=?1",
                params![username], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            ).optional().map_err(db_error)?
        };
        let hash = account.as_ref().map_or(self.dummy_hash.as_str(), |record| record.3.as_str());
        if !verify_password(password, hash) {
            return self.record_failed(&username);
        }
        let (id, username, is_admin, _) = account.ok_or(AuthError::Unauthorized)?;
        self.failed_attempts.lock().expect("failed login mutex poisoned").remove(&username);
        Ok(User { id, username, is_admin })
    }

    pub fn create_session(&self, user_id: i64) -> Result<String, AuthError> {
        let token = generate_token();
        let conn = self.db.lock().expect("account database mutex poisoned");
        let created = now();
        conn.execute("DELETE FROM sessions WHERE expires_at<=?1", params![created]).map_err(db_error)?;
        conn.execute(
            "INSERT INTO sessions(token_hash,user_id,created_at,expires_at) VALUES(?1,?2,?3,?4)",
            params![session_hash(&token), user_id, created, created + SESSION_AGE_SECONDS],
        ).map_err(db_error)?;
        conn.execute(
            "DELETE FROM sessions WHERE user_id=?1 AND token_hash NOT IN
             (SELECT token_hash FROM sessions WHERE user_id=?1 ORDER BY created_at DESC LIMIT 5)",
            params![user_id],
        ).map_err(db_error)?;
        Ok(token)
    }

    pub fn current_user(&self, token: &str) -> Result<Option<User>, AuthError> {
        if token.len() != 64 || !token.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Ok(None);
        }
        let conn = self.db.lock().expect("account database mutex poisoned");
        conn.query_row(
            "SELECT u.id, u.username, u.is_admin FROM sessions s
             JOIN users u ON u.id=s.user_id
             WHERE s.token_hash=?1 AND s.expires_at>?2",
            params![session_hash(token), now()],
            |row| Ok(User { id: row.get(0)?, username: row.get(1)?, is_admin: row.get(2)? }),
        ).optional().map_err(db_error)
    }

    pub fn revoke_session(&self, token: &str) -> Result<(), AuthError> {
        let conn = self.db.lock().expect("account database mutex poisoned");
        conn.execute("DELETE FROM sessions WHERE token_hash=?1", params![session_hash(token)]).map_err(db_error)?;
        Ok(())
    }

    pub fn users(&self) -> Result<Vec<User>, AuthError> {
        let conn = self.db.lock().expect("account database mutex poisoned");
        let mut query = conn.prepare("SELECT id,username,is_admin FROM users ORDER BY id").map_err(db_error)?;
        let rows = query.query_map([], |row| Ok(User {
            id: row.get(0)?, username: row.get(1)?, is_admin: row.get(2)?,
        })).map_err(db_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(db_error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn first_admin_and_second_regular_user_survive_restart() {
        let dir = tempdir().unwrap();
        let (token, first_id) = {
            let accounts = Accounts::open(dir.path()).unwrap();
            assert!(accounts.setup_required().unwrap());
            let first = accounts.register("Admin", "strong-start-password").unwrap();
            assert!(first.is_admin);
            assert_eq!(first.username, "admin");
            assert!(!accounts.setup_required().unwrap());
            let second = accounts.register("viewer", "viewer-valid-password").unwrap();
            assert!(!second.is_admin);
            assert!(matches!(accounts.register("ADMIN", "another-valid-password"), Err(AuthError::Conflict(_))));
            assert!(accounts.login("VIEWER", "bad-password-here").is_err());
            assert_eq!(accounts.login("viewer", "viewer-valid-password").unwrap(), second);
            (accounts.create_session(first.id).unwrap(), first.id)
        };
        let accounts = Accounts::open(dir.path()).unwrap();
        assert!(accounts.current_user(&token).unwrap().unwrap().is_admin);
        assert_eq!(accounts.current_user(&token).unwrap().unwrap().id, first_id);
        accounts.revoke_session(&token).unwrap();
        assert!(accounts.current_user(&token).unwrap().is_none());
    }

    #[test]
    fn registration_can_be_closed_but_existing_users_can_log_in() {
        let dir = tempdir().unwrap();
        let accounts = Accounts::open(dir.path()).unwrap();
        let admin = accounts.register("owner", "owner-valid-password").unwrap();
        accounts.set_registration(false).unwrap();
        assert!(!accounts.registration_open().unwrap());
        assert!(matches!(accounts.register("new-user", "valid-new-password"), Err(AuthError::Forbidden(_))));
        assert_eq!(accounts.login("owner", "owner-valid-password").unwrap(), admin);
        drop(accounts);
        let reopened = Accounts::open(dir.path()).unwrap();
        assert!(!reopened.registration_open().unwrap());
    }

    #[test]
    fn invalid_names_and_short_passwords_are_rejected() {
        let dir = tempdir().unwrap();
        let accounts = Accounts::open(dir.path()).unwrap();
        assert!(accounts.register("../admin", "valid-password-123").is_err());
        assert!(accounts.register("admin", "short").is_err());
        assert!(accounts.setup_required().unwrap());
    }
}
