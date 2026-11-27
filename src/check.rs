//! Vérification d'un ensemble de mods sans lancer le jeu.
//!
//! Une instance isolée de Factorio (son propre config.ini et son propre dossier d'écriture,
//! donc compatible avec une partie en cours) exécute l'étape data et charge les prototypes via
//! `--dump-prototype-locale`, puis quitte avant le chargement des sprites (la partie la plus
//! longue d'un vrai lancement). Les erreurs du log sont analysées pour retrouver les mods en cause,
//! y compris les incompatibilités que les mods ne déclarent pas.

use anyhow::{Context, Result, anyhow};
use sha1::{Digest, Sha1};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::modpack::Resolution;
use crate::store::{self, Index};
use crate::version::is_builtin;

/// Dossiers de l'instance de vérification : <root>/config.ini, <root>/write, <root>/mods.
#[derive(Clone, Debug)]
pub struct CheckDirs {
    pub root: PathBuf,
}

impl CheckDirs {
    pub fn new(root: PathBuf) -> CheckDirs {
        CheckDirs { root }
    }
    pub fn config(&self) -> PathBuf {
        self.root.join("config.ini")
    }
    pub fn write(&self) -> PathBuf {
        self.root.join("write")
    }
    pub fn mods(&self) -> PathBuf {
        self.root.join("mods")
    }
    pub fn log(&self) -> PathBuf {
        self.write().join("factorio-current.log")
    }
    fn ok_cache(&self) -> PathBuf {
        self.root.join("ok.json")
    }

    /// Écrit le config.ini qui isole l'instance (lecture : données du jeu ; écriture : dossier dédié).
    pub fn prepare(&self) -> Result<()> {
        fs::create_dir_all(self.write())?;
        fs::create_dir_all(self.mods())?;
        let write = self.write().to_string_lossy().replace('\\', "/");
        let ini = format!("[path]\nread-data=__PATH__executable__/../../data\nwrite-data={write}\n");
        if fs::read_to_string(self.config()).ok().as_deref() != Some(ini.as_str()) {
            fs::write(self.config(), ini)?;
        }
        Ok(())
    }

    pub fn is_known_ok(&self, fp: &str) -> bool {
        self.read_ok().iter().any(|f| f == fp)
    }

    pub fn remember_ok(&self, fp: &str) {
        let mut list = self.read_ok();
        list.retain(|f| f != fp);
        list.push(fp.to_string());
        if list.len() > 100 {
            list.drain(..list.len() - 100);
        }
        if let Ok(text) = serde_json::to_string(&list) {
            let _ = crate::config::write_atomic(&self.ok_cache(), text.as_bytes());
        }
    }

    fn read_ok(&self) -> Vec<String> {
        fs::read_to_string(self.ok_cache()).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
    }

    fn tolerated_path(&self) -> PathBuf {
        self.root.join("tolerated-missing.json")
    }

    /// Référence : fichiers introuvables mais sans conséquence (le jeu s'est chargé avec).
    /// `None` tant qu'aucune référence n'a été établie.
    pub fn tolerated(&self) -> Option<BTreeSet<String>> {
        fs::read_to_string(self.tolerated_path()).ok().and_then(|t| serde_json::from_str(&t).ok())
    }

    pub fn add_tolerated(&self, files: &BTreeSet<String>) {
        let mut set = self.tolerated().unwrap_or_default();
        set.extend(files.iter().cloned());
        self.write_tolerated(&set);
    }

    pub fn remove_tolerated(&self, files: &[String]) {
        if let Some(mut set) = self.tolerated() {
            for f in files {
                set.remove(f);
            }
            self.write_tolerated(&set);
        }
    }

    fn write_tolerated(&self, set: &BTreeSet<String>) {
        if let Ok(text) = serde_json::to_string_pretty(set) {
            let _ = crate::config::write_atomic(&self.tolerated_path(), text.as_bytes());
        }
    }

    /// Oublie les ensembles validés (après une erreur constatée en jeu malgré la vérification).
    pub fn forget_ok(&self, fp: &str) {
        let mut list = self.read_ok();
        list.retain(|f| f != fp);
        if let Ok(text) = serde_json::to_string(&list) {
            let _ = crate::config::write_atomic(&self.ok_cache(), text.as_bytes());
        }
    }
}

/// Empreinte d'un lancement : mods et versions, DLC désactivés, version du jeu, réglages de démarrage.
pub fn fingerprint(res: &Resolution, game: &str, settings: Option<&Path>) -> String {
    let mut h = Sha1::new();
    h.update(format!("game={game}\n"));
    for (n, v) in &res.mods {
        h.update(format!("{n}={v}\n"));
    }
    for b in &res.disabled_builtins {
        h.update(format!("-{b}\n"));
    }
    if let Some(bytes) = settings.and_then(|p| fs::read(p).ok()) {
        h.update(b"settings=");
        h.update(&bytes);
    }
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// Erreur relevée par Factorio, avec les mods cités (le plus probablement fautif en premier).
#[derive(Clone, Debug, PartialEq)]
pub struct Failure {
    pub text: String,
    pub culprits: Vec<String>,
    /// Erreur d'un mod pendant la partie (le chargement, lui, a réussi).
    pub runtime: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    /// Chargement réussi ; fichiers référencés mais introuvables (à comparer à la référence).
    Ok(Missing),
    Failed(Failure),
}

/// Fichier référencé : nombre de références et premiers prototypes qui l'utilisent.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Usage {
    pub count: usize,
    pub protos: Vec<String>,
}

