//! Export / import de modpacks (.fmpack = zip avec manifest.json, réglages et zips optionnels).

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{Read, Write};
use std::path::Path;

use crate::modpack::{Modpack, PackEntry, new_id};
use crate::store::{AddMode, Index, Store, find};

#[derive(Serialize, Deserialize)]
struct Manifest {
    format: u32,
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    base_mod: Option<String>,
    #[serde(default)]
    factorio_version: Option<String>,
    mods: Vec<PackEntry>,
    #[serde(default)]
    disabled_builtins: Vec<String>,
    #[serde(default)]
    includes_settings: bool,
    #[serde(default)]
    includes_mods: bool,
}

pub fn export(pack: &Modpack, dest: &Path, index: &Index, settings: Option<&Path>, include_mods: bool) -> Result<()> {
    let file = fs::File::create(dest).with_context(|| format!("création de {}", dest.display()))?;
    let mut zip = zip::ZipWriter::new(file);
    let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    let stored = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    let settings = settings.filter(|s| s.exists());
    let manifest = Manifest {
        format: 1,
        name: pack.name.clone(),
        description: pack.description.clone(),
        base_mod: pack.base_mod.clone(),
        factorio_version: pack.factorio_version.clone(),
        mods: pack.mods.clone(),
        disabled_builtins: pack.disabled_builtins.clone(),
        includes_settings: settings.is_some(),
        includes_mods: include_mods,
    };
    zip.start_file("manifest.json", opts)?;
    zip.write_all(serde_json::to_string_pretty(&manifest)?.as_bytes())?;
    if let Some(s) = settings {
        zip.start_file("mod-settings.dat", opts)?;
        zip.write_all(&fs::read(s)?)?;
    }
    if include_mods {
        for e in &pack.mods {
            let Ok(v) = e.version.parse() else { continue };
            if let Some(info) = find(index, &e.name, &v) {
                zip.start_file(format!("mods/{}", info.file_name()), stored)?;
                let mut f = fs::File::open(&info.path)?;
                std::io::copy(&mut f, &mut zip)?;
            }
        }
    }
    zip.finish()?;
    Ok(())
}

pub struct Imported {
    pub pack: Modpack,
    pub settings: Option<Vec<u8>>,
    /// Mods (nom, version) absents du dépôt après import, à télécharger.
    pub missing: Vec<(String, String)>,
}

pub fn import(src: &Path, store: &Store, tmp_dir: &Path) -> Result<Imported> {
    let file = fs::File::open(src)?;
    let mut zip = zip::ZipArchive::new(file).context("fichier .fmpack invalide")?;
    let manifest: Manifest = {
        let mut text = String::new();
        zip.by_name("manifest.json").context("manifest.json manquant")?.read_to_string(&mut text)?;
        serde_json::from_str(&text)?
    };
    if manifest.format != 1 {
        bail!("format de modpack non supporté ({})", manifest.format);
    }
    let settings = match zip.by_name("mod-settings.dat") {
        Ok(mut f) => {
            let mut buf = Vec::new();
            f.read_to_end(&mut buf)?;
            Some(buf)
        }
        Err(_) => None,
    };
    fs::create_dir_all(tmp_dir)?;
    for i in 0..zip.len() {
        let mut f = zip.by_index(i)?;
        let name = f.name().to_string();
        let Some(file_name) = name.strip_prefix("mods/") else { continue };
        if file_name.is_empty() || file_name.contains('/') || file_name.contains("..") {
            continue;
        }
        let tmp = tmp_dir.join(file_name);
        let mut out = fs::File::create(&tmp)?;
        std::io::copy(&mut f, &mut out)?;
        drop(out);
        store.add_file(&tmp, AddMode::Move)?;
    }
    let index = store.scan();
    let missing = manifest
        .mods
        .iter()
        .filter(|e| e.version.parse().map(|v| find(&index, &e.name, &v).is_none()).unwrap_or(true))
        .map(|e| (e.name.clone(), e.version.clone()))
        .collect();
    let pack = Modpack {
        id: new_id(),
        name: manifest.name,
        description: manifest.description,
        base_mod: manifest.base_mod,
        factorio_version: manifest.factorio_version,
        mods: manifest.mods,
        disabled_builtins: manifest.disabled_builtins,
        saves: vec![],
    };
    Ok(Imported { pack, settings, missing })
}
