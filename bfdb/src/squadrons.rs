//! Global Attrition squadrons (letky) — membership keyed by UCID.
use anyhow::{bail, Result};
use chrono::{DateTime, Utc};
use serde_derive::{Deserialize, Serialize};
use uuid::Uuid;
use yats::Tree;

const MAX_ICON_BYTES: usize = 50 * 1024;
const MAX_ICON_PX: u32 = 200;
/// Apply libimagequant palette compression when upload exceeds this size.
const ICON_COMPRESS_THRESHOLD: usize = 20 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Squadron {
    pub id: Uuid,
    pub name: String,
    pub commander_ucid: String,
    pub created_at: DateTime<Utc>,
    pub dissolved_at: Option<DateTime<Utc>>,
    /// Raw PNG bytes (validated on upload).
    #[serde(default)]
    pub icon_png: Option<Vec<u8>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Membership {
    pub squadron_id: Uuid,
    pub ucid: String,
    pub joined_at: DateTime<Utc>,
    pub left_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum JoinRequestStatus {
    Pending,
    Approved,
    Rejected,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JoinRequest {
    pub id: Uuid,
    pub squadron_id: Uuid,
    pub ucid: String,
    pub requested_at: DateTime<Utc>,
    pub status: JoinRequestStatus,
}

#[derive(Clone)]
pub struct SquadronStore {
    squadrons: Tree<String, Squadron>,
    /// Lowercased name → squadron id (includes dissolved; uniqueness forever).
    by_name: Tree<String, String>,
    /// Active membership only: ucid → Membership
    active_member: Tree<String, Membership>,
    /// History rows keyed by uuid string
    history: Tree<String, Membership>,
    requests: Tree<String, JoinRequest>,
}

impl SquadronStore {
    pub fn open(db: &sled::Db) -> Result<Self> {
        Ok(Self {
            squadrons: Tree::open(db, "squadrons")?,
            by_name: Tree::open(db, "squadrons_by_name")?,
            active_member: Tree::open(db, "squadron_active_member")?,
            history: Tree::open(db, "squadron_membership_history")?,
            requests: Tree::open(db, "squadron_join_requests")?,
        })
    }

    pub fn get(&self, id: &Uuid) -> Result<Option<Squadron>> {
        Ok(self.squadrons.get(&id.to_string())?)
    }

    pub fn active_for_ucid(&self, ucid: &str) -> Result<Option<(Squadron, Membership)>> {
        let Some(m) = self.active_member.get(&ucid.to_string())? else {
            return Ok(None);
        };
        let Some(s) = self.get(&m.squadron_id)? else {
            return Ok(None);
        };
        if s.dissolved_at.is_some() {
            return Ok(None);
        }
        Ok(Some((s, m)))
    }

    pub fn list(&self, include_dissolved: bool) -> Result<Vec<Squadron>> {
        let mut out = Vec::new();
        for r in self.squadrons.iter() {
            let (_, s) = r?;
            if !include_dissolved && s.dissolved_at.is_some() {
                continue;
            }
            out.push(s);
        }
        out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        Ok(out)
    }

    pub fn create(&self, name: &str, commander_ucid: &str) -> Result<Squadron> {
        let name = normalize_name(name)?;
        if self.by_name.get(&name.to_lowercase())?.is_some() {
            bail!("squadron name already taken");
        }
        if self.active_member.get(&commander_ucid.to_string())?.is_some() {
            bail!("already in a squadron");
        }
        let id = Uuid::new_v4();
        let sq = Squadron {
            id,
            name: name.clone(),
            commander_ucid: commander_ucid.to_string(),
            created_at: Utc::now(),
            dissolved_at: None,
            icon_png: None,
        };
        self.squadrons.insert(&id.to_string(), &sq)?;
        self.by_name.insert(&name.to_lowercase(), &id.to_string())?;
        let mem = Membership {
            squadron_id: id,
            ucid: commander_ucid.to_string(),
            joined_at: Utc::now(),
            left_at: None,
        };
        self.active_member.insert(&commander_ucid.to_string(), &mem)?;
        self.history.insert(&Uuid::new_v4().to_string(), &mem)?;
        Ok(sq)
    }

    pub fn rename(&self, id: &Uuid, new_name: &str, actor_ucid: &str) -> Result<Squadron> {
        let mut sq = self.require_active(id)?;
        if sq.commander_ucid != actor_ucid {
            bail!("only the commander can rename");
        }
        let new_name = normalize_name(new_name)?;
        let key = new_name.to_lowercase();
        if let Some(other) = self.by_name.get(&key)? {
            if other != id.to_string() {
                bail!("squadron name already taken");
            }
        }
        let _ = self.by_name.remove(&sq.name.to_lowercase());
        sq.name = new_name;
        self.by_name.insert(&key, &id.to_string())?;
        self.squadrons.insert(&id.to_string(), &sq)?;
        Ok(sq)
    }

    pub fn set_icon(&self, id: &Uuid, png: &[u8], actor_ucid: &str) -> Result<Squadron> {
        let mut sq = self.require_active(id)?;
        if sq.commander_ucid != actor_ucid {
            bail!("only the commander can set the icon");
        }
        validate_png(png)?;
        let stored = prepare_icon_png(png)?;
        validate_png(&stored)?;
        sq.icon_png = Some(stored);
        self.squadrons.insert(&id.to_string(), &sq)?;
        Ok(sq)
    }

    pub fn clear_icon(&self, id: &Uuid, actor_ucid: &str) -> Result<Squadron> {
        let mut sq = self.require_active(id)?;
        if sq.commander_ucid != actor_ucid {
            bail!("only the commander can clear the icon");
        }
        sq.icon_png = None;
        self.squadrons.insert(&id.to_string(), &sq)?;
        Ok(sq)
    }

    pub fn request_join(&self, id: &Uuid, ucid: &str) -> Result<JoinRequest> {
        let sq = self.require_active(id)?;
        if self.active_member.get(&ucid.to_string())?.is_some() {
            bail!("already in a squadron");
        }
        for r in self.requests.iter() {
            let (_, req) = r?;
            if req.squadron_id == sq.id
                && req.ucid == ucid
                && req.status == JoinRequestStatus::Pending
            {
                bail!("join request already pending");
            }
        }
        let req = JoinRequest {
            id: Uuid::new_v4(),
            squadron_id: sq.id,
            ucid: ucid.to_string(),
            requested_at: Utc::now(),
            status: JoinRequestStatus::Pending,
        };
        self.requests.insert(&req.id.to_string(), &req)?;
        Ok(req)
    }

    pub fn decide_join(
        &self,
        request_id: &Uuid,
        actor_ucid: &str,
        approve: bool,
    ) -> Result<JoinRequest> {
        let mut req = self
            .requests
            .get(&request_id.to_string())?
            .ok_or_else(|| anyhow::anyhow!("join request not found"))?;
        if req.status != JoinRequestStatus::Pending {
            bail!("join request is not pending");
        }
        let sq = self.require_active(&req.squadron_id)?;
        if sq.commander_ucid != actor_ucid {
            bail!("only the commander can approve or deny");
        }
        if approve {
            if self.active_member.get(&req.ucid)?.is_some() {
                req.status = JoinRequestStatus::Rejected;
                self.requests.insert(&req.id.to_string(), &req)?;
                bail!("applicant already joined another squadron");
            }
            let mem = Membership {
                squadron_id: sq.id,
                ucid: req.ucid.clone(),
                joined_at: Utc::now(),
                left_at: None,
            };
            self.active_member.insert(&req.ucid, &mem)?;
            self.history.insert(&Uuid::new_v4().to_string(), &mem)?;
            req.status = JoinRequestStatus::Approved;
        } else {
            req.status = JoinRequestStatus::Rejected;
        }
        self.requests.insert(&req.id.to_string(), &req)?;
        Ok(req)
    }

    pub fn transfer_command(&self, id: &Uuid, actor_ucid: &str, new_co: &str) -> Result<Squadron> {
        let mut sq = self.require_active(id)?;
        if sq.commander_ucid != actor_ucid {
            bail!("only the commander can transfer command");
        }
        let Some(mem) = self.active_member.get(&new_co.to_string())? else {
            bail!("new commander must be an active member");
        };
        if mem.squadron_id != *id {
            bail!("new commander must be an active member");
        }
        if new_co == actor_ucid {
            bail!("already commander");
        }
        sq.commander_ucid = new_co.to_string();
        self.squadrons.insert(&id.to_string(), &sq)?;
        Ok(sq)
    }

    pub fn leave(&self, ucid: &str) -> Result<&'static str> {
        let Some(mut mem) = self.active_member.get(&ucid.to_string())? else {
            bail!("not in a squadron");
        };
        let Some(mut sq) = self.get(&mem.squadron_id)? else {
            let _ = self.active_member.remove(&ucid.to_string());
            bail!("squadron missing");
        };
        if sq.dissolved_at.is_some() {
            let _ = self.active_member.remove(&ucid.to_string());
            bail!("squadron already dissolved");
        }
        if sq.commander_ucid == ucid {
            // Dissolve — CO left without transfer
            self.dissolve_inner(&mut sq)?;
            return Ok("dissolved");
        }
        mem.left_at = Some(Utc::now());
        self.history.insert(&Uuid::new_v4().to_string(), &mem)?;
        let _ = self.active_member.remove(&ucid.to_string());
        Ok("left")
    }

    /// CO removes a non-commander member (force leave). Does not dissolve.
    pub fn kick(&self, id: &Uuid, actor_ucid: &str, target_ucid: &str) -> Result<()> {
        let sq = self.require_active(id)?;
        if sq.commander_ucid != actor_ucid {
            bail!("only the commander can kick members");
        }
        if target_ucid == actor_ucid {
            bail!("cannot kick yourself — leave or transfer command first");
        }
        if target_ucid == sq.commander_ucid {
            bail!("cannot kick the commander");
        }
        let Some(mut mem) = self.active_member.get(&target_ucid.to_string())? else {
            bail!("pilot is not in a squadron");
        };
        if mem.squadron_id != *id {
            bail!("pilot is not a member of this squadron");
        }
        mem.left_at = Some(Utc::now());
        self.history.insert(&Uuid::new_v4().to_string(), &mem)?;
        let _ = self.active_member.remove(&target_ucid.to_string());
        Ok(())
    }

    pub fn pending_for_squadron(&self, id: &Uuid) -> Result<Vec<JoinRequest>> {
        let mut out = Vec::new();
        for r in self.requests.iter() {
            let (_, req) = r?;
            if req.squadron_id == *id && req.status == JoinRequestStatus::Pending {
                out.push(req);
            }
        }
        out.sort_by_key(|r| r.requested_at);
        Ok(out)
    }

    /// Squadron ids with a pending join request from this UCID.
    pub fn pending_join_squadron_ids_for_ucid(&self, ucid: &str) -> Result<Vec<Uuid>> {
        let mut out = Vec::new();
        for r in self.requests.iter() {
            let (_, req) = r?;
            if req.ucid == ucid && req.status == JoinRequestStatus::Pending {
                out.push(req.squadron_id);
            }
        }
        out.sort();
        out.dedup();
        Ok(out)
    }

    pub fn members(&self, id: &Uuid) -> Result<Vec<Membership>> {
        let mut out = Vec::new();
        for r in self.active_member.iter() {
            let (_, m) = r?;
            if m.squadron_id == *id {
                out.push(m);
            }
        }
        out.sort_by_key(|m| m.joined_at);
        Ok(out)
    }

    pub fn public_summary(sq: &Squadron) -> serde_json::Value {
        serde_json::json!({
            "id": sq.id.to_string(),
            "name": sq.name,
            "created_at": sq.created_at.to_rfc3339(),
            "dissolved_at": sq.dissolved_at.map(|t| t.to_rfc3339()),
            "has_icon": sq.icon_png.is_some(),
        })
    }

    fn require_active(&self, id: &Uuid) -> Result<Squadron> {
        let sq = self
            .get(id)?
            .ok_or_else(|| anyhow::anyhow!("squadron not found"))?;
        if sq.dissolved_at.is_some() {
            bail!("squadron is dissolved");
        }
        Ok(sq)
    }

    fn dissolve_inner(&self, sq: &mut Squadron) -> Result<()> {
        sq.dissolved_at = Some(Utc::now());
        self.squadrons.insert(&sq.id.to_string(), sq)?;
        // Remove all active members + history leave stamps
        let mut to_clear = Vec::new();
        for r in self.active_member.iter() {
            let (ucid, mut m) = r?;
            if m.squadron_id == sq.id {
                m.left_at = Some(Utc::now());
                self.history.insert(&Uuid::new_v4().to_string(), &m)?;
                to_clear.push(ucid);
            }
        }
        for u in to_clear {
            let _ = self.active_member.remove(&u);
        }
        // Auto-reject pending
        let mut reqs = Vec::new();
        for r in self.requests.iter() {
            let (k, mut req) = r?;
            if req.squadron_id == sq.id && req.status == JoinRequestStatus::Pending {
                req.status = JoinRequestStatus::Rejected;
                reqs.push((k, req));
            }
        }
        for (k, req) in reqs {
            self.requests.insert(&k, &req)?;
        }
        Ok(())
    }
}

fn normalize_name(name: &str) -> Result<String> {
    let n = name.trim().to_string();
    if n.len() < 2 || n.len() > 48 {
        bail!("squadron name must be 2–48 characters");
    }
    if !n
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == ' ' || c == '-' || c == '_' || c == '.')
    {
        bail!("squadron name may only contain letters, digits, space, - _ .");
    }
    Ok(n)
}

fn validate_png(png: &[u8]) -> Result<()> {
    if png.len() > MAX_ICON_BYTES {
        bail!("icon must be at most {} KB", MAX_ICON_BYTES / 1024);
    }
    if png.len() < 24 {
        bail!("invalid PNG");
    }
    const SIG: &[u8] = &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    if &png[0..8] != SIG {
        bail!("icon must be a PNG file");
    }
    // IHDR is the first chunk
    if &png[12..16] != b"IHDR" {
        bail!("invalid PNG (missing IHDR)");
    }
    let w = u32::from_be_bytes([png[16], png[17], png[18], png[19]]);
    let h = u32::from_be_bytes([png[20], png[21], png[22], png[23]]);
    if w == 0 || h == 0 || w > MAX_ICON_PX || h > MAX_ICON_PX {
        bail!("icon must be at most {MAX_ICON_PX}×{MAX_ICON_PX} pixels");
    }
    // Reject APNG (acTL chunk anywhere in first pass of chunk headers)
    let mut i = 8usize;
    while i + 8 <= png.len() {
        let len = u32::from_be_bytes([png[i], png[i + 1], png[i + 2], png[i + 3]]) as usize;
        let typ = &png[i + 4..i + 8];
        if typ == b"acTL" {
            bail!("animated PNG is not allowed");
        }
        let next = i.saturating_add(12).saturating_add(len);
        if next <= i {
            break;
        }
        i = next;
    }
    Ok(())
}

/// Keep original under threshold; otherwise try libimagequant and keep the smaller PNG.
fn prepare_icon_png(png: &[u8]) -> Result<Vec<u8>> {
    if png.len() <= ICON_COMPRESS_THRESHOLD {
        return Ok(png.to_vec());
    }
    match compress_png_palette(png) {
        Ok(out) if !out.is_empty() && out.len() < png.len() && out.len() <= MAX_ICON_BYTES => {
            log::info!(
                "squadron icon compressed {} → {} bytes (libimagequant)",
                png.len(),
                out.len()
            );
            Ok(out)
        }
        Ok(out) => {
            log::debug!(
                "squadron icon compress skipped (out {} B vs in {} B)",
                out.len(),
                png.len()
            );
            Ok(png.to_vec())
        }
        Err(e) => {
            log::warn!("squadron icon compress failed, keeping original: {e:#}");
            Ok(png.to_vec())
        }
    }
}

fn compress_png_palette(png: &[u8]) -> Result<Vec<u8>> {
    let rgba = image::load_from_memory(png)
        .map_err(|e| anyhow::anyhow!("png decode: {e}"))?
        .to_rgba8();
    let w = rgba.width() as usize;
    let h = rgba.height() as usize;
    if w == 0 || h == 0 {
        bail!("empty image");
    }

    let pixels: Vec<rgb::RGBA8> = rgba
        .pixels()
        .map(|p| rgb::RGBA8 {
            r: p.0[0],
            g: p.0[1],
            b: p.0[2],
            a: p.0[3],
        })
        .collect();

    let mut attr = imagequant::Attributes::new();
    // Quality band similar to typical TinyPNG / pngquant defaults for badges.
    attr.set_quality(20, 80)
        .map_err(|e| anyhow::anyhow!("imagequant quality: {e}"))?;
    attr.set_speed(3)
        .map_err(|e| anyhow::anyhow!("imagequant speed: {e}"))?;

    let mut iq_img = attr
        .new_image(&pixels[..], w, h, 0.0)
        .map_err(|e| anyhow::anyhow!("imagequant image: {e}"))?;
    let mut quantized = attr
        .quantize(&mut iq_img)
        .map_err(|e| anyhow::anyhow!("imagequant quantize: {e}"))?;
    quantized
        .set_dithering_level(0.8)
        .map_err(|e| anyhow::anyhow!("imagequant dither: {e}"))?;
    let (palette, indices) = quantized
        .remapped(&mut iq_img)
        .map_err(|e| anyhow::anyhow!("imagequant remap: {e}"))?;

    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, w as u32, h as u32);
        encoder.set_color(png::ColorType::Indexed);
        encoder.set_depth(png::BitDepth::Eight);
        let rgb_pal: Vec<u8> = palette.iter().flat_map(|c| [c.r, c.g, c.b]).collect();
        encoder.set_palette(rgb_pal);
        let trns: Vec<u8> = palette.iter().map(|c| c.a).collect();
        if trns.iter().any(|&a| a < 255) {
            encoder.set_trns(trns);
        }
        encoder.set_compression(png::Compression::Best);
        let mut writer = encoder
            .write_header()
            .map_err(|e| anyhow::anyhow!("png header: {e}"))?;
        writer
            .write_image_data(&indices)
            .map_err(|e| anyhow::anyhow!("png data: {e}"))?;
    }
    Ok(out)
}