/// Fichiers introuvables : chemin (« __mod__/… ») -> utilisation.
pub type Missing = BTreeMap<String, Usage>;

pub fn missing_paths(m: &Missing) -> BTreeSet<String> {
    m.keys().cloned().collect()
}

/// Échec dû à des fichiers introuvables absents de la référence (`tolerated`) : Factorio
/// s'arrêtera au début du chargement des sprites (« AtlasBuilder: File … not found »).
/// Chaque fichier est expliqué : prototypes qui l'utilisent, mods qui fournissent un fichier de
/// ce nom, mods dont le code le cite. Un mod qui fournit le fichier est le premier suspect : le
/// chemin lui était sans doute destiné, mais a été construit au nom d'un autre mod.
pub fn missing_failure(
    missing: &Missing,
    tolerated: &BTreeSet<String>,
    known: &BTreeSet<String>,
    mods_dir: &Path,
) -> Option<Failure> {
    let new: Vec<(&String, &Usage)> = missing.iter().filter(|(p, _)| !tolerated.contains(*p)).collect();
    if new.is_empty() {
        return None;
    }
    let mut text = format!(
        "{} fichier(s) graphique(s) ou sonore(s) introuvable(s), absents de la dernière configuration qui \
         fonctionnait : Factorio s'arrêtera au chargement des sprites.\n",
        new.len()
    );
    let explained: Vec<(String, Explanation)> =
        new.iter().take(5).map(|(p, _)| ((*p).clone(), explain(p, mods_dir))).collect();
    let mut suspects: Vec<String> = Vec::new();
    let mut owners: Vec<String> = Vec::new();
    for (i, (p, usage)) in new.iter().enumerate() {
        if let Some(owner) = owner_of(p) {
            if known.contains(owner) && !owners.iter().any(|o| o == owner) {
                owners.push(owner.to_string());
            }
        }
        if i >= 15 {
            continue;
        }
        text.push_str(&format!("\nFile {p} not found"));
        if !usage.protos.is_empty() {
            text.push_str(&format!(
                "\n    utilisé {} fois, par exemple dans : {}",
                usage.count,
                usage.protos.join(", ")
            ));
        }
        if let Some((_, ex)) = explained.iter().find(|(q, _)| q == *p) {
            if !ex.provided_by.is_empty() {
                let list: Vec<String> = ex.provided_by.iter().map(|(m, f)| format!("{m} ({f})")).collect();
                text.push_str(&format!("\n    fichier de ce nom fourni par : {}", list.join(", ")));
            }
            if !ex.cited_by.is_empty() {
                let list: Vec<String> = ex.cited_by.iter().map(|(m, f)| format!("{m} ({f})")).collect();
                text.push_str(&format!("\n    cité dans le code de : {}", list.join(", ")));
            }
            for (m, _) in &ex.provided_by {
                if known.contains(m) && owner_of(p) != Some(m.as_str()) && !suspects.contains(m) {
                    suspects.push(m.clone());
                }
            }
        }
    }
    if new.len() > 15 {
        text.push_str(&format!("\n… et {} autres", new.len() - 15));
    }
    if !suspects.is_empty() {
        text.push_str(&format!(
            "\n\nPiste : {} fournit ce fichier, mais le chemin a été construit au nom d'un autre mod. \
             Cela arrive quand deux mods remplacent la même variable globale (par exemple « util ») \
             pendant l'étape data.",
            suspects.join(", ")
        ));
    }
    let mut culprits = suspects;
    for o in owners {
        if !culprits.contains(&o) {
            culprits.push(o);
        }
    }
    Some(Failure { text, culprits, runtime: false })
}

fn owner_of(path: &str) -> Option<&str> {
    path.strip_prefix("__").and_then(|r| r.split_once("__/")).map(|(m, _)| m)
}

/// Origine probable d'un fichier introuvable, d'après le contenu des mods.
#[derive(Debug, Default)]
struct Explanation {
    /// Mods contenant un fichier de même nom (mod, chemin dans le mod).
    provided_by: Vec<(String, String)>,
    /// Mods dont un fichier Lua cite ce nom (mod, « fichier.lua:ligne »).
    cited_by: Vec<(String, String)>,
}

