//! Opaque public pilot IDs for the dashboard (never derived from UCID).
//!
//! Stored in the existing Sled DB — no wipe, no re-registration. Public API
//! paths and JSON use `id` (`p_<uuid>`); UCID stays for admin / claim / logs.
use anyhow::{bail, Result};
use dcso3::net::Ucid;
use uuid::Uuid;
use yats::Tree;

#[derive(Clone)]
pub struct PilotPublicIds {
    /// ucid string → public id
    by_ucid: Tree<String, String>,
    /// public id → ucid string
    by_id: Tree<String, String>,
}

impl PilotPublicIds {
    pub fn open(db: &sled::Db) -> Result<Self> {
        Ok(Self {
            by_ucid: Tree::open(db, "pilot_public_by_ucid")?,
            by_id: Tree::open(db, "pilot_public_by_id")?,
        })
    }

    fn new_id() -> String {
        format!("p_{}", Uuid::new_v4().as_simple())
    }

    pub fn id_for(&self, ucid: &Ucid) -> Result<String> {
        let key = ucid.to_string();
        if let Some(id) = self.by_ucid.get(&key)? {
            return Ok(id);
        }
        // Rare race: retry a few times on id collision.
        for _ in 0..8 {
            let id = Self::new_id();
            if self.by_id.get(&id)?.is_some() {
                continue;
            }
            self.by_id.insert(&id, &key)?;
            self.by_ucid.insert(&key, &id)?;
            return Ok(id);
        }
        bail!("could not allocate unique public pilot id");
    }

    pub fn ucid_for_id(&self, id: &str) -> Result<Option<Ucid>> {
        let Some(u) = self.by_id.get(&id.to_string())? else {
            return Ok(None);
        };
        Ok(Some(u.parse().map_err(|e| anyhow::anyhow!("bad ucid in public id map: {e:?}"))?))
    }

    /// Resolve a path/query key: prefer `p_…` public id; accept legacy raw UCID.
    pub fn resolve(&self, key: &str) -> Result<Ucid> {
        let key = key.trim();
        if key.is_empty() {
            bail!("empty pilot id");
        }
        if key.starts_with("p_") {
            return self
                .ucid_for_id(key)?
                .ok_or_else(|| anyhow::anyhow!("unknown pilot id"));
        }
        // Legacy bookmarks / admin-shaped keys (32 hex UCID).
        let ucid: Ucid = key
            .parse()
            .map_err(|_| anyhow::anyhow!("unknown pilot id"))?;
        // Ensure a public id exists even when resolved via legacy UCID.
        let _ = self.id_for(&ucid)?;
        Ok(ucid)
    }

    /// Assign public ids for every pilot row that does not have one yet.
    pub fn ensure_all(&self, ucids: impl IntoIterator<Item = Ucid>) -> Result<u32> {
        let mut n = 0u32;
        for ucid in ucids {
            let key = ucid.to_string();
            if self.by_ucid.get(&key)?.is_none() {
                let _ = self.id_for(&ucid)?;
                n += 1;
            }
        }
        Ok(n)
    }
}
