//! Player web accounts (username + password) and claim / recovery codes.
use anyhow::{bail, Result};
use log::info;
use chrono::{Duration, Utc};
use argon2::Argon2;
use dcso3::net::Ucid;
use password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use rand::Rng;
use serde_derive::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use yats::Tree;

pub const CLAIM_TTL_DAYS: i64 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClaimKind {
    Link,
    Recovery,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum UnitSystem {
    #[default]
    Metric,
    Imperial,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WebPreferences {
    #[serde(default)]
    pub units: UnitSystem,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebAccount {
    pub username: String,
    pub password_hash: String,
    pub ucid: Option<String>,
    pub created_at: chrono::DateTime<Utc>,
    /// Set after a successful recovery `-linkme` until password is changed.
    #[serde(default)]
    pub recovery_unlocked: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClaimCode {
    pub username: String,
    pub kind: ClaimKind,
    pub expires: chrono::DateTime<Utc>,
}

/// Admin list row (no password hash, no raw claim code).
#[derive(Debug, Clone, Serialize)]
pub struct WebAccountAdminRow {
    pub username: String,
    pub ucid: Option<String>,
    pub created_at: chrono::DateTime<Utc>,
    pub linked: bool,
    pub recovery_unlocked: bool,
    pub pending_claim: bool,
    pub pending_claim_kind: Option<&'static str>,
    pub pending_claim_expires: Option<chrono::DateTime<Utc>>,
}

#[derive(Clone)]
pub struct WebAuthStore {
    accounts: Tree<String, WebAccount>,
    by_ucid: Tree<String, String>,
    codes: Tree<String, ClaimCode>,
    /// Separate tree: bincode cannot add fields to `WebAccount` without breaking old rows.
    preferences: Tree<String, WebPreferences>,
}

impl WebAuthStore {
    pub fn open(db: &sled::Db) -> Result<Self> {
        let store = Self {
            accounts: Tree::open(db, "web_accounts")?,
            by_ucid: Tree::open(db, "web_accounts_by_ucid")?,
            codes: Tree::open(db, "web_claim_codes")?,
            preferences: Tree::open(db, "web_account_preferences")?,
        };
        match store.purge_stale() {
            Ok(n) if n > 0 => info!("web_auth: purged {n} unlinked account(s) with expired link codes"),
            Err(e) => log::warn!("web_auth: purge_stale on open failed: {e:?}"),
            _ => {}
        }
        Ok(store)
    }

    /// Drop expired claim codes and unlinked accounts with no valid Link code.
    /// Re-issuing a Link code keeps the account (extends the claim window).
    pub fn purge_stale(&self) -> Result<usize> {
        self.purge_expired_codes()?;
        let now = Utc::now();
        let mut keep: HashSet<String> = HashSet::new();
        for r in self.codes.iter() {
            let (_, claim) = r?;
            if claim.expires >= now && matches!(claim.kind, ClaimKind::Link) {
                keep.insert(claim.username.clone());
            }
        }
        let mut dead = Vec::new();
        for r in self.accounts.iter() {
            let (_, acc) = r?;
            if acc.ucid.is_some() {
                continue;
            }
            if keep.contains(&acc.username) {
                continue;
            }
            dead.push(acc.username.clone());
        }
        for user in &dead {
            self.delete_unlinked_account(user)?;
        }
        if !dead.is_empty() {
            info!("web_auth: purged {} unlinked account(s)", dead.len());
        }
        Ok(dead.len())
    }

    fn delete_unlinked_account(&self, username: &str) -> Result<()> {
        let mut codes = Vec::new();
        for r in self.codes.iter() {
            let (k, v) = r?;
            if v.username == username {
                codes.push(k);
            }
        }
        for k in codes {
            let _ = self.codes.remove(&k);
        }
        let key = username.to_string();
        let _ = self.preferences.remove(&key);
        let _ = self.accounts.remove(&key);
        Ok(())
    }

    pub fn get_account(&self, username: &str) -> Result<Option<WebAccount>> {
        Ok(self.accounts.get(&normalize_username(username)?)?)
    }

    pub fn account_for_ucid(&self, ucid: &Ucid) -> Result<Option<WebAccount>> {
        let Some(user) = self.by_ucid.get(&ucid.to_string())? else {
            return Ok(None);
        };
        self.get_account(&user)
    }

    /// UCIDs that already have a linked web account.
    pub fn linked_ucids(&self) -> Result<std::collections::HashSet<String>> {
        let mut out = std::collections::HashSet::new();
        for r in self.by_ucid.iter() {
            let (ucid, _) = r?;
            out.insert(ucid);
        }
        Ok(out)
    }

    /// Admin: detach UCID from a web account. Returns the previous UCID if any.
    pub fn unlink_account(&self, username: &str) -> Result<Option<String>> {
        let user = normalize_username(username)?;
        let mut account = self
            .accounts
            .get(&user)?
            .ok_or_else(|| anyhow::anyhow!("account not found"))?;
        let prev = account.ucid.take();
        if let Some(ref u) = prev {
            let _ = self.by_ucid.remove(u);
        }
        account.recovery_unlocked = false;
        self.accounts.insert(&user, &account)?;
        Ok(prev)
    }

    pub fn register(&self, username: &str, password: &str) -> Result<(WebAccount, String)> {
        let _ = self.purge_stale()?;
        let user = normalize_username(username)?;
        validate_password(password)?;
        if self.accounts.get(&user)?.is_some() {
            bail!("username already taken");
        }
        let account = WebAccount {
            username: user.clone(),
            password_hash: hash_password(password)?,
            ucid: None,
            created_at: Utc::now(),
            recovery_unlocked: false,
        };
        self.accounts.insert(&user, &account)?;
        let code = self.issue_code(&user, ClaimKind::Link)?;
        Ok((account, code))
    }

    pub fn get_preferences(&self, username: &str) -> Result<WebPreferences> {
        let user = normalize_username(username)?;
        Ok(self.preferences.get(&user)?.unwrap_or_default())
    }

    pub fn set_preferences(&self, username: &str, preferences: WebPreferences) -> Result<WebPreferences> {
        let user = normalize_username(username)?;
        if self.accounts.get(&user)?.is_none() {
            bail!("account not found");
        }
        self.preferences.insert(&user, &preferences)?;
        Ok(preferences)
    }

    pub fn verify_login(&self, username: &str, password: &str) -> Result<WebAccount> {
        let _ = self.purge_stale()?;
        let user = normalize_username(username)?;
        let Some(account) = self.accounts.get(&user)? else {
            bail!("invalid username or password");
        };
        if !verify_password(password, &account.password_hash)? {
            bail!("invalid username or password");
        }
        if account.recovery_unlocked {
            bail!("password reset required — use Forgot password completion");
        }
        Ok(account)
    }

    pub fn forgot(&self, username: &str) -> Result<Option<String>> {
        let _ = self.purge_stale()?;
        let user = normalize_username(username)?;
        if self.accounts.get(&user)?.is_none() {
            return Ok(None);
        }
        Ok(Some(self.issue_code(&user, ClaimKind::Recovery)?))
    }

    pub fn set_password_after_recovery(&self, username: &str, new_password: &str) -> Result<WebAccount> {
        let user = normalize_username(username)?;
        validate_password(new_password)?;
        let mut account = self
            .accounts
            .get(&user)?
            .ok_or_else(|| anyhow::anyhow!("account not found"))?;
        if !account.recovery_unlocked {
            bail!("recovery not verified — enter the claim code in DCS first");
        }
        account.password_hash = hash_password(new_password)?;
        account.recovery_unlocked = false;
        self.accounts.insert(&user, &account)?;
        Ok(account)
    }

    pub fn issue_link_code(&self, username: &str) -> Result<String> {
        let user = normalize_username(username)?;
        if self.accounts.get(&user)?.is_none() {
            bail!("account not found");
        }
        // Renew first so this account is kept by purge_stale (extends claim window).
        let code = self.issue_code(&user, ClaimKind::Link)?;
        let _ = self.purge_stale()?;
        Ok(code)
    }

    pub fn list_for_admin(&self) -> Result<Vec<WebAccountAdminRow>> {
        let _ = self.purge_stale()?;
        let now = Utc::now();
        let mut pending: HashMap<String, (ClaimKind, chrono::DateTime<Utc>)> = HashMap::new();
        for r in self.codes.iter() {
            let (_, claim) = r?;
            if claim.expires < now {
                continue;
            }
            match pending.get(&claim.username) {
                Some((_, exp)) if *exp >= claim.expires => {}
                _ => {
                    pending.insert(claim.username.clone(), (claim.kind, claim.expires));
                }
            }
        }
        let mut rows = Vec::new();
        for r in self.accounts.iter() {
            let (_, acc) = r?;
            let (pending_claim_kind, pending_claim_expires) = match pending.get(&acc.username) {
                Some((ClaimKind::Link, exp)) => (Some("link"), Some(*exp)),
                Some((ClaimKind::Recovery, exp)) => (Some("recovery"), Some(*exp)),
                None => (None, None),
            };
            rows.push(WebAccountAdminRow {
                username: acc.username.clone(),
                ucid: acc.ucid.clone(),
                created_at: acc.created_at,
                linked: acc.ucid.is_some(),
                recovery_unlocked: acc.recovery_unlocked,
                pending_claim: pending_claim_kind.is_some(),
                pending_claim_kind,
                pending_claim_expires,
            });
        }
        rows.sort_by(|a, b| a.username.to_ascii_lowercase().cmp(&b.username.to_ascii_lowercase()));
        Ok(rows)
    }

    fn issue_code(&self, username: &str, kind: ClaimKind) -> Result<String> {
        self.purge_expired_codes()?;
        let mut rng = rand::thread_rng();
        let code = loop {
            let c = format!("{:04}", rng.gen_range(1000..10000));
            if self.codes.get(&c)?.is_none() {
                break c;
            }
        };
        self.codes.insert(
            &code,
            &ClaimCode {
                username: username.to_string(),
                kind,
                expires: Utc::now() + Duration::days(CLAIM_TTL_DAYS),
            },
        )?;
        Ok(code)
    }

    /// Apply in-game `-linkme` / `Stat::WebClaim`.
    pub fn apply_web_claim(&self, ucid: &Ucid, code: &str) -> Result<&'static str> {
        let _ = self.purge_stale()?;
        let code = code.trim();
        if code.len() != 4 || !code.chars().all(|c| c.is_ascii_digit()) {
            bail!("invalid claim code");
        }
        let Some(claim) = self.codes.get(&code.to_string())? else {
            bail!("unknown or expired claim code");
        };
        if claim.expires < Utc::now() {
            let _ = self.codes.remove(&code.to_string());
            bail!("claim code expired");
        }
        let mut account = self
            .accounts
            .get(&claim.username)?
            .ok_or_else(|| anyhow::anyhow!("account missing for claim"))?;

        match claim.kind {
            ClaimKind::Link => {
                if let Some(owner) = self.by_ucid.get(&ucid.to_string())? {
                    if owner != claim.username {
                        // 2a — warn and stop (no transfer)
                        bail!("UCID already linked to another account — use Login or Forgot password");
                    }
                }
                if let Some(ref old) = account.ucid {
                    if old != &ucid.to_string() {
                        let _ = self.by_ucid.remove(old);
                    }
                }
                account.ucid = Some(ucid.to_string());
                account.recovery_unlocked = false;
                self.accounts.insert(&claim.username, &account)?;
                self.by_ucid.insert(&ucid.to_string(), &claim.username)?;
                let _ = self.codes.remove(&code.to_string());
                Ok("linked")
            }
            ClaimKind::Recovery => {
                let Some(ref linked) = account.ucid else {
                    bail!("account has no linked UCID — register a new claim first");
                };
                if linked != &ucid.to_string() {
                    bail!("UCID does not match this account");
                }
                account.recovery_unlocked = true;
                self.accounts.insert(&claim.username, &account)?;
                let _ = self.codes.remove(&code.to_string());
                Ok("recovery_unlocked")
            }
        }
    }

    fn purge_expired_codes(&self) -> Result<()> {
        let now = Utc::now();
        let mut dead = Vec::new();
        for r in self.codes.iter() {
            let (k, v) = r?;
            if v.expires < now {
                dead.push(k);
            }
        }
        for k in dead {
            let _ = self.codes.remove(&k);
        }
        Ok(())
    }
}

fn normalize_username(username: &str) -> Result<String> {
    let u = username.trim().to_string();
    if u.len() < 3 || u.len() > 32 {
        bail!("username must be 3–32 characters");
    }
    if !u
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        bail!("username may only contain letters, digits, _ and -");
    }
    if is_reserved_username(&u) {
        bail!("username not allowed");
    }
    Ok(u)
}

/// Exact match (ASCII case-insensitive) of common DCS default MP nicknames
/// and a few generic account names. Non-ASCII defaults (e.g. Czech "Hráč")
/// are already rejected by the ASCII-only rule above.
fn is_reserved_username(username: &str) -> bool {
    const RESERVED: &[&str] = &[
        // DCS / common "Player" localizations (ASCII forms)
        "player",
        "spieler",
        "joueur",
        "giocatore",
        "jugador",
        "jogador",
        "gracz",
        "speler",
        "pelaaja",
        "spiller",
        "spelare",
        "jatekos",
        // Generic / confusing
        "pilot",
        "guest",
        "anonymous",
        "admin",
        "administrator",
        "root",
        "system",
        "server",
        "attrition",
        "fowl",
    ];
    let lower = username.to_ascii_lowercase();
    RESERVED.iter().any(|r| *r == lower.as_str())
}

fn validate_password(password: &str) -> Result<()> {
    if password.len() < 8 {
        bail!("password must be at least 8 characters");
    }
    if password.len() > 128 {
        bail!("password too long");
    }
    Ok(())
}

fn hash_password(password: &str) -> Result<String> {
    let salt = SaltString::generate(&mut rand::thread_rng());
    let hash = Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| anyhow::anyhow!("password hash failed: {e}"))?;
    Ok(hash.to_string())
}

fn verify_password(password: &str, hash: &str) -> Result<bool> {
    let parsed = PasswordHash::new(hash)
        .map_err(|e| anyhow::anyhow!("bad password hash: {e}"))?;
    Ok(Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok())
}