fn explain(path: &str, mods_dir: &Path) -> Explanation {
    let mut ex = Explanation::default();
    let Some(file_name) = path.rsplit('/').next().filter(|f| !f.is_empty()) else { return ex };
    let rel = path.strip_prefix("__").and_then(|r| r.split_once("__/")).map(|(_, r)| r.to_lowercase());
    let lower_name = file_name.to_lowercase();
    let Ok(rd) = fs::read_dir(mods_dir) else { return ex };
    for e in rd.flatten() {
        let p = e.path();
        let Some((name, _)) = p.file_name().and_then(|f| f.to_str()).and_then(crate::factorio::split_zip_name) else {
            continue;
        };
        let Ok(file) = fs::File::open(&p) else { continue };
        let Ok(mut zip) = zip::ZipArchive::new(file) else { continue };
        let names: Vec<String> = zip.file_names().map(str::to_string).collect();
        // Fichier de même chemin relatif en priorité, sinon de même nom.
        let inner = |n: &str| n.split_once('/').map(|(_, r)| r.to_string()).unwrap_or_default();
        let exact = names.iter().find(|n| rel.as_deref() == Some(inner(n).to_lowercase().as_str()));
        let same_name = names.iter().find(|n| n.to_lowercase().ends_with(&format!("/{lower_name}")));
        if let Some(n) = exact.or(same_name) {
            ex.provided_by.push((name.clone(), inner(n)));
        }
        for n in names.iter().filter(|n| n.ends_with(".lua")) {
            let Ok(mut f) = zip.by_name(n) else { continue };
            let mut src = String::new();
            if f.read_to_string(&mut src).is_err() {
                continue;
            }
            if let Some(line) = src.lines().position(|l| l.contains(file_name)) {
                ex.cited_by.push((name.clone(), format!("{}:{}", inner(n), line + 1)));
                break;
            }
        }
    }
    ex
}

/// Compare les fichiers introuvables à la référence. Sans référence, celle-ci est établie
/// à partir de ce premier ensemble qui se charge (puis confirmée par les vrais lancements).
pub fn judge(dirs: &CheckDirs, known: &BTreeSet<String>, outcome: Outcome) -> Outcome {
    match outcome {
        Outcome::Ok(missing) => match dirs.tolerated() {
            None => {
                dirs.add_tolerated(&missing_paths(&missing));
                Outcome::Ok(missing)
            }
            Some(t) => match missing_failure(&missing, &t, known, &dirs.mods()) {
                Some(f) => Outcome::Failed(f),
                None => Outcome::Ok(missing),
            },
        },
        o => o,
    }
}

fn is_media_ref(s: &str) -> bool {
    if !s.starts_with("__") {
        return false;
    }
    let lower = s.to_ascii_lowercase();
    [".png", ".jpg", ".jpeg", ".ogg", ".wav", ".voc"].iter().any(|e| lower.ends_with(e))
}

/// Fichiers cités par un prototype, selon les règles du moteur : un `filename` à côté de
/// `stripes` est ignoré, et seule la première image d'une liste `filenames` est comptée
/// (les suivantes peuvent dépasser le nombre d'images réellement utilisées).
fn collect_refs(v: &serde_json::Value, key: &str, out: &mut Vec<String>) {
    use serde_json::Value;
    match v {
        Value::String(s) if is_media_ref(s) => out.push(s.clone()),
        Value::Object(m) => {
            let stripes = m.contains_key("stripes");
            for (k, x) in m {
                if !(k == "filename" && stripes) {
                    collect_refs(x, k, out);
                }
            }
        }
        Value::Array(a) => {
            let items = if key == "filenames" { &a[..a.len().min(1)] } else { &a[..] };
            for x in items {
                collect_refs(x, key, out);
            }
        }
        _ => {}
    }
}

/// Parcours en flux de data-raw-dump.json (plusieurs centaines de Mo) : un seul prototype
/// à la fois en mémoire.
struct Types<'a>(&'a mut Missing);
struct Protos<'a> {
    refs: &'a mut Missing,
    kind: String,
}

impl<'de> serde::de::Visitor<'de> for Types<'_> {
    type Value = ();
    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("data.raw")
    }
    fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> std::result::Result<(), A::Error> {
        while let Some(kind) = map.next_key::<String>()? {
            map.next_value_seed(Protos { refs: &mut *self.0, kind })?;
        }
        Ok(())
    }
}

impl<'de> serde::de::DeserializeSeed<'de> for Protos<'_> {
    type Value = ();
    fn deserialize<D: serde::Deserializer<'de>>(self, d: D) -> std::result::Result<(), D::Error> {
        d.deserialize_any(self)
    }
}

impl<'de> serde::de::Visitor<'de> for Protos<'_> {
    type Value = ();
    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("prototypes")
    }
    fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> std::result::Result<(), A::Error> {
        let mut found = Vec::new();
        while let Some(name) = map.next_key::<String>()? {
            let value: serde_json::Value = map.next_value()?;
            found.clear();
            collect_refs(&value, "", &mut found);
            for r in found.drain(..) {
                let u = self.refs.entry(r).or_default();
                u.count += 1;
                let id = format!("{}/{name}", self.kind);
                if u.protos.len() < 3 && !u.protos.contains(&id) {
                    u.protos.push(id);
                }
            }
        }
        Ok(())
    }
    // Les autres formes (rares à ce niveau) sont ignorées.
    fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> std::result::Result<(), A::Error> {
        while seq.next_element::<serde::de::IgnoredAny>()?.is_some() {}
        Ok(())
    }
    fn visit_str<E>(self, _: &str) -> std::result::Result<(), E> {
        Ok(())
    }
    fn visit_bool<E>(self, _: bool) -> std::result::Result<(), E> {
        Ok(())
    }
    fn visit_i64<E>(self, _: i64) -> std::result::Result<(), E> {
        Ok(())
    }
    fn visit_u64<E>(self, _: u64) -> std::result::Result<(), E> {
        Ok(())
    }
    fn visit_f64<E>(self, _: f64) -> std::result::Result<(), E> {
        Ok(())
    }
    fn visit_unit<E>(self) -> std::result::Result<(), E> {
        Ok(())
    }
}

