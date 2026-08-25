use anyhow::{bail, Context, Result};
use std::{
    fs::File,
    io::{self, BufWriter, Read, Write},
    path::Path,
};
use zip::{write::FileOptions, ZipArchive, ZipWriter};

const MISSION_ENTRY: &str = "mission";

pub fn is_miz(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("miz"))
        .unwrap_or(false)
}

pub fn read_mission_lua(path: &Path) -> Result<String> {
    if is_miz(path) {
        let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
        let mut archive = ZipArchive::new(file)
            .with_context(|| format!("unzipping {}", path.display()))?;
        let mut mission = archive
            .by_name(MISSION_ENTRY)
            .with_context(|| format!("{} has no '{MISSION_ENTRY}' entry", path.display()))?;
        let mut buf = String::new();
        mission.read_to_string(&mut buf).context("reading mission from miz")?;
        Ok(buf)
    } else {
        std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))
    }
}

pub fn write_mission_output(
    dest_path: &Path,
    output_path: &Path,
    mission_lua: &str,
) -> Result<()> {
    let dest_miz = is_miz(dest_path);
    let out_miz = is_miz(output_path);
    if dest_miz != out_miz {
        bail!(
            "--dest and --output must both be .miz or both be mission Lua files (dest={}, output={})",
            dest_path.display(),
            output_path.display()
        );
    }
    if let Some(parent) = output_path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
    }
    if out_miz {
        write_miz_replacing_mission(dest_path, output_path, mission_lua)
    } else {
        std::fs::write(output_path, mission_lua)
            .with_context(|| format!("writing {}", output_path.display()))
    }
}

fn write_miz_replacing_mission(dest_miz: &Path, output: &Path, mission_lua: &str) -> Result<()> {
    let file = File::open(dest_miz).with_context(|| format!("opening {}", dest_miz.display()))?;
    let mut archive =
        ZipArchive::new(file).with_context(|| format!("unzipping {}", dest_miz.display()))?;
    let out = File::create(output).with_context(|| format!("creating {}", output.display()))?;
    let mut zip = ZipWriter::new(BufWriter::new(out));
    let mut replaced = false;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).with_context(|| format!("zip entry {i}"))?;
        let name = entry.name().to_string();
        if name == MISSION_ENTRY {
            zip.start_file(MISSION_ENTRY, FileOptions::default())
                .context("starting mission zip entry")?;
            zip.write_all(mission_lua.as_bytes())
                .context("writing mission zip entry")?;
            replaced = true;
            continue;
        }
        zip.start_file(&name, FileOptions::default())
            .with_context(|| format!("starting zip entry {name}"))?;
        io::copy(&mut entry, &mut zip).with_context(|| format!("copying zip entry {name}"))?;
    }
    if !replaced {
        bail!("{} has no '{MISSION_ENTRY}' entry; warehouses and other files were not written", dest_miz.display());
    }
    zip.finish().context("finishing output miz")?;
    Ok(())
}
