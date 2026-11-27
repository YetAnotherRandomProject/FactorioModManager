//! Modpacks, état persistant et résolution de l'ensemble de mods à charger.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::write_atomic;
use crate::store::Index;
use crate::version::{Version, is_builtin};

fn t() -> bool {
    true
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct PackEntry {
    pub name: String,
    pub version: String,
    #[serde(default = "t")]
    pub enabled: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Modpack {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Mod du portail (catégorie "mod-packs") dont ce modpack est dérivé.
    #[serde(default)]
    pub base_mod: Option<String>,
    #[serde(default)]
    pub factorio_version: Option<String>,
    #[serde(default)]
    pub mods: Vec<PackEntry>,
    /// Mods intégrés (DLC) désactivés par ce modpack.
    #[serde(default)]
    pub disabled_builtins: Vec<String>,
    #[serde(default)]
    pub saves: Vec<PathBuf>,
}

impl Modpack {
    pub fn new(name: &str) -> Modpack {
        Modpack { id: new_id(), name: name.to_string(), ..Default::default() }
    }

    /// Ajoute ou met à jour un mod (une seule version par modpack).
    pub fn set_mod(&mut self, name: &str, version: &Version) {
        if let Some(e) = self.mods.iter_mut().find(|e| e.name == name) {
            e.version = version.to_string();
        } else {
            self.mods.push(PackEntry { name: name.to_string(), version: version.to_string(), enabled: true });
            self.mods.sort_by_key(|e| e.name.to_lowercase());
        }
    }

    pub fn entry(&self, name: &str) -> Option<&PackEntry> {
        self.mods.iter().find(|e| e.name == name)
    }
}

pub fn new_id() -> String {
    let n = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    format!("{n:x}")
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct State {
    #[serde(default)]
    pub packs: Vec<Modpack>,
    /// Identifiants des modpacks actifs, dans l'ordre d'activation.
    #[serde(default)]
    pub active: Vec<String>,
    /// Choix mémorisés pour les conflits de versions : nom du mod -> version.
    #[serde(default)]
    pub conflict_choices: BTreeMap<String, String>,
}

impl State {
    pub fn load(path: &Path) -> State {
        fs::read_to_string(path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }
    pub fn save(&self, path: &Path) -> Result<()> {
        write_atomic(path, serde_json::to_string_pretty(self)?.as_bytes())
    }
    pub fn pack(&self, id: &str) -> Option<&Modpack> {
        self.packs.iter().find(|p| p.id == id)
    }
    pub fn pack_mut(&mut self, id: &str) -> Option<&mut Modpack> {
        self.packs.iter_mut().find(|p| p.id == id)
    }
    pub fn is_active(&self, id: &str) -> bool {
        self.active.iter().any(|a| a == id)
    }
    pub fn active_packs(&self) -> Vec<&Modpack> {
        self.active.iter().filter_map(|id| self.pack(id)).collect()
    }
    /// Noms des modpacks qui utilisent (nom, version).
    pub fn packs_using(&self, name: &str, version: Option<&str>) -> Vec<String> {
        self.packs
            .iter()
            .filter(|p| p.mods.iter().any(|e| e.name == name && version.map(|v| v == e.version).unwrap_or(true)))
            .map(|p| p.name.clone())
            .collect()
    }
}

#[derive(Clone, Debug)]
pub struct ConflictOption {
    pub version: String,
    pub packs: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct Conflict {
    pub name: String,
    pub options: Vec<ConflictOption>,
}

#[derive(Clone, Debug, Default)]
pub struct Resolution {
    pub mods: BTreeMap<String, Version>,
    pub conflicts: Vec<Conflict>,
    pub disabled_builtins: BTreeSet<String>,
}

/// Calcule l'union des modpacks actifs. `choices` tranche les conflits déjà arbitrés.
pub fn resolve(state: &State, choices: &BTreeMap<String, String>) -> Resolution {
    let mut wanted: BTreeMap<String, BTreeMap<String, Vec<String>>> = BTreeMap::new();
    let mut res = Resolution::default();
    for pack in state.active_packs() {
        for e in pack.mods.iter().filter(|e| e.enabled) {
            wanted
                .entry(e.name.clone())
                .or_default()
                .entry(e.version.clone())
                .or_default()
                .push(pack.name.clone());
        }
        res.disabled_builtins.extend(pack.disabled_builtins.iter().cloned());
    }
    for (name, versions) in wanted {
        let pick = if versions.len() == 1 {
            versions.keys().next().cloned()
        } else if let Some(c) = choices.get(&name).filter(|c| versions.contains_key(*c)) {
            Some(c.clone())
        } else {
            let mut opts: Vec<ConflictOption> = versions
                .iter()
                .map(|(v, p)| ConflictOption { version: v.clone(), packs: p.clone() })
                .collect();
            opts.sort_by_key(|o| std::cmp::Reverse(o.version.parse::<Version>().unwrap_or_default()));
            let first = opts[0].version.clone();
            res.conflicts.push(Conflict { name: name.clone(), options: opts });
            Some(first)
        };
        if let Some(v) = pick.and_then(|v| v.parse().ok()) {
            res.mods.insert(name, v);
        }
    }
    res
}

/// Problème détecté dans un ensemble de mods, avec les mods dont la désactivation le résout.
#[derive(Clone, Debug, PartialEq)]
pub struct Issue {
    pub text: String,
    /// Un seul candidat : correction automatique possible. Deux (incompatibilité) : au choix.
    pub disable: Vec<String>,
}

impl Issue {
    pub fn is_automatic(&self) -> bool {
        self.disable.len() == 1
    }
}

/// Problèmes d'une résolution : mods absents du dépôt, prévus pour une autre version de Factorio,
/// dépendances obligatoires manquantes ou de mauvaise version, DLC requis désactivé, incompatibilités.
/// `game` : version du jeu installé (vide = pas de contrôle de version).
pub fn issues(res: &Resolution, index: &Index, builtins: &[String], game: &str) -> Vec<Issue> {
    let mut out = Vec::new();
    let mut pairs: BTreeSet<(String, String)> = BTreeSet::new();
    for (name, ver) in &res.mods {
        let one = |text: String| Issue { text, disable: vec![name.clone()] };
        let Some(info) = crate::store::find(index, name, ver) else {
            out.push(one(format!("{name} {ver} n'est pas dans le dépôt")));
            continue;
        };
        if !crate::version::factorio_compatible(&info.factorio_version, game) {
            out.push(one(format!(
                "{name} {ver} est prévu pour Factorio {} (jeu installé : {game})",
                info.factorio_version
            )));
        }
        for dep in info.deps() {
            if is_builtin(&dep.name, builtins) {
                if dep.kind.is_required() && res.disabled_builtins.contains(&dep.name) {
                    out.push(one(format!("{name} requiert {} qui est désactivé", dep.name)));
                }
                continue;
            }
            let present = res.mods.get(&dep.name);
            if dep.kind.is_required() {
                match present {
                    None => out.push(one(format!("{name} requiert {} {}", dep.name, dep.constraint_str()))),
                    Some(v) if !dep.matches(v) => out.push(one(format!(
                        "{name} requiert {} {} (version chargée : {v})",
                        dep.name,
                        dep.constraint_str()
                    ))),
                    _ => {}
                }
            } else if dep.kind == crate::version::DepKind::Incompatible && present.is_some() {
                // Une seule entrée par paire, même si les deux mods se déclarent incompatibles.
                let pair = if *name < dep.name { (name.clone(), dep.name.clone()) } else { (dep.name.clone(), name.clone()) };
                if pairs.insert(pair) {
                    out.push(Issue {
                        text: format!("{name} est incompatible avec {}", dep.name),
                        disable: vec![name.clone(), dep.name.clone()],
                    });
                }
            }
        }
    }
    out
}

/// Désactive dans le modpack tous les mods en cause d'un problème à candidat unique, jusqu'à ce
/// qu'il n'en reste plus (un mod qui dépendait d'un mod désactivé l'est à son tour).
/// Les incompatibilités, qui demandent un choix, ne sont pas traitées. Renvoie les mods désactivés.
pub fn auto_disable(pack: &mut Modpack, index: &Index, builtins: &[String], game: &str) -> Vec<String> {
    let mut disabled = Vec::new();
    loop {
        let single = State { packs: vec![pack.clone()], active: vec![pack.id.clone()], ..Default::default() };
        let res = resolve(&single, &BTreeMap::new());
        let names: BTreeSet<String> = issues(&res, index, builtins, game)
            .into_iter()
            .filter(Issue::is_automatic)
            .flat_map(|i| i.disable)
            .collect();
        if names.is_empty() {
            return disabled;
        }
        for e in pack.mods.iter_mut().filter(|e| e.enabled && names.contains(&e.name)) {
            e.enabled = false;
            disabled.push(e.name.clone());
        }
    }
}

/// Problèmes d'un modpack pris seul.
pub fn pack_issues(pack: &Modpack, index: &Index, builtins: &[String], game: &str) -> Vec<Issue> {
    let single = State { packs: vec![pack.clone()], active: vec![pack.id.clone()], ..Default::default() };
    issues(&resolve(&single, &BTreeMap::new()), index, builtins, game)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pack(name: &str, mods: &[(&str, &str)]) -> Modpack {
        let mut p = Modpack::new(name);
        p.id = name.to_string();
        for (n, v) in mods {
            p.set_mod(n, &v.parse().unwrap());
        }
        p
    }

    #[test]
    fn union_and_conflicts() {
        let mut s = State::default();
        s.packs.push(pack("A", &[("flib", "0.15.0"), ("foo", "1.0.0")]));
        s.packs.push(pack("B", &[("flib", "0.16.0"), ("bar", "2.0.0")]));
        s.packs.push(pack("C", &[("baz", "1.0.0")]));
        s.active = vec!["A".into(), "B".into()];

        let r = resolve(&s, &BTreeMap::new());
        assert_eq!(r.mods.len(), 3);
        assert!(!r.mods.contains_key("baz"));
        assert_eq!(r.conflicts.len(), 1);
        assert_eq!(r.conflicts[0].name, "flib");
        assert_eq!(r.conflicts[0].options[0].version, "0.16.0");

        let mut choices = BTreeMap::new();
        choices.insert("flib".to_string(), "0.15.0".to_string());
        let r = resolve(&s, &choices);
        assert!(r.conflicts.is_empty());
        assert_eq!(r.mods["flib"], Version(0, 15, 0));
    }

    fn info(name: &str, ver: &str, fv: &str, deps: &[&str]) -> crate::store::ModInfo {
        crate::store::ModInfo {
            name: name.into(),
            version: ver.parse().unwrap(),
            title: String::new(),
            author: String::new(),
            description: String::new(),
            factorio_version: fv.into(),
            dependencies: deps.iter().map(|d| d.to_string()).collect(),
            homepage: String::new(),
            path: Default::default(),
        }
    }

    #[test]
    fn auto_disable_cascades_and_leaves_incompatibilities() {
        let mut index = Index::new();
        // old : prévu pour 2.0 -> à désactiver ; uses-old en dépend -> désactivé en cascade
        index.insert("old".into(), vec![info("old", "1.0.0", "2.0", &["base"])]);
        index.insert("uses-old".into(), vec![info("uses-old", "1.0.0", "2.1", &["base", "old"])]);
        // a et b s'excluent : au choix de l'utilisateur
        index.insert("a".into(), vec![info("a", "1.0.0", "2.1", &["base", "! b"])]);
        index.insert("b".into(), vec![info("b", "1.0.0", "2.1", &["base", "!a"])]);
        index.insert("ok".into(), vec![info("ok", "1.0.0", "2.1", &["base"])]);
        let mut p = pack("P", &[("old", "1.0.0"), ("uses-old", "1.0.0"), ("a", "1.0.0"), ("b", "1.0.0"), ("ok", "1.0.0")]);
        let builtins = vec!["base".to_string()];

        let before = pack_issues(&p, &index, &builtins, "2.1");
        assert_eq!(before.iter().filter(|i| !i.is_automatic()).count(), 1, "une seule entrée pour la paire a/b");

        let mut off = auto_disable(&mut p, &index, &builtins, "2.1");
        off.sort();
        assert_eq!(off, vec!["old".to_string(), "uses-old".to_string()]);
        let after = pack_issues(&p, &index, &builtins, "2.1");
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].disable, vec!["a".to_string(), "b".to_string()]);
        assert!(p.entry("ok").unwrap().enabled);
    }

    #[test]
    fn disabled_entries_are_ignored() {
        let mut s = State::default();
        let mut a = pack("A", &[("foo", "1.0.0")]);
        a.mods[0].enabled = false;
        s.packs.push(a);
        s.active = vec!["A".into()];
        assert!(resolve(&s, &BTreeMap::new()).mods.is_empty());
    }
}