fn scan_refs(dump: &[u8]) -> Result<Missing> {
    use serde::Deserializer;
    let mut refs = Missing::new();
    let mut de = serde_json::Deserializer::from_slice(dump);
    (&mut de).deserialize_map(Types(&mut refs))?;
    Ok(refs)
}

/// Fichiers (images, sons) référencés dans `dump` mais absents des mods de `mods_dir`
/// ou des données du jeu (`data_dir`, pour base, core et les DLC).
pub fn missing_files(dump: &[u8], mods_dir: &Path, data_dir: Option<&Path>) -> Result<Missing> {
    let refs = scan_refs(dump)?;
    // Contenu des zips : chemin relatif au dossier racine du mod, en minuscules.
    let mut entries: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for e in fs::read_dir(mods_dir)?.flatten() {
        let path = e.path();
        let Some((name, _)) = path.file_name().and_then(|f| f.to_str()).and_then(crate::factorio::split_zip_name) else {
            continue;
        };
        let Ok(file) = fs::File::open(&path) else { continue };
        let Ok(zip) = zip::ZipArchive::new(file) else { continue };
        let set = zip.file_names().filter_map(|n| n.split_once('/').map(|(_, r)| r.to_lowercase())).collect();
        entries.insert(name, set);
    }
    let mut missing = Missing::new();
    for (r, usage) in refs {
        let Some((m, rel)) = r.strip_prefix("__").and_then(|x| x.split_once("__/")) else { continue };
        let mut rel = rel.to_string();
        while rel.contains("//") {
            rel = rel.replace("//", "/");
        }
        let found = match entries.get(m) {
            Some(set) => set.contains(&rel.to_lowercase()),
            None => data_dir.is_some_and(|d| d.join(m).join(&rel).exists()),
        };
        if !found {
            missing.insert(r, usage);
        }
    }
    Ok(missing)
}

/// Correspondance titre -> nom pour les messages qui citent les mods par leur titre
/// (« Modifications: Base mod › Mod A »).
pub fn titles(index: &Index, res: &Resolution) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for (name, ver) in &res.mods {
        if let Some(info) = store::find(index, name, ver) {
            if !info.title.trim().is_empty() {
                out.insert(info.title.trim().to_string(), name.clone());
            }
        }
    }
    out
}

/// Bloc d'erreur du log (entre « ------------- Error ------------- » et la ligne de tirets).
fn error_block(log: &str) -> Option<String> {
    let start = log.rfind("------------- Error -------------")?;
    let body = &log[start + "------------- Error -------------".len()..];
    let end = body.find("\n---------------------------------").unwrap_or(body.len());
    Some(body[..end].trim().to_string())
}

/// Erreur d'exécution d'un mod en partie (« The mod X caused a non-recoverable error »).
fn runtime_error(log: &str) -> Option<String> {
    let idx = log.rfind("caused a non-recoverable error")?;
    let line_start = log[..idx].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let tail = &log[line_start..];
    // Le message continue sur les lignes sans horodatage qui suivent.
    let mut out = Vec::new();
    for (i, line) in tail.lines().enumerate() {
        let stamped = line.trim_start().split_whitespace().next().is_some_and(|t| t.parse::<f64>().is_ok());
        if i > 0 && stamped {
            break;
        }
        out.push(line);
        if out.len() > 40 {
            break;
        }
    }
    let text = out.join("\n");
    // Retire l'horodatage et la source (« 12.345 Error MainLoop.cpp:1234: »).
    let text = match text.find(".cpp:") {
        Some(i) => text[i..].split_once(": ").map(|(_, r)| r.to_string()).unwrap_or(text.clone()),
        None => text,
    };
    Some(text.trim().to_string())
}

/// Mods cités dans un message d'erreur. `known` : noms chargés ; `titles` : titre -> nom.
pub fn culprits(text: &str, known: &BTreeSet<String>, titles: &BTreeMap<String, String>) -> Vec<String> {
    let mut primary: Vec<String> = Vec::new();
    let mut secondary: Vec<String> = Vec::new();
    let resolve = |s: &str| -> Option<String> {
        let s = s.trim().trim_matches(|c| c == '"' || c == '.' || c == ',' || c == '(' || c == ')');
        if known.contains(s) {
            Some(s.to_string())
        } else {
            titles.get(s).cloned()
        }
    };
    let push = |list: &mut Vec<String>, name: Option<String>| {
        if let Some(n) = name {
            if !list.contains(&n) {
                list.push(n);
            }
        }
    };
    for line in text.lines() {
        if let Some(rest) = line.split("Failed to load mod \"").nth(1) {
            push(&mut primary, rest.split('"').next().and_then(resolve));
        }
        if let Some(rest) = line.split("removed by ").nth(1) {
            for part in rest.trim_end_matches('.').split([',', '&']).flat_map(|p| p.split(" and ")) {
                push(&mut primary, resolve(part));
            }
        }
        if let Some(rest) = line.split("The mod ").nth(1) {
            if let Some(name) = rest.split(" caused").next() {
                // « The mod Titre (1.2.3) caused… » : on retire la version.
                let name = name.rsplit_once(" (").map(|(n, _)| n).unwrap_or(name);
                push(&mut primary, resolve(name));
            }
        }
        if let Some(rest) = line.trim_start().strip_prefix("Modifications:") {
            // Le dernier mod de la chaîne est celui qui a laissé le prototype dans cet état.
            for part in rest.split('›').rev() {
                push(&mut secondary, resolve(part));
            }
        }
        if let Some(rest) = line.strip_prefix("• ") {
            push(&mut primary, resolve(rest));
        }
        let mut cur = line;
        while let Some(i) = cur.find("__") {
            let after = &cur[i + 2..];
            let Some(j) = after.find("__") else { break };
            push(&mut secondary, resolve(&after[..j]));
            cur = &after[j + 2..];
        }
    }
    for s in secondary {
        if !primary.contains(&s) {
            primary.push(s);
        }
    }
    primary
}

/// Analyse un log de Factorio. `None` : aucune erreur de chargement ou d'exécution.
pub fn parse_log(log: &str, known: &BTreeSet<String>, titles: &BTreeMap<String, String>) -> Option<Failure> {
    let (text, runtime) = match error_block(log).or_else(|| atlas_error(log)) {
        Some(t) => (t, false),
        None => (runtime_error(log)?, true),
    };
    let culprits = culprits(&text, known, titles);
    Some(Failure { text, culprits, runtime })
}

/// Fichier de sprite introuvable au chargement des sprites
/// (« Error AtlasBuilder.cpp:1383: File __X__/a.png not found; mods: … » puis les prototypes concernés).
fn atlas_error(log: &str) -> Option<String> {
    let idx = log.find("not found; mods:")?;
    let start = log[..idx].rfind("File __")?;
    let mut out = Vec::new();
    for (i, line) in log[start..].lines().enumerate() {
        if i > 0 && !line.starts_with("    ") {
            break;
        }
        // La liste « mods: » (historique du prototype) est très longue : elle ne désigne pas le coupable.
        let line = if i == 0 { line.split("; mods:").next().unwrap_or(line) } else { line };
        out.push(line.trim().to_string());
        if out.len() > 25 {
            out.push("…".into());
            break;
        }
    }
    Some(out.join("\n"))
}

/// Fichiers signalés introuvables par le vrai jeu (pour les retirer de la référence).
pub fn atlas_missing(log: &str) -> Vec<String> {
    log.lines()
        .filter_map(|l| l.split("File ").nth(1))
        .filter_map(|r| r.split_once(" not found;").map(|(p, _)| p.to_string()))
        .collect()
}

/// Le log montre que le jeu a passé le chargement des prototypes.
pub fn reached_init(log: &str) -> bool {
    log.contains("Factorio initialised")
}

/// Progression approximative d'après les lignes « Loading mod X 1.0 (data.lua) ».
struct Progress {
    total: usize,
    stage: usize,
    seen: BTreeSet<String>,
    value: f32,
}

impl Progress {
    fn feed(&mut self, line: &str) -> Option<(f32, String)> {
        const STAGES: [&str; 3] = ["(data.lua)", "(data-updates.lua)", "(data-final-fixes.lua)"];
        if let Some(rest) = line.split("Loading mod ").nth(1) {
            if rest.starts_with("settings ") {
                return None;
            }
            let stage = STAGES.iter().position(|s| rest.trim_end().ends_with(s))?;
            if stage != self.stage {
                self.stage = stage;
                self.seen.clear();
            }
            let name = rest.split_whitespace().next().unwrap_or_default().to_string();
            self.seen.insert(name.clone());
            let frac = (self.seen.len() as f32 / self.total.max(1) as f32).min(1.0);
            self.value = self.value.max(0.85 * (stage as f32 + frac) / 3.0);
            let label = ["data", "data-updates", "data-final-fixes"][stage];
            return Some((self.value, format!("{label} : {name}")));
        }
        if line.contains("Checksum for core") {
            self.value = self.value.max(0.88);
            return Some((self.value, "chargement des prototypes".into()));
        }
        if line.contains("Prototype list checksum") {
            self.value = 0.93;
            return Some((self.value, "export des données".into()));
        }
        None
    }
}

/// Lance l'instance de vérification sur `dirs.mods()` (déjà rempli) et attend sa fin.
/// `cancel` arrête l'instance ; `progress` reçoit (0..1, étape en cours).
pub fn run(
    exe: &Path,
    dirs: &CheckDirs,
    mod_count: usize,
    known: &BTreeSet<String>,
    titles: &BTreeMap<String, String>,
    cancel: &AtomicBool,
    mut progress: impl FnMut(f32, &str),
) -> Result<Option<Outcome>> {
    dirs.prepare()?;
    let log_path = dirs.log();
    let _ = fs::remove_file(&log_path);
    let mut cmd = Command::new(exe);
    if let Some(dir) = exe.parent() {
        cmd.current_dir(dir);
    }
    cmd.arg("--config")
        .arg(dirs.config())
        .arg("--mod-directory")
        .arg(dirs.mods())
        .arg("--dump-data")
        .arg("--disable-audio")
        .arg("--nogamepad")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = cmd.spawn().with_context(|| format!("impossible de lancer {}", exe.display()))?;

    let mut prog = Progress { total: mod_count + 1, stage: 0, seen: BTreeSet::new(), value: 0.0 };
    let mut pos = 0u64;
    let mut pending = String::new();
    let mut log = String::new();
    let status = loop {
        if cancel.load(Ordering::SeqCst) {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(None);
        }
        // Suivi du log en direct (Factorio l'écrit au fil de l'eau).
        if let Ok(mut f) = fs::File::open(&log_path) {
            if f.seek(SeekFrom::Start(pos)).is_ok() {
                let mut buf = Vec::new();
                if let Ok(n) = f.read_to_end(&mut buf) {
                    pos += n as u64;
                    pending.push_str(&String::from_utf8_lossy(&buf));
                    while let Some(i) = pending.find('\n') {
                        let line: String = pending.drain(..=i).collect();
                        if let Some((p, label)) = prog.feed(&line) {
                            progress(p, &label);
                        }
                        log.push_str(&line);
                    }
                }
            }
        }
        if let Some(status) = child.try_wait()? {
            break status;
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    // Relecture complète : la fin du log a pu être écrite après le dernier passage.
    let log = fs::read_to_string(&log_path).unwrap_or(log + &pending);
    let output = dirs.write().join("script-output");
    let result: Result<Option<Outcome>> = if let Some(f) = parse_log(&log, known, titles) {
        Ok(Some(Outcome::Failed(f)))
    } else if status.success() && reached_init(&log) {
        progress(0.96, "recherche des fichiers introuvables");
        (|| {
            let dump = fs::read(output.join("data-raw-dump.json")).context("lecture de data-raw-dump.json")?;
            let data = crate::factorio::data_dir(exe);
            Ok(Some(Outcome::Ok(missing_files(&dump, &dirs.mods(), data.as_deref())?)))
        })()
    } else {
        Ok(None)
    };
    let _ = fs::remove_dir_all(&output);
    if let Some(outcome) = result? {
        return Ok(Some(outcome));
    }
    if log.contains("lock") && log.contains("another instance") {
        return Err(anyhow!("une autre instance de vérification est en cours"));
    }
    let tail: Vec<&str> = log.lines().rev().take(15).collect();
    Ok(Some(Outcome::Failed(Failure {
        text: format!(
            "Factorio s'est arrêté sans message d'erreur reconnu (code {}).\n\n{}",
            status.code().map(|c| c.to_string()).unwrap_or("?".into()),
            tail.into_iter().rev().collect::<Vec<_>>().join("\n")
        ),
        culprits: vec![],
        runtime: false,
    })))
}

/// Dépendances obligatoires (non intégrées) de `names`, récursivement, parmi les mods de `res`.
pub fn required_closure(names: &BTreeSet<String>, res: &Resolution, index: &Index, builtins: &[String]) -> BTreeSet<String> {
    let mut out = names.clone();
    let mut stack: Vec<String> = names.iter().cloned().collect();
    while let Some(n) = stack.pop() {
        let Some(info) = res.mods.get(&n).and_then(|v| store::find(index, &n, v)) else { continue };
        for dep in info.deps() {
            if dep.kind.is_required() && !is_builtin(&dep.name, builtins) && res.mods.contains_key(&dep.name) && out.insert(dep.name.clone()) {
                stack.push(dep.name.clone());
            }
        }
    }
    out
}

/// Sous-ensemble d'une résolution.
pub fn subset(res: &Resolution, names: &BTreeSet<String>) -> Resolution {
    Resolution {
        mods: res.mods.iter().filter(|(n, _)| names.contains(*n)).map(|(n, v)| (n.clone(), *v)).collect(),
        conflicts: vec![],
        disabled_builtins: res.disabled_builtins.clone(),
    }
}

/// Résultat de la recherche du mod partenaire d'une erreur.
#[derive(Clone, Debug, PartialEq)]
pub enum Bisect {
    /// Le mod en cause plante même seul (avec ses dépendances).
    Alone,
    /// Le mod en cause plante à cause de ce mod.
    Partner(String),
    /// Impossible de conclure (l'erreur ne se reproduit pas de façon stable).
    Unknown,
}

/// Recherche par dichotomie du mod qui, ajouté à `culprit` (et ses dépendances), reproduit l'erreur.
/// `test(ensemble)` lance une vérification et renvoie `Some(true)` si l'erreur de `culprit`
/// réapparaît, `Some(false)` sinon, `None` si la recherche est annulée.
pub fn bisect(
    culprit: &str,
    res: &Resolution,
    index: &Index,
    builtins: &[String],
    mut test: impl FnMut(&BTreeSet<String>, usize, usize) -> Result<Option<bool>>,
) -> Result<Option<Bisect>> {
    let keep = required_closure(&BTreeSet::from([culprit.to_string()]), res, index, builtins);
    let mut candidates: Vec<String> = res.mods.keys().filter(|n| !keep.contains(*n)).cloned().collect();
    let with = |extra: &[String]| -> BTreeSet<String> {
        let mut s = keep.clone();
        s.extend(extra.iter().cloned());
        required_closure(&s, res, index, builtins)
    };
    // Étapes prévues : une pour le mod seul, log2(n) pour la dichotomie.
    let planned = 1 + (candidates.len().max(1) as f32).log2().ceil() as usize;
    let mut step = 1;
    match test(&keep, step, planned)? {
        None => return Ok(None),
        Some(true) => return Ok(Some(Bisect::Alone)),
        Some(false) => {}
    }
    while candidates.len() > 1 {
        step += 1;
        let half = candidates.len() / 2;
        let (a, b) = candidates.split_at(half);
        let (a, b) = (a.to_vec(), b.to_vec());
        match test(&with(&a), step, planned.max(step))? {
            None => return Ok(None),
            Some(true) => candidates = a,
            Some(false) => candidates = b,
        }
    }
    let Some(partner) = candidates.pop() else { return Ok(Some(Bisect::Unknown)) };
    // Confirmation : l'erreur doit se reproduire avec ce seul mod en plus.
    match test(&with(std::slice::from_ref(&partner)), step + 1, planned.max(step + 1))? {
        None => Ok(None),
        Some(true) => Ok(Some(Bisect::Partner(partner))),
        Some(false) => Ok(Some(Bisect::Unknown)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::version::Version;

    fn known(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_lua_error() {
        let log = "   0.300 Error Util.cpp:81: Failed to load mod \"modB\": __modB__/data-updates.lua:1: attempt to index field 'iron-gear-wheel' (a nil value)\n\
stack traceback:\n\t__modB__/data-updates.lua:1: in main chunk\n\n\
------------- Error -------------\n\
Failed to load mod \"modB\": __modB__/data-updates.lua:1: attempt to index field 'iron-gear-wheel' (a nil value)\n\
stack traceback:\n\t__modB__/data-updates.lua:1: in main chunk\n\t__modA__/lib.lua:3: in function\n\
---------------------------------\n   0.40 Goodbye\n";
        let f = parse_log(log, &known(&["modA", "modB"]), &BTreeMap::new()).unwrap();
        assert!(f.text.starts_with("Failed to load mod \"modB\""));
        assert_eq!(f.culprits, vec!["modB", "modA"]);
    }

    #[test]
    fn parses_removed_by_and_modifications() {
        let log = "------------- Error -------------\nError in assignID: item with name 'iron-plate' does not exist. It was removed by modC.\n\nSource: steam-power (technology).\n---------------------------------\n";
        let f = parse_log(log, &known(&["modC"]), &BTreeMap::new()).unwrap();
        assert_eq!(f.culprits, vec!["modC"]);

        let log = "------------- Error -------------\nError while loading entity prototype \"assembling-machine-1\" (assembling-machine): bad\nModifications: Base mod › modD title › modE title\n---------------------------------\n";
        let titles = BTreeMap::from([("modD title".to_string(), "modD".to_string()), ("modE title".to_string(), "modE".to_string())]);
        let f = parse_log(log, &known(&["modD", "modE"]), &titles).unwrap();
        assert_eq!(f.culprits, vec!["modE", "modD"]);
    }

    #[test]
    fn parses_runtime_error_and_ignores_noise() {
        let ok = "  55.403 Error ControlSettings.cpp:1057: construction-drone-toggle (custom-input): unknown key_sequence: SHIFT + ]\n  57.1 Goodbye\n";
        assert!(parse_log(ok, &known(&[]), &BTreeMap::new()).is_none());
        let log = " 120.0 Error MainLoop.cpp:1424: Exception at tick 5: The mod Repair Turret (1.4.5) caused a non-recoverable error.\nPlease report this error to the mod author.\n\nError while running event Repair_Turret_2::on_tick (ID 0)\n__Repair_Turret_2__/control.lua:10: boom\n 121.0 Goodbye\n";
        let titles = BTreeMap::from([("Repair Turret".to_string(), "Repair_Turret_2".to_string())]);
        let f = parse_log(log, &known(&["Repair_Turret_2"]), &titles).unwrap();
        assert!(f.text.starts_with("Exception at tick 5"));
        assert_eq!(f.culprits, vec!["Repair_Turret_2"]);
    }

    #[test]
    fn parses_atlas_error() {
        let log = "  71.281 Parallel sprite loader initialized (threads: 32, bitmaps: 63)\n  71.364 Error AtlasBuilder.cpp:1383: File __CDF__/data/tf_util/empty-sprite.png not found; mods: razi-protocol CDF Krastorio2\n         Prototypes using the sprite:\n           data.raw[\"roboport\"][\"x\"]\n  71.889 Initial atlas bitmap size is 16384\n  72.563 Factorio initialised\n";
        let f = parse_log(log, &known(&["CDF", "razi-protocol", "Krastorio2"]), &BTreeMap::new()).unwrap();
        assert!(!f.runtime);
        assert!(f.text.starts_with("File __CDF__/data/tf_util/empty-sprite.png not found"));
        assert!(!f.text.contains("Krastorio2"));
        assert_eq!(f.culprits, vec!["CDF"]);
        assert_eq!(atlas_missing(log), vec!["__CDF__/data/tf_util/empty-sprite.png"]);
    }

    #[test]
    fn scan_follows_engine_rules() {
        let json = r#"{"explosion":{"e":{"animations":[{"filename":"__a__/x.png"},
            {"filename":"__a__/ignored.png","stripes":[{"filename":"__a__/s1.png"}]}]}},
            "animation":{"anim":{"filenames":["__b__/f1.png","__b__/f2.png"],"icon":"__b__/i.PNG","name":"__notafile__"},
            "other":{"icon":"__a__/x.png"}}}"#;
        let refs = scan_refs(json.as_bytes()).unwrap();
        let got: Vec<&str> = refs.keys().map(|s| s.as_str()).collect();
        assert_eq!(got, vec!["__a__/s1.png", "__a__/x.png", "__b__/f1.png", "__b__/i.PNG"]);
        assert_eq!(refs["__a__/x.png"].count, 2);
        assert_eq!(refs["__a__/x.png"].protos, vec!["explosion/e", "animation/other"]);
    }

    #[test]
    fn missing_failure_reports_only_new_files() {
        let mut missing = Missing::new();
        missing.insert("__base__/old.ogg".into(), Usage::default());
        missing.insert("__CDF__/empty.png".into(), Usage { count: 650, protos: vec!["explosion/e".into()] });
        let tolerated: BTreeSet<String> = ["__base__/old.ogg".to_string()].into();
        let dir = std::env::temp_dir().join(format!("fmm-missing-{}", crate::modpack::new_id()));
        fs::create_dir_all(&dir).unwrap();
        let f = missing_failure(&missing, &tolerated, &known(&["CDF"]), &dir).unwrap();
        assert_eq!(f.culprits, vec!["CDF"]);
        assert!(f.text.contains("File __CDF__/empty.png not found"));
        assert!(f.text.contains("utilisé 650 fois"));
        assert!(missing_failure(&missing, &missing_paths(&missing), &known(&["CDF"]), &dir).is_none());
        let _ = fs::remove_dir_all(dir);
    }

    /// Sur un vrai dump : FMM_DUMP=data-raw-dump.json FMM_MODS=dossier FMM_DATA=Factorio/data cargo test -- --ignored
    #[test]
    #[ignore]
    fn real_dump_missing_files() {
        let (Ok(dump), Ok(mods)) = (std::env::var("FMM_DUMP"), std::env::var("FMM_MODS")) else { return };
        let data = std::env::var("FMM_DATA").ok().map(PathBuf::from);
        let t = std::time::Instant::now();
        let bytes = fs::read(&dump).unwrap();
        let missing = missing_files(&bytes, Path::new(&mods), data.as_deref()).unwrap();
        println!("{} introuvables en {:?}", missing.len(), t.elapsed());
        let target: Missing = missing.into_iter().filter(|(m, _)| m.contains("empty-sprite")).collect();
        let t = std::time::Instant::now();
        if let Some(f) = missing_failure(&target, &BTreeSet::new(), &known(&["Repair_Turret_2", "Construction_Drones_Forked"]), Path::new(&mods)) {
            println!("{}
suspects : {:?} ({:?})", f.text, f.culprits, t.elapsed());
        }
    }

    #[test]
    fn fingerprint_changes_with_mods() {
        let mut r = Resolution::default();
        r.mods.insert("a".into(), Version(1, 0, 0));
        let f1 = fingerprint(&r, "2.1", None);
        r.mods.insert("b".into(), Version(1, 0, 0));
        assert_ne!(f1, fingerprint(&r, "2.1", None));
        assert_ne!(fingerprint(&r, "2.0", None), fingerprint(&r, "2.1", None));
    }

    #[test]
    fn bisect_finds_partner() {
        let mut r = Resolution::default();
        for n in ["a", "b", "c", "d", "e", "f", "g", "x"] {
            r.mods.insert(n.into(), Version(1, 0, 0));
        }
        let index = Index::new();
        let mut runs = 0;
        let out = bisect("x", &r, &index, &[], |set, _, _| {
            runs += 1;
            Ok(Some(set.contains("x") && set.contains("e")))
        })
        .unwrap();
        assert_eq!(out, Some(Bisect::Partner("e".into())));
        assert!(runs <= 5, "{runs} essais");
        let out = bisect("x", &r, &index, &[], |_, _, _| Ok(Some(true))).unwrap();
        assert_eq!(out, Some(Bisect::Alone));
    }
}
