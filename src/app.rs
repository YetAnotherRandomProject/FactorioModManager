//! Contrôleur : relie l'interface Slint au cœur (dépôt, portail, modpacks, synchro).

use anyhow::{Context, Result, anyhow};
use slint::{ComponentHandle, Image, Model, ModelRc, SharedString, VecModel, Weak};
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::config::{Config, Paths};
use crate::factorio::{self, ModList};
use crate::modpack::{self, Conflict, Modpack, PackEntry, Resolution, State};
use crate::portal::{self, ModListEntry, Portal, Release};
use crate::store::{self, AddMode, Index, Store};
use crate::sync::{self, SyncPlan};
use crate::transfer;
use crate::updater::{self, AppRelease};
use crate::version::{DepKind, Dependency, Version, is_builtin, same_major_minor};
use crate::{
    AddRow, AppWindow, BuiltinRow, ConflictRow, DepRow, ModRow, PackModRow, PackRow, SaveRow, SearchRow, VersionRow,
};

type Shared = Arc<Mutex<Core>>;

const CATEGORIES: &[(&str, Option<&str>)] = &[
    ("Toutes catégories", None),
    ("Modpacks", Some("mod-packs")),
    ("Contenu", Some("content")),
    ("Refonte", Some("overhaul")),
    ("Ajustements", Some("tweaks")),
    ("Utilitaires", Some("utilities")),
    ("Scénarios", Some("scenarios")),
    ("Traductions", Some("localizations")),
    ("Interne", Some("internal")),
    ("Sans catégorie", Some("no-category")),
];
const VERSIONS: &[&str] = &["Toutes versions", "2.1", "2.0", "1.1", "1.0", "0.18", "0.17"];

// ---------------------------------------------------------------------------
// État du cœur
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct DlItem {
    name: String,
    kind: DepKind,
    constraint: Option<Dependency>,
    release: Option<Release>,
    installed: Option<Version>,
    checked: bool,
    checkable: bool,
    status: String,
}

#[derive(Clone, Debug)]
struct DlSession {
    name: String,
    title: String,
    category: Option<String>,
    releases: Vec<Release>,
    version_index: usize,
    items: Vec<DlItem>,
}

enum After {
    Nothing,
    Sync,
    Launch(Option<PathBuf>),
}

enum Confirm {
    Sync(SyncPlan, Option<PathBuf>),
    DeletePack(String),
    DeleteVersion(String, Version),
}

pub struct Core {
    paths: Paths,
    config: Config,
    state: State,
    store: Store,
    index: Index,
    portal: Arc<Portal>,
    portal_list: Option<Arc<Vec<ModListEntry>>>,
    portal_list_at: Option<Instant>,
    last_check: Option<Instant>,
    checking: bool,
    app_update: Option<AppRelease>,
    updates: BTreeMap<String, Release>,
    builtins: Vec<String>,
    game_version: Option<String>,
    icons: HashMap<String, PathBuf>,
    session_choices: BTreeMap<String, String>,
    selected_mod: Option<String>,
    selected_pack: Option<String>,
    dl: Option<DlSession>,
    dl_gen: u64,
    conflicts: Vec<Conflict>,
    conflict_sel: Vec<usize>,
    after_conflict: After,
    confirm: Option<Confirm>,
}

impl Core {
    fn config_path(&self) -> PathBuf {
        self.paths.data.join("config.json")
    }
    fn state_path(&self) -> PathBuf {
        self.paths.data.join("state.json")
    }
    fn save_state(&self) {
        if let Err(e) = self.state.save(&self.state_path()) {
            eprintln!("sauvegarde de l'état: {e:#}");
        }
    }
    fn save_config(&self) {
        if let Err(e) = self.config.save(&self.config_path()) {
            eprintln!("sauvegarde des paramètres: {e:#}");
        }
    }
    fn credentials(&self) -> Option<(String, String)> {
        if !self.config.username.is_empty() && !self.config.token.is_empty() {
            return Some((self.config.username.clone(), self.config.token.clone()));
        }
        factorio::read_credentials(self.config.factorio_user_dir.as_ref()?)
    }
    fn target_factorio(&self, pack: Option<&Modpack>) -> String {
        if self.config.feat_factorio_version {
            if let Some(v) = pack.and_then(|p| p.factorio_version.clone()).filter(|v| !v.is_empty()) {
                return v;
            }
        }
        if !self.config.target_factorio.is_empty() {
            return self.config.target_factorio.clone();
        }
        self.game_version
            .as_ref()
            .and_then(|v| v.parse::<Version>().ok())
            .map(|v| v.major_minor())
            .unwrap_or_default()
    }
    fn choices(&self) -> BTreeMap<String, String> {
        let mut c = self.state.conflict_choices.clone();
        c.extend(self.session_choices.clone());
        c
    }
    fn resolution(&self) -> Resolution {
        modpack::resolve(&self.state, &self.choices())
    }
    fn settings_source(&self) -> Option<(String, PathBuf)> {
        if !self.config.feat_mod_settings {
            return None;
        }
        self.state
            .active
            .iter()
            .map(|id| (id.clone(), self.paths.pack_settings(id)))
            .find(|(_, p)| p.exists())
    }
    fn refresh_factorio(&mut self) {
        let exe = self.config.factorio_exe.clone();
        self.builtins = factorio::builtin_mods(exe.as_deref());
        self.game_version = factorio::detect_version(exe.as_deref(), self.config.factorio_user_dir.as_deref());
    }
}

// ---------------------------------------------------------------------------
// Utilitaires UI
// ---------------------------------------------------------------------------

static BUSY: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    static IMAGES: RefCell<HashMap<PathBuf, Option<Image>>> = RefCell::new(HashMap::new());
}

fn load_image(path: Option<&PathBuf>) -> Option<Image> {
    let path = path?;
    IMAGES.with(|cache| {
        cache
            .borrow_mut()
            .entry(path.clone())
            .or_insert_with(|| Image::load_from_path(path).ok())
            .clone()
    })
}

fn model<T: Clone + 'static>(v: Vec<T>) -> ModelRc<T> {
    ModelRc::new(VecModel::from(v))
}

fn strings(v: Vec<String>) -> ModelRc<SharedString> {
    model(v.into_iter().map(SharedString::from).collect())
}

/// Titre affichable : certains info.json contiennent des titres de gabarit (« [MOD DISPLAY NAME] »).
fn display_title(title: &str, name: &str) -> String {
    let t = one_line(title);
    if t.is_empty() || (t.starts_with('[') && t.ends_with(']')) { name.to_string() } else { t }
}

/// Texte sur une seule ligne : les retours à la ligne et espaces multiples (fréquents dans les
/// résumés du portail) deviennent un espace, pour ne pas déborder des lignes de hauteur fixe.
fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn letter(s: &str) -> SharedString {
    s.chars().next().map(|c| c.to_uppercase().to_string()).unwrap_or_default().into()
}

fn short(s: &str, max: usize) -> String {
    let line = one_line(s);
    if line.chars().count() > max {
        format!("{}…", line.chars().take(max).collect::<String>())
    } else {
        line
    }
}

fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(' ');
        }
        out.push(c);
    }
    out
}

fn post_status(w: &Weak<AppWindow>, msg: impl Into<String>) {
    let msg = msg.into();
    let _ = w.upgrade_in_event_loop(move |ui| ui.set_status(msg.into()));
}

fn post_progress(w: &Weak<AppWindow>, p: f32) {
    let _ = w.upgrade_in_event_loop(move |ui| ui.set_progress(p));
}

fn post_refresh(w: &Weak<AppWindow>, c: &Shared) {
    let c = c.clone();
    let _ = w.upgrade_in_event_loop(move |ui| {
        let k = c.lock().unwrap();
        refresh_all(&ui, &k);
    });
}

/// Lance une tâche en arrière-plan ; son résultat s'affiche dans la barre d'état.
fn task<F>(ui: &AppWindow, core: &Shared, label: &str, f: F)
where
    F: FnOnce(&Shared, &Weak<AppWindow>) -> Result<String> + Send + 'static,
{
    BUSY.fetch_add(1, Ordering::SeqCst);
    ui.set_busy(true);
    ui.set_status(label.into());
    let c = core.clone();
    let w = ui.as_weak();
    std::thread::spawn(move || {
        let msg = match f(&c, &w) {
            Ok(m) => m,
            Err(e) => format!("Erreur : {e:#}"),
        };
        let c2 = c.clone();
        let _ = w.upgrade_in_event_loop(move |ui| {
            let left = BUSY.fetch_sub(1, Ordering::SeqCst).saturating_sub(1);
            ui.set_busy(left > 0);
            ui.set_progress(-1.0);
            if !msg.is_empty() {
                ui.set_status(msg.into());
            }
            let k = c2.lock().unwrap();
            refresh_all(&ui, &k);
        });
    });
}

// ---------------------------------------------------------------------------
// Rafraîchissement des vues
// ---------------------------------------------------------------------------

fn refresh_all(ui: &AppWindow, k: &Core) {
    refresh_library(ui, k);
    refresh_detail(ui, k);
    refresh_packs(ui, k);
    refresh_pack_detail(ui, k, false);
}

fn portal_summary<'a>(k: &'a Core, name: &str) -> Option<&'a ModListEntry> {
    k.portal_list.as_ref()?.iter().find(|m| m.name == name)
}

fn refresh_library(ui: &AppWindow, k: &Core) {
    let filter = ui.get_library_filter().to_lowercase();
    let mode = ui.get_library_mode();
    let res = k.resolution();
    let mut rows = Vec::new();
    for (name, versions) in &k.index {
        let latest = &versions[0];
        let packs = k.state.packs_using(name, None);
        let update = k.updates.get(name).map(|r| r.version.clone()).unwrap_or_default();
        let active = res.mods.contains_key(name);
        let keep = match mode {
            1 => active,
            2 => packs.is_empty(),
            3 => !update.is_empty(),
            _ => true,
        };
        if !keep {
            continue;
        }
        let title = display_title(&latest.title, name);
        if !filter.is_empty()
            && !name.to_lowercase().contains(&filter)
            && !title.to_lowercase().contains(&filter)
            && !latest.author.to_lowercase().contains(&filter)
        {
            continue;
        }
        let icon = load_image(k.icons.get(name));
        rows.push(ModRow {
            name: name.into(),
            letter: letter(&title),
            title: title.into(),
            summary: short(&latest.description, 160).into(),
            has_icon: icon.is_some(),
            icon: icon.unwrap_or_default(),
            version: latest.version.to_string().into(),
            versions_count: versions.len() as i32,
            update: update.into(),
            packs: packs.join(", ").into(),
            active,
        });
    }
    rows.sort_by_key(|r| r.title.to_lowercase());
    ui.set_library_total(k.index.len() as i32);
    ui.set_library(model(rows));
    ui.set_selected_mod(k.selected_mod.clone().unwrap_or_default().into());
}

fn refresh_detail(ui: &AppWindow, k: &Core) {
    let Some(versions) = k.selected_mod.as_ref().and_then(|n| k.index.get(n)) else {
        ui.set_detail_visible(false);
        return;
    };
    let latest = &versions[0];
    let name = &latest.name;
    let title = display_title(&latest.title, name);
    ui.set_detail_visible(true);
    ui.set_detail_name(name.into());
    ui.set_detail_letter(letter(&title));
    ui.set_detail_title(title.into());
    ui.set_detail_author(latest.author.clone().into());
    ui.set_detail_factorio(latest.factorio_version.clone().into());
    let summary = portal_summary(k, name)
        .map(|m| m.summary.clone())
        .filter(|s| s.trim() != latest.description.trim())
        .unwrap_or_default();
    ui.set_detail_summary(summary.into());
    ui.set_detail_description(latest.description.clone().into());
    ui.set_detail_homepage(latest.homepage.clone().into());
    ui.set_detail_update(k.updates.get(name).map(|r| format!("version {}", r.version)).unwrap_or_default().into());
    let icon = load_image(k.icons.get(name));
    ui.set_detail_has_icon(icon.is_some());
    ui.set_detail_icon(icon.unwrap_or_default());
    let rows: Vec<VersionRow> = versions
        .iter()
        .map(|m| {
            let packs = k.state.packs_using(name, Some(&m.version.to_string()));
            VersionRow {
                version: m.version.to_string().into(),
                factorio: m.factorio_version.clone().into(),
                used: !packs.is_empty(),
                packs: packs.join(", ").into(),
            }
        })
        .collect();
    ui.set_detail_versions(model(rows));
    ui.set_detail_version_names(strings(versions.iter().map(|m| m.version.to_string()).collect()));
    let mut deps: Vec<(i32, String)> = latest
        .deps()
        .iter()
        .map(|d| (d.kind.group(), format!("{} : {} {}", d.kind.label(), d.name, d.constraint_str())))
        .collect();
    deps.sort();
    ui.set_detail_deps(strings(deps.into_iter().map(|(_, s)| s).collect()));
    if ui.get_add_version_index() >= versions.len() as i32 {
        ui.set_add_version_index(0);
    }
}

fn refresh_packs(ui: &AppWindow, k: &Core) {
    let rows: Vec<PackRow> = k
        .state
        .packs
        .iter()
        .map(|p| PackRow {
            id: p.id.clone().into(),
            name: p.name.clone().into(),
            description: p.description.clone().into(),
            active: k.state.is_active(&p.id),
            mods_count: p.mods.len() as i32,
            base_mod: p.base_mod.clone().unwrap_or_default().into(),
            factorio: p.factorio_version.clone().unwrap_or_default().into(),
            has_settings: k.paths.pack_settings(&p.id).exists(),
            saves_count: p.saves.len() as i32,
        })
        .collect();
    ui.set_packs(model(rows));
    ui.set_pack_names(strings(k.state.packs.iter().map(|p| p.name.clone()).collect()));
    if ui.get_add_pack_index() >= k.state.packs.len() as i32 {
        ui.set_add_pack_index(0);
    }
    let res = k.resolution();
    let n = k.state.active.len();
    let mut summary = format!("{n} modpack(s) actif(s)\n{} mod(s) chargé(s)", res.mods.len());
    if !res.conflicts.is_empty() {
        summary.push_str(&format!("\n⚠ {} conflit(s) de version", res.conflicts.len()));
    }
    ui.set_active_summary(summary.into());
    ui.set_app_update(k.app_update.as_ref().map(|r| r.version.to_string()).unwrap_or_default().into());
}

fn pack_mod_versions(index: &Index, e: &PackEntry) -> Vec<String> {
    let mut v: Vec<String> =
        index.get(&e.name).map(|l| l.iter().map(|m| m.version.to_string()).collect()).unwrap_or_default();
    if !v.contains(&e.version) {
        v.push(e.version.clone());
    }
    v
}

fn refresh_pack_detail(ui: &AppWindow, k: &Core, reset_meta: bool) {
    let current = k.selected_pack.clone().unwrap_or_default();
    let reset_meta = reset_meta || ui.get_selected_pack().as_str() != current;
    ui.set_selected_pack(current.into());
    let Some(pack) = k.selected_pack.as_ref().and_then(|id| k.state.pack(id)) else {
        ui.set_pack_visible(false);
        return;
    };
    ui.set_pack_visible(true);
    if reset_meta {
        ui.set_pack_name(pack.name.clone().into());
        ui.set_pack_description(pack.description.clone().into());
        ui.set_pack_factorio(pack.factorio_version.clone().unwrap_or_default().into());
    }
    ui.set_pack_base_mod(pack.base_mod.clone().unwrap_or_default().into());

    let target = pack.factorio_version.clone().unwrap_or_default();
    let mut warning = String::new();
    if let (false, Some(game)) = (target.is_empty(), &k.game_version) {
        if !same_major_minor(&target, game) {
            warning = format!("Ce modpack cible Factorio {target}, la version installée est {game}.");
        }
    }
    ui.set_pack_factorio_warning(warning.into());

    let filter = ui.get_pack_mod_filter().trim().to_lowercase();
    let rows: Vec<PackModRow> = pack
        .mods
        .iter()
        .enumerate()
        .filter_map(|(pos, e)| {
            let ver: Version = e.version.parse().unwrap_or_default();
            let info = store::find(&k.index, &e.name, &ver);
            let title = display_title(info.map(|i| i.title.as_str()).unwrap_or(""), &e.name);
            if !filter.is_empty() && !e.name.to_lowercase().contains(&filter) && !title.to_lowercase().contains(&filter) {
                return None;
            }
            let versions = pack_mod_versions(&k.index, e);
            let idx = versions.iter().position(|v| v == &e.version).unwrap_or(0);
            let icon = load_image(k.icons.get(&e.name));
            Some(PackModRow {
                // Position dans le modpack (la liste affichée peut être filtrée).
                index: pos as i32,
                name: e.name.clone().into(),
                letter: letter(&title),
                title: title.into(),
                has_icon: icon.is_some(),
                icon: icon.unwrap_or_default(),
                versions: strings(versions),
                version_index: idx as i32,
                enabled: e.enabled,
                missing: info.is_none(),
            })
        })
        .collect();
    ui.set_pack_mods_total(pack.mods.len() as i32);
    ui.set_pack_mods(model(rows));

    let builtins: Vec<BuiltinRow> = k
        .builtins
        .iter()
        .map(|b| BuiltinRow { name: b.clone().into(), enabled: !pack.disabled_builtins.contains(b) })
        .collect();
    ui.set_pack_builtins(model(builtins));

    let saves: Vec<SaveRow> = pack
        .saves
        .iter()
        .map(|p| SaveRow {
            name: p.file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_default().into(),
            path: p.to_string_lossy().to_string().into(),
        })
        .collect();
    ui.set_pack_saves(model(saves));

    let settings = k.paths.pack_settings(&pack.id);
    let has = settings.exists();
    ui.set_pack_has_settings(has);
    let info = if has {
        let size = fs::metadata(&settings).map(|m| m.len()).unwrap_or(0);
        format!("Réglages enregistrés ({size} octets). Ils seront appliqués à l'activation et au lancement.")
    } else {
        "Aucun réglage enregistré : Factorio utilisera les réglages déjà présents.".to_string()
    };
    ui.set_pack_settings_info(info.into());

    let single = State { packs: vec![pack.clone()], active: vec![pack.id.clone()], ..Default::default() };
    let res = modpack::resolve(&single, &BTreeMap::new());
    let mut warnings = modpack::check(&res, &k.index, &k.builtins);
    if k.config.feat_factorio_version && !target.is_empty() {
        for e in &pack.mods {
            let ver: Version = e.version.parse().unwrap_or_default();
            if let Some(i) = store::find(&k.index, &e.name, &ver) {
                if !i.factorio_version.is_empty() && !same_major_minor(&i.factorio_version, &target) {
                    warnings.push(format!("{} {} est prévu pour Factorio {}", e.name, e.version, i.factorio_version));
                }
            }
        }
    }
    ui.set_pack_warnings(strings(warnings));
}

fn refresh_dl(ui: &AppWindow, k: &Core) {
    let Some(dl) = &k.dl else {
        ui.set_dl_visible(false);
        return;
    };
    ui.set_dl_title(dl.title.clone().into());
    ui.set_dl_name(dl.name.clone().into());
    ui.set_dl_versions(strings(
        dl.releases.iter().map(|r| format!("{}  (Factorio {})", r.version, r.factorio())).collect(),
    ));
    ui.set_dl_version_index(dl.version_index as i32);
    let rows: Vec<DepRow> = dl
        .items
        .iter()
        .map(|i| DepRow {
            name: i.name.clone().into(),
            kind: i.kind.label().into(),
            group: i.kind.group(),
            constraint: i.constraint.as_ref().map(|d| d.constraint_str()).unwrap_or_default().into(),
            status: i.status.clone().into(),
            checked: i.checked,
            checkable: i.checkable,
        })
        .collect();
    let target = k.target_factorio(None);
    let rel = dl.releases.get(dl.version_index);
    let mut info = format!(
        "{} dépendance(s) obligatoire(s), {} recommandée(s) ou optionnelle(s).",
        dl.items.iter().filter(|i| i.kind.is_required()).count(),
        dl.items.iter().filter(|i| i.kind.is_optional_like()).count()
    );
    if let Some(r) = rel {
        if !target.is_empty() && !same_major_minor(&r.factorio(), &target) {
            info.push_str(&format!("  ⚠ Cette version est prévue pour Factorio {} (cible : {target}).", r.factorio()));
        }
    }
    ui.set_dl_info(info.into());
    ui.set_dl_deps(model(rows));
    let mut targets = vec!["Aucun (dépôt uniquement)".to_string()];
    targets.extend(k.state.packs.iter().map(|p| p.name.clone()));
    ui.set_dl_targets(strings(targets));
    ui.set_dl_is_modpack(portal::is_modpack_category(dl.category.as_deref()));
}

fn refresh_settings(ui: &AppWindow, k: &Core) {
    let c = &k.config;
    ui.set_s_user_dir(c.factorio_user_dir.as_ref().map(|p| p.display().to_string()).unwrap_or_default().into());
    ui.set_s_exe(c.factorio_exe.as_ref().map(|p| p.display().to_string()).unwrap_or_default().into());
    ui.set_s_username(c.username.clone().into());
    ui.set_s_token(c.token.clone().into());
    ui.set_s_target(c.target_factorio.clone().into());
    ui.set_s_feat_settings(c.feat_mod_settings);
    ui.set_s_feat_version(c.feat_factorio_version);
    ui.set_s_feat_export(c.feat_export);
    ui.set_s_feat_saves(c.feat_saves);
    ui.set_s_auto_updates(c.auto_check_updates);
    ui.set_s_auto_sync(c.auto_sync_on_activate);
    ui.set_s_export_mods(c.export_include_mods);
    ui.set_s_copy_back(c.copy_back_settings);
    ui.set_s_interval(c.check_interval_minutes.to_string().into());
    ui.set_s_auto_app(c.auto_update_app);
    ui.set_app_version(format!("v{}", env!("CARGO_PKG_VERSION")).into());
    ui.set_s_data_dir(k.paths.data.display().to_string().into());
    let detected = match &k.game_version {
        Some(v) => format!("Factorio {v} détecté • intégrés : {}", k.builtins.join(", ")),
        None => "Version du jeu non détectée".to_string(),
    };
    ui.set_s_detected(detected.into());
    let creds = if !c.username.is_empty() && !c.token.is_empty() {
        format!("Identifiants manuels : {}", c.username)
    } else {
        match c.factorio_user_dir.as_ref().and_then(|d| factorio::read_credentials(d)) {
            Some((u, _)) => format!("Connecté en tant que « {u} » (lu depuis player-data.json)."),
            None => "Aucun identifiant trouvé dans player-data.json : connecte-toi au portail dans le jeu, ou saisis-les ci-dessous.".into(),
        }
    };
    ui.set_s_credentials(creds.into());
}

// ---------------------------------------------------------------------------
// Opérations de fond
// ---------------------------------------------------------------------------

/// Ajoute une version d'un mod au modpack, avec ses dépendances obligatoires déjà présentes
/// dans le dépôt. Renvoie le nombre de dépendances ajoutées.
fn add_mod_to_pack(k: &mut Core, pack_id: &str, info: &store::ModInfo) -> usize {
    let builtins = k.builtins.clone();
    let index = k.index.clone();
    let Some(pack) = k.state.pack_mut(pack_id) else { return 0 };
    pack.set_mod(&info.name, &info.version);
    let mut added = 0;
    let mut stack = info.deps();
    let mut seen = HashSet::new();
    while let Some(dep) = stack.pop() {
        if !dep.kind.is_required() || is_builtin(&dep.name, &builtins) || !seen.insert(dep.name.clone()) {
            continue;
        }
        if pack.entry(&dep.name).is_some() {
            continue;
        }
        if let Some(m) = index.get(&dep.name).and_then(|v| v.iter().find(|m| dep.matches(&m.version))) {
            pack.set_mod(&m.name, &m.version);
            stack.extend(m.deps());
            added += 1;
        }
    }
    added
}

/// Résultats de la recherche « ajouter un mod » de la vue modpack : dépôt d'abord, puis portail.
fn pack_add_rows(k: &Core, query: &str) -> Vec<AddRow> {
    let q = query.trim().to_lowercase();
    if q.chars().count() < 2 {
        return vec![];
    }
    let pack = k.selected_pack.as_ref().and_then(|id| k.state.pack(id));
    let in_pack = |n: &str| pack.map(|p| p.entry(n).is_some()).unwrap_or(false);
    let mut rows = Vec::new();
    for (name, versions) in &k.index {
        if rows.len() >= 8 {
            break;
        }
        let title = display_title(&versions[0].title, name);
        if in_pack(name) || !(name.to_lowercase().contains(&q) || title.to_lowercase().contains(&q)) {
            continue;
        }
        rows.push(AddRow {
            name: name.into(),
            title: title.into(),
            info: format!("Dans le dépôt  •  {}", versions[0].version).into(),
            installed: true,
        });
    }
    if let Some(list) = &k.portal_list {
        let target = k.target_factorio(pack);
        let filter = (!target.is_empty()).then_some(target.as_str());
        for m in portal::search(list, &q, None, filter, 30) {
            if rows.len() >= 16 {
                break;
            }
            if k.index.contains_key(&m.name) || in_pack(&m.name) {
                continue;
            }
            let ver = m.latest_release.as_ref().map(|r| r.version.clone()).unwrap_or_default();
            rows.push(AddRow {
                name: m.name.clone().into(),
                title: display_title(&m.title, &m.name).into(),
                info: format!("Portail  •  par {}  •  {ver}", one_line(&m.owner)).into(),
                installed: false,
            });
        }
    }
    rows
}

fn rescan(c: &Shared) {
    let store = c.lock().unwrap().store.clone();
    let index = store.scan();
    let mut icons = HashMap::new();
    for (name, versions) in &index {
        if let Some(p) = versions.iter().find_map(|v| store.icon(v)) {
            icons.insert(name.clone(), p);
        }
    }
    let mut k = c.lock().unwrap();
    for (n, p) in k.icons.drain() {
        icons.entry(n).or_insert(p);
    }
    k.icons = icons;
    k.index = index;
    if let Some(sel) = k.selected_mod.clone() {
        if !k.index.contains_key(&sel) {
            k.selected_mod = None;
        }
    }
}

/// Miniatures du portail pour les mods sans thumbnail.png.
fn fetch_missing_icons(c: &Shared) {
    let (portal, names) = {
        let k = c.lock().unwrap();
        let names: Vec<String> = k.index.keys().filter(|n| !k.icons.contains_key(*n)).cloned().collect();
        (k.portal.clone(), names)
    };
    let found: Vec<(String, PathBuf)> = parallel_map(&names, 6, |n| portal.thumbnail(n).map(|p| (n.clone(), p)));
    let mut k = c.lock().unwrap();
    for (n, p) in found {
        k.icons.insert(n, p);
    }
}

fn parallel_map<T: Sync, R: Send>(items: &[T], threads: usize, f: impl Fn(&T) -> Option<R> + Sync) -> Vec<R> {
    if items.is_empty() {
        return vec![];
    }
    let chunk = items.len().div_ceil(threads.max(1));
    std::thread::scope(|s| {
        let handles: Vec<_> = items
            .chunks(chunk)
            .map(|part| {
                let f = &f;
                s.spawn(move || part.iter().filter_map(f).collect::<Vec<R>>())
            })
            .collect();
        handles.into_iter().flat_map(|h| h.join().unwrap_or_default()).collect()
    })
}

fn check_updates(c: &Shared) -> Result<String> {
    let (portal, names, target, local) = {
        let k = c.lock().unwrap();
        let names: Vec<String> = k.index.keys().filter(|n| !is_builtin(n, &k.builtins)).cloned().collect();
        let local: HashMap<String, (Version, String)> = k
            .index
            .iter()
            .map(|(n, v)| (n.clone(), (v[0].version, v[0].factorio_version.clone())))
            .collect();
        (k.portal.clone(), names, k.target_factorio(None), local)
    };
    let releases = portal.releases_for(&names)?;
    let mut updates = BTreeMap::new();
    for (name, rels) in releases {
        let Some((cur, cur_fv)) = local.get(&name) else { continue };
        let best = rels
            .into_iter()
            .filter(|r| same_major_minor(&r.factorio(), &target) || same_major_minor(&r.factorio(), cur_fv))
            .max_by_key(|r| r.ver());
        if let Some(r) = best.filter(|r| r.ver() > *cur) {
            updates.insert(name, r);
        }
    }
    let n = updates.len();
    c.lock().unwrap().updates = updates;
    Ok(if n == 0 { "Tous les mods sont à jour.".into() } else { format!("{n} mise(s) à jour disponible(s).") })
}

/// Index du portail en mémoire ; `force` le retélécharge, sinon le cache disque est utilisé
/// tant qu'il a moins que l'intervalle de vérification.
fn ensure_portal_list(c: &Shared, force: bool) -> Result<Arc<Vec<ModListEntry>>> {
    let (portal, cached, max_age) = {
        let k = c.lock().unwrap();
        (k.portal.clone(), k.portal_list.clone(), k.config.check_interval())
    };
    if let (Some(l), false) = (cached, force) {
        return Ok(l);
    }
    let list = Arc::new(portal.list_all(if force { Duration::ZERO } else { max_age })?);
    let mut k = c.lock().unwrap();
    k.portal_list = Some(list.clone());
    k.portal_list_at = Some(Instant::now());
    Ok(list)
}

fn portal_list_stale(k: &Core) -> bool {
    k.portal_list_at.map(|t| t.elapsed() > k.config.check_interval()).unwrap_or(true)
}

/// Vérification périodique : mises à jour des mods, index du portail, nouvelle version de l'app.
fn periodic_check(c: &Shared, w: &Weak<AppWindow>) -> Result<String> {
    let (mods_auto, app_auto, stale, before, has_mods) = {
        let mut k = c.lock().unwrap();
        k.checking = true;
        k.last_check = Some(Instant::now());
        (
            k.config.auto_check_updates,
            k.config.auto_update_app,
            portal_list_stale(&k),
            k.updates.len(),
            !k.index.is_empty(),
        )
    };
    let mut msg = String::new();
    if mods_auto && has_mods {
        post_status(w, "Vérification des mises à jour des mods…");
        msg = check_updates(c).unwrap_or_else(|e| format!("Mises à jour : {e:#}"));
    }
    if stale {
        if let Err(e) = ensure_portal_list(c, true) {
            eprintln!("index du portail: {e:#}");
        }
    }
    if app_auto {
        match updater::check() {
            Ok(Some(rel)) => {
                msg = format!("Nouvelle version de l'application disponible : {}", rel.version);
                c.lock().unwrap().app_update = Some(rel);
            }
            Ok(None) => {}
            Err(e) => eprintln!("mise à jour de l'app: {e:#}"),
        }
    }
    let mut k = c.lock().unwrap();
    k.checking = false;
    if k.updates.len() > before {
        msg = format!("{} nouvelle(s) mise(s) à jour de mods disponible(s).", k.updates.len() - before);
    }
    Ok(msg)
}

/// Fermeture transitive des dépendances obligatoires d'une release.
fn required_closure(
    portal: &Portal,
    index: &Index,
    builtins: &[String],
    target: &str,
    root: &str,
    deps: Vec<Dependency>,
    seen: &mut HashSet<String>,
) -> Vec<DlItem> {
    let mut items = Vec::new();
    let mut frontier: Vec<(Dependency, String)> =
        deps.into_iter().filter(|d| d.kind.is_required()).map(|d| (d, root.to_string())).collect();
    while !frontier.is_empty() {
        let to_fetch: Vec<String> = frontier
            .iter()
            .filter(|(d, _)| !is_builtin(&d.name, builtins) && !seen.contains(&d.name))
            .filter(|(d, _)| !index.get(&d.name).map(|v| v.iter().any(|m| d.matches(&m.version))).unwrap_or(false))
            .map(|(d, _)| d.name.clone())
            .collect();
        parallel_map(&to_fetch, 8, |n| portal.full(n).ok().map(|_| ()));
        let mut next = Vec::new();
        for (dep, parent) in frontier {
            if is_builtin(&dep.name, builtins) || !seen.insert(dep.name.clone()) {
                continue;
            }
            let installed = index.get(&dep.name).and_then(|v| v.iter().find(|m| dep.matches(&m.version)));
            let mut item = DlItem {
                name: dep.name.clone(),
                kind: dep.kind,
                constraint: Some(dep.clone()),
                release: None,
                installed: None,
                checked: true,
                checkable: true,
                status: String::new(),
            };
            if let Some(info) = installed {
                item.installed = Some(info.version);
                item.status = format!("Déjà dans le dépôt ({}) • requis par {parent}", info.version);
                next.extend(info.deps().into_iter().filter(|d| d.kind.is_required()).map(|d| (d, dep.name.clone())));
            } else {
                match portal.full(&dep.name) {
                    Ok(full) => match full.best_release(target, Some(&dep)) {
                        Some(r) => {
                            item.status =
                                format!("À télécharger : {} (Factorio {}) • requis par {parent}", r.version, r.factorio());
                            next.extend(r.deps().into_iter().filter(|d| d.kind.is_required()).map(|d| (d, dep.name.clone())));
                            item.release = Some(r);
                        }
                        None => {
                            item.checked = false;
                            item.checkable = false;
                            item.status = format!("⚠ Aucune version compatible sur le portail • requis par {parent}");
                        }
                    },
                    Err(e) => {
                        item.checked = false;
                        item.checkable = false;
                        item.status = format!("⚠ Introuvable sur le portail ({e}) • requis par {parent}");
                    }
                }
            }
            items.push(item);
        }
        frontier = next;
    }
    items
}

fn build_items(portal: &Portal, index: &Index, builtins: &[String], target: &str, name: &str, rel: &Release) -> Vec<DlItem> {
    let mut seen = HashSet::from([name.to_string()]);
    let deps = rel.deps();
    let mut items = required_closure(portal, index, builtins, target, name, deps.clone(), &mut seen);
    for dep in deps.iter().filter(|d| d.kind.is_optional_like()) {
        if is_builtin(&dep.name, builtins) || !seen.insert(dep.name.clone()) {
            continue;
        }
        let installed = index.get(&dep.name).and_then(|v| v.iter().find(|m| dep.matches(&m.version)));
        items.push(DlItem {
            name: dep.name.clone(),
            kind: dep.kind,
            constraint: Some(dep.clone()),
            release: None,
            installed: installed.map(|i| i.version),
            checked: false,
            checkable: true,
            status: match installed {
                Some(i) => format!("Déjà dans le dépôt ({})", i.version),
                None => "Sera téléchargé avec ses dépendances obligatoires".into(),
            },
        });
    }
    // Les mods incompatibles (« ! ») ne sont pas proposés : ce ne sont pas des dépendances à ajouter.
    // Les conflits éventuels restent signalés dans les problèmes détectés du modpack.
    items.sort_by_key(|i| (i.kind.group(), i.name.to_lowercase()));
    items
}

fn load_dl_items(c: &Shared, w: &Weak<AppWindow>, version_index: usize) {
    let (portal, index, builtins, target, dl) = {
        let k = c.lock().unwrap();
        let target = k.target_factorio(k.selected_pack.as_ref().and_then(|id| k.state.pack(id)));
        (k.portal.clone(), k.index.clone(), k.builtins.clone(), target, k.dl.clone())
    };
    let Some(dl) = dl else { return };
    let Some(rel) = dl.releases.get(version_index).cloned() else { return };
    let items = build_items(&portal, &index, &builtins, &target, &dl.name, &rel);
    let c2 = c.clone();
    let _ = w.upgrade_in_event_loop(move |ui| {
        let mut k = c2.lock().unwrap();
        if let Some(d) = k.dl.as_mut().filter(|d| d.name == dl.name) {
            d.items = items;
            d.version_index = version_index;
        }
        ui.set_dl_loading(false);
        ui.set_dl_req_all(true);
        ui.set_dl_rec_all(false);
        ui.set_dl_opt_all(false);
        ui.set_dl_hidden_all(false);
        refresh_dl(&ui, &k);
    });
}

/// Modpack proposé par défaut dans la fenêtre de téléchargement / dépendances.
enum DlTarget {
    /// Le modpack sélectionné dans la vue Modpacks (recherche, ajout depuis un pack).
    SelectedPack,
    /// Aucun : uniquement le dépôt (fiche de la bibliothèque).
    NoPack,
    /// Un modpack précis (ouverture depuis une ligne du modpack).
    Pack(String),
}

/// Ouvre la sélection des dépendances d'un mod : dans une fenêtre (`overlay`) ou intégrée à la fiche
/// de la bibliothèque.
fn open_download(ui: &AppWindow, core: &Shared, name: String, preset: Option<String>, target: DlTarget, overlay: bool) {
    let generation = {
        let mut k = core.lock().unwrap();
        k.dl_gen += 1;
        k.dl = None;
        let id = match target {
            DlTarget::SelectedPack => k.selected_pack.clone(),
            DlTarget::NoPack => None,
            DlTarget::Pack(id) => Some(id),
        };
        let idx = id.and_then(|id| k.state.packs.iter().position(|p| p.id == id));
        ui.set_dl_target_index(idx.map(|i| i as i32 + 1).unwrap_or(0));
        ui.set_dl_installed(k.index.contains_key(&name));
        k.dl_gen
    };
    ui.set_dl_name(name.clone().into());
    ui.set_dl_visible(overlay);
    ui.set_dl_loading(true);
    ui.set_dl_title(name.clone().into());
    ui.set_dl_info("Chargement des informations du portail…".into());
    ui.set_dl_versions(strings(vec![]));
    ui.set_dl_deps(model(vec![]));
    ui.set_dl_is_modpack(false);
    ui.set_dl_make_pack(false);
    let c = core.clone();
    let w = ui.as_weak();
    std::thread::spawn(move || {
        let (portal, target) = {
            let k = c.lock().unwrap();
            (k.portal.clone(), k.target_factorio(k.selected_pack.as_ref().and_then(|id| k.state.pack(id))))
        };
        let full = match portal.full(&name) {
            Ok(f) => f,
            Err(e) => {
                let msg = format!("Erreur : {e:#}");
                let _ = w.upgrade_in_event_loop(move |ui| {
                    ui.set_dl_loading(false);
                    ui.set_dl_info(msg.into());
                });
                return;
            }
        };
        let releases = full.sorted_releases();
        let idx = preset
            .and_then(|v| releases.iter().position(|r| r.version == v))
            .or_else(|| {
                let best = full.best_release(&target, None)?;
                releases.iter().position(|r| r.version == best.version)
            })
            .unwrap_or(0);
        let is_pack = portal::is_modpack_category(full.category.as_deref());
        let title = display_title(&full.title, &full.name);
        {
            let mut k = c.lock().unwrap();
            if k.dl_gen != generation {
                return; // un autre mod a été sélectionné entre-temps
            }
            k.dl = Some(DlSession {
                name: full.name.clone(),
                title: title.clone(),
                category: full.category.clone(),
                releases,
                version_index: idx,
                items: vec![],
            });
        }
        let c2 = c.clone();
        let _ = w.upgrade_in_event_loop(move |ui| {
            let k = c2.lock().unwrap();
            refresh_dl(&ui, &k);
            ui.set_dl_loading(true);
            ui.set_dl_make_pack(is_pack);
            ui.set_dl_pack_name(title.into());
        });
        load_dl_items(&c, &w, idx);
    });
}

/// Télécharge les mods choisis et met à jour le modpack ciblé.
fn confirm_download(c: &Shared, w: &Weak<AppWindow>, target_pack: Option<String>, new_pack: Option<String>) -> Result<String> {
    let (portal, index, builtins, target, dl, creds, downloads, store) = {
        let k = c.lock().unwrap();
        let pack = target_pack.as_ref().and_then(|id| k.state.pack(id));
        (
            k.portal.clone(),
            k.index.clone(),
            k.builtins.clone(),
            k.target_factorio(pack),
            k.dl.clone().ok_or_else(|| anyhow!("aucun téléchargement en cours"))?,
            k.credentials(),
            k.paths.downloads.clone(),
            k.store.clone(),
        )
    };
    let main = dl.releases.get(dl.version_index).cloned().ok_or_else(|| anyhow!("version introuvable"))?;

    // Liste finale : (nom, version, release à télécharger éventuelle)
    let mut wanted: Vec<(String, Version, Option<Release>)> = Vec::new();
    let main_installed = store::find(&index, &dl.name, &main.ver()).is_some();
    wanted.push((dl.name.clone(), main.ver(), if main_installed { None } else { Some(main.clone()) }));
    let mut seen: HashSet<String> = HashSet::from([dl.name.clone()]);
    for item in dl.items.iter().filter(|i| i.checked && i.kind.is_required()) {
        seen.insert(item.name.clone());
        if let Some(v) = item.installed {
            wanted.push((item.name.clone(), v, None));
        } else if let Some(r) = &item.release {
            wanted.push((item.name.clone(), r.ver(), Some(r.clone())));
        }
    }
    for item in dl.items.iter().filter(|i| i.checked && !i.kind.is_required() && i.kind != DepKind::Incompatible) {
        if !seen.insert(item.name.clone()) {
            continue;
        }
        if let Some(v) = item.installed {
            wanted.push((item.name.clone(), v, None));
            continue;
        }
        let full = portal.full(&item.name)?;
        let rel = full
            .best_release(&target, item.constraint.as_ref())
            .ok_or_else(|| anyhow!("aucune version compatible pour {}", item.name))?;
        let extra = required_closure(&portal, &index, &builtins, &target, &item.name, rel.deps(), &mut seen);
        wanted.push((item.name.clone(), rel.ver(), Some(rel)));
        for e in extra {
            if let Some(v) = e.installed {
                wanted.push((e.name, v, None));
            } else if let Some(r) = e.release {
                wanted.push((e.name, r.ver(), Some(r)));
            }
        }
    }

    let to_download: Vec<&Release> = wanted.iter().filter_map(|(_, _, r)| r.as_ref()).collect();
    if !to_download.is_empty() {
        let (user, token) = creds.ok_or_else(|| {
            anyhow!("identifiants du portail introuvables : connecte-toi au portail dans Factorio ou renseigne-les dans Paramètres")
        })?;
        let n = to_download.len();
        for (i, rel) in to_download.iter().enumerate() {
            post_status(w, format!("Téléchargement {}/{n} : {}", i + 1, rel.file_name));
            let w2 = w.clone();
            let path = portal.download(rel, &user, &token, &downloads, |done, total| {
                if total > 0 {
                    let overall = (i as f32 + done as f32 / total as f32) / n as f32;
                    post_progress(&w2, overall);
                }
            })?;
            store.add_file(&path, AddMode::Move)?;
        }
    }
    rescan(c);

    let mut k = c.lock().unwrap();
    let mut msg = format!("{} mod(s) téléchargé(s)", to_download.len());
    let pack_id = if let Some(name) = new_pack {
        let mut p = Modpack::new(&name);
        p.base_mod = Some(dl.name.clone());
        if k.config.feat_factorio_version {
            let f = main.factorio();
            p.factorio_version = if f.is_empty() { None } else { Some(f) };
        }
        let id = p.id.clone();
        k.state.packs.push(p);
        msg.push_str(&format!(", modpack « {name} » créé"));
        Some(id)
    } else {
        target_pack
    };
    if let Some(id) = pack_id {
        if let Some(p) = k.state.pack_mut(&id) {
            for (n, v, _) in &wanted {
                p.set_mod(n, v);
            }
            if !msg.contains("créé") {
                msg.push_str(&format!(", ajouté(s) à « {} »", p.name));
            }
        }
        k.selected_pack = Some(id);
    }
    let core = &mut *k;
    let index = &core.index;
    core.updates.retain(|name, r| index.get(name).map(|v| r.ver() > v[0].version).unwrap_or(true));
    k.dl = None;
    k.save_state();
    Ok(msg + ".")
}

fn import_existing(c: &Shared, w: &Weak<AppWindow>) -> Result<String> {
    let (mods_dir, store, builtins) = {
        let k = c.lock().unwrap();
        (k.config.mods_dir().ok_or_else(|| anyhow!("dossier Factorio non configuré"))?, k.store.clone(), k.builtins.clone())
    };
    let zips: Vec<PathBuf> = fs::read_dir(&mods_dir)
        .with_context(|| format!("lecture de {}", mods_dir.display()))?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("zip"))
        .collect();
    let n = zips.len();
    let mut imported: Vec<store::ModInfo> = Vec::new();
    let mut errors = 0;
    for (i, z) in zips.iter().enumerate() {
        post_progress(w, i as f32 / n.max(1) as f32);
        match store.add_file(z, AddMode::LinkOrCopy) {
            Ok(info) => imported.push(info),
            Err(e) => {
                errors += 1;
                eprintln!("import: {e:#}");
            }
        }
    }
    let list = ModList::read(&mods_dir);
    let mut pack = Modpack::new("Mods actuels (import)");
    pack.description = "Mods activés dans Factorio au moment de l'import".into();
    for b in &builtins {
        if list.mods.iter().any(|m| &m.name == b && !m.enabled) {
            pack.disabled_builtins.push(b.clone());
        }
    }
    let mut best: BTreeMap<String, Version> = BTreeMap::new();
    for info in &imported {
        let enabled = list.mods.is_empty() || list.is_enabled(&info.name);
        if !enabled {
            continue;
        }
        let pinned = list
            .mods
            .iter()
            .find(|m| m.name == info.name)
            .and_then(|m| m.version.as_ref())
            .and_then(|v| v.parse::<Version>().ok());
        let e = best.entry(info.name.clone()).or_insert(info.version);
        match pinned {
            Some(p) => *e = p,
            None if info.version > *e => *e = info.version,
            _ => {}
        }
    }
    for (name, v) in &best {
        pack.set_mod(name, v);
    }
    rescan(c);
    let mut k = c.lock().unwrap();
    let pack_name = pack.name.clone();
    let id = pack.id.clone();
    k.state.packs.push(pack);
    if k.state.active.is_empty() {
        k.state.active.push(id.clone());
    }
    k.selected_pack = Some(id);
    k.save_state();
    Ok(format!(
        "{} zip(s) importé(s) dans le dépôt{}, modpack « {pack_name} » créé avec {} mod(s) actif(s).",
        imported.len(),
        if errors > 0 { format!(" ({errors} erreur(s))") } else { String::new() },
        best.len()
    ))
}

// ---------------------------------------------------------------------------
// Activation, conflits, synchro, lancement
// ---------------------------------------------------------------------------

fn show_conflicts(ui: &AppWindow, k: &mut Core, conflicts: Vec<Conflict>, after: After) {
    let rows: Vec<ConflictRow> = conflicts
        .iter()
        .map(|c| ConflictRow {
            name: c.name.clone().into(),
            labels: strings(c.options.iter().map(|o| format!("{}  —  {}", o.version, o.packs.join(", "))).collect()),
            selected: 0,
        })
        .collect();
    k.conflict_sel = vec![0; conflicts.len()];
    k.conflicts = conflicts;
    k.after_conflict = after;
    ui.set_conflicts(model(rows));
    ui.set_conflict_visible(true);
}

/// Vérifie les conflits puis exécute l'action demandée.
fn proceed(ui: &AppWindow, core: &Shared, after: After) {
    let mut k = core.lock().unwrap();
    let res = k.resolution();
    if !res.conflicts.is_empty() {
        show_conflicts(ui, &mut k, res.conflicts, after);
        return;
    }
    drop(k);
    match after {
        After::Nothing => {}
        After::Sync => start_sync(ui, core),
        After::Launch(save) => start_launch(ui, core, save),
    }
}

fn start_sync(ui: &AppWindow, core: &Shared) {
    let mut k = core.lock().unwrap();
    let Some(mods_dir) = k.config.mods_dir() else {
        ui.set_status("Configure le dossier de Factorio dans Paramètres.".into());
        return;
    };
    let res = k.resolution();
    let plan = sync::plan(&mods_dir, &res, &k.index, &k.builtins);
    let settings = k.settings_source().map(|(_, p)| p);
    let mut text = plan.summary();
    if let Some((id, _)) = k.settings_source() {
        let name = k.state.pack(&id).map(|p| p.name.clone()).unwrap_or_default();
        text.push_str(&format!("\nRéglages des mods : ceux du modpack « {name} ».\n"));
    }
    text.push_str(&format!("\nDossier : {}", mods_dir.display()));
    ui.set_confirm_title("Synchroniser le dossier mods/ de Factorio ?".into());
    ui.set_confirm_text(text.into());
    ui.set_confirm_ok("Synchroniser".into());
    ui.set_confirm_visible(true);
    k.confirm = Some(Confirm::Sync(plan, settings));
}

fn start_launch(ui: &AppWindow, core: &Shared, save: Option<PathBuf>) {
    task(ui, core, "Préparation du profil de lancement…", move |c, w| {
        let (exe, profile, res, index, builtins, settings, copy_back, single) = {
            let k = c.lock().unwrap();
            let exe = k.config.factorio_exe.clone().ok_or_else(|| anyhow!("exécutable de Factorio non configuré"))?;
            let settings = k.settings_source();
            let single = if k.state.active.len() == 1 { k.state.active.first().cloned() } else { None };
            (
                exe,
                k.paths.profile.clone(),
                k.resolution(),
                k.index.clone(),
                k.builtins.clone(),
                settings,
                k.config.copy_back_settings && k.config.feat_mod_settings,
                single,
            )
        };
        let missing = sync::build_profile(&profile, &res, &index, &builtins, settings.as_ref().map(|(_, p)| p.as_path()))?;
        let mut child = factorio::launch(&exe, Some(&profile), save.as_deref())?;
        let mut msg = format!("Factorio lancé avec {} mod(s).", res.mods.len());
        if !missing.is_empty() {
            msg.push_str(&format!(" Absents du dépôt : {}", missing.join(", ")));
        }
        post_status(w, msg.clone());
        let target = settings.map(|(id, _)| id).or(single);
        let c2 = c.clone();
        let w2 = w.clone();
        std::thread::spawn(move || {
            let _ = child.wait();
            if !copy_back {
                return;
            }
            let Some(id) = target else { return };
            let src = profile.join("mod-settings.dat");
            if !src.exists() {
                return;
            }
            let (dst, name) = {
                let k = c2.lock().unwrap();
                (k.paths.pack_settings(&id), k.state.pack(&id).map(|p| p.name.clone()).unwrap_or_default())
            };
            if let Some(d) = dst.parent() {
                let _ = fs::create_dir_all(d);
            }
            if fs::copy(&src, &dst).is_ok() {
                post_status(&w2, format!("Partie terminée : réglages enregistrés dans « {name} »."));
                post_refresh(&w2, &c2);
            }
        });
        Ok(msg)
    });
}

fn toggle_pack(ui: &AppWindow, core: &Shared, id: &str, on: bool) {
    let auto_sync = {
        let mut k = core.lock().unwrap();
        k.state.active.retain(|a| a != id);
        if on {
            k.state.active.push(id.to_string());
        }
        k.save_state();
        refresh_all(ui, &k);
        k.config.auto_sync_on_activate
    };
    let res = core.lock().unwrap().resolution();
    if !res.conflicts.is_empty() {
        let mut k = core.lock().unwrap();
        show_conflicts(ui, &mut k, res.conflicts, if auto_sync { After::Sync } else { After::Nothing });
    } else if auto_sync {
        start_sync(ui, core);
    }
}

// ---------------------------------------------------------------------------
// Démarrage
// ---------------------------------------------------------------------------

pub fn run() -> Result<()> {
    let paths = Paths::new()?;
    let mut config = Config::load(&paths.data.join("config.json"));
    if config.factorio_user_dir.is_none() {
        config.factorio_user_dir = factorio::default_user_dir().filter(|d| d.exists());
    }
    if config.factorio_exe.is_none() {
        config.factorio_exe = factorio::detect_exe(config.factorio_user_dir.as_deref());
    }
    let state = State::load(&paths.data.join("state.json"));
    let store = Store::new(paths.store.clone(), paths.cache.join("icons-128"))?;
    let portal = Arc::new(Portal::new(paths.cache.clone())?);
    let mut core = Core {
        paths,
        config,
        state,
        store,
        index: Index::new(),
        portal,
        portal_list: None,
        portal_list_at: None,
        last_check: None,
        checking: false,
        app_update: None,
        updates: BTreeMap::new(),
        builtins: vec![],
        game_version: None,
        icons: HashMap::new(),
        session_choices: BTreeMap::new(),
        selected_mod: None,
        selected_pack: None,
        dl: None,
        dl_gen: 0,
        conflicts: vec![],
        conflict_sel: vec![],
        after_conflict: After::Nothing,
        confirm: None,
    };
    core.refresh_factorio();
    core.selected_pack = core.state.packs.first().map(|p| p.id.clone());
    core.save_config();
    let core: Shared = Arc::new(Mutex::new(core));

    let ui = AppWindow::new()?;
    {
        let k = core.lock().unwrap();
        refresh_settings(&ui, &k);
        refresh_all(&ui, &k);
        refresh_pack_detail(&ui, &k, true);
        ui.set_search_categories(strings(CATEGORIES.iter().map(|(l, _)| l.to_string()).collect()));
        ui.set_search_versions(strings(VERSIONS.iter().map(|s| s.to_string()).collect()));
        let target = k.target_factorio(None);
        ui.set_search_version(VERSIONS.iter().position(|v| *v == target).unwrap_or(0) as i32);
    }
    wire_library(&ui, &core);
    wire_search(&ui, &core);
    wire_download(&ui, &core);
    wire_packs(&ui, &core);
    wire_dialogs(&ui, &core);
    wire_settings(&ui, &core);

    task(&ui, &core, "Analyse du dépôt…", |c, w| {
        rescan(c);
        post_refresh(w, c);
        let (empty, auto, has_mods) = {
            let k = c.lock().unwrap();
            let has_mods = k
                .config
                .mods_dir()
                .and_then(|d| fs::read_dir(d).ok())
                .map(|mut rd| rd.any(|e| e.map(|e| e.path().extension().is_some_and(|x| x == "zip")).unwrap_or(false)))
                .unwrap_or(false);
            (k.index.is_empty(), k.config.auto_check_updates, has_mods)
        };
        let _ = auto;
        if let Err(e) = ensure_portal_list(c, false) {
            eprintln!("index du portail: {e:#}");
        }
        let mut msg = periodic_check(c, w)?;
        post_refresh(w, c);
        fetch_missing_icons(c);
        if empty && has_mods {
            msg = "Dépôt vide : clique sur « Importer les mods de Factorio » pour reprendre tes mods actuels.".into();
        } else if msg.is_empty() {
            msg = "Prêt".into();
        }
        Ok(msg)
    });

    // Vérification régulière (mises à jour fréquentes des mods, index, version de l'app).
    let timer = slint::Timer::default();
    {
        let (c, w) = (core.clone(), ui.as_weak());
        timer.start(slint::TimerMode::Repeated, Duration::from_secs(60), move || {
            let Some(ui) = w.upgrade() else { return };
            let due = {
                let k = c.lock().unwrap();
                !k.checking && k.last_check.map(|t| t.elapsed() >= k.config.check_interval()).unwrap_or(false)
            };
            if due {
                task(&ui, &c, "Vérification périodique…", periodic_check);
            }
        });
    }

    ui.run()?;
    drop(timer);
    Ok(())
}

// ---------------------------------------------------------------------------
// Câblage des callbacks
// ---------------------------------------------------------------------------

fn wire_library(ui: &AppWindow, core: &Shared) {
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_library_filter_changed(move || {
        let ui = w.unwrap();
        refresh_library(&ui, &c.lock().unwrap());
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_select_mod(move |name| {
        let ui = w.unwrap();
        let mut k = c.lock().unwrap();
        k.selected_mod = Some(name.to_string());
        ui.set_add_version_index(0);
        ui.set_selected_mod(name.clone());
        refresh_detail(&ui, &k);
        let preset = k.index.get(name.as_str()).map(|v| v[0].version.to_string());
        drop(k);
        open_download(&ui, &c, name.to_string(), preset, DlTarget::NoPack, false);
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_import_existing(move || {
        let ui = w.unwrap();
        task(&ui, &c, "Import des mods de Factorio dans le dépôt…", import_existing);
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_check_updates(move || {
        let ui = w.unwrap();
        task(&ui, &c, "Vérification des mises à jour…", |c, _| check_updates(c));
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_refresh_library(move || {
        let ui = w.unwrap();
        task(&ui, &c, "Analyse du dépôt…", |c, _| {
            rescan(c);
            fetch_missing_icons(c);
            Ok("Dépôt actualisé.".into())
        });
    });
    ui.on_open_portal(|name| {
        let _ = open::that(format!("https://mods.factorio.com/mod/{}", portal::urlencode(&name)));
    });
    ui.on_open_url(|url| {
        if url.starts_with("http://") || url.starts_with("https://") {
            let _ = open::that(url.as_str());
        }
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_download_update(move |name| {
        let ui = w.unwrap();
        let preset = c.lock().unwrap().updates.get(name.as_str()).map(|r| r.version.clone());
        open_download(&ui, &c, name.to_string(), preset, DlTarget::NoPack, true);
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_delete_version(move |name, version| {
        let ui = w.unwrap();
        let mut k = c.lock().unwrap();
        let Ok(v) = version.parse::<Version>() else { return };
        ui.set_confirm_title(format!("Supprimer {name} {version} du dépôt ?").into());
        ui.set_confirm_text("Le fichier sera supprimé définitivement du dépôt. Tu pourras le retélécharger depuis le portail.".into());
        ui.set_confirm_ok("Supprimer".into());
        ui.set_confirm_visible(true);
        k.confirm = Some(Confirm::DeleteVersion(name.to_string(), v));
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_use_version_everywhere(move |name, version| {
        let ui = w.unwrap();
        let mut k = c.lock().unwrap();
        let mut n = 0;
        for p in k.state.packs.iter_mut() {
            if let Some(e) = p.mods.iter_mut().find(|e| e.name == name.as_str()) {
                e.version = version.to_string();
                n += 1;
            }
        }
        k.save_state();
        refresh_all(&ui, &k);
        ui.set_status(format!("{name} {version} utilisé dans {n} modpack(s).").into());
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_add_to_pack(move |name, pack_idx, ver_idx| {
        let ui = w.unwrap();
        let mut k = c.lock().unwrap();
        let Some(versions) = k.index.get(name.as_str()).cloned() else { return };
        let Some(info) = versions.get(ver_idx.max(0) as usize).cloned() else { return };
        let Some((pack_id, pname)) =
            k.state.packs.get(pack_idx.max(0) as usize).map(|p| (p.id.clone(), p.name.clone()))
        else {
            return;
        };
        let added = add_mod_to_pack(&mut k, &pack_id, &info);
        k.save_state();
        refresh_all(&ui, &k);
        ui.set_status(format!("{} {} ajouté à « {pname} » (+{added} dépendance(s)).", info.name, info.version).into());
    });
}

fn wire_search(ui: &AppWindow, core: &Shared) {
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_do_search(move || {
        let ui = w.unwrap();
        let query = ui.get_search_query().to_string();
        let cat = CATEGORIES.get(ui.get_search_category().max(0) as usize).and_then(|(_, c)| *c).map(String::from);
        let ver = match ui.get_search_version() {
            i if i > 0 => VERSIONS.get(i as usize).map(|s| s.to_string()),
            _ => None,
        };
        let loaded = c.lock().unwrap().portal_list.is_some();
        let label = if loaded { "Recherche…" } else { "Téléchargement de l'index du portail (une fois par jour)…" };
        let stale = portal_list_stale(&c.lock().unwrap());
        task(&ui, &c, label, move |c, w| {
            let list = ensure_portal_list(c, false)?;
            let hits = portal::search(&list, &query, cat.as_deref(), ver.as_deref(), 200);
            let names: Vec<String> = hits.iter().map(|m| m.name.clone()).collect();
            let data: Vec<ModListEntry> = hits.into_iter().cloned().collect();
            let total = data.len();
            let c2 = c.clone();
            let index_size = list.len();
            let _ = w.upgrade_in_event_loop(move |ui| {
                let k = c2.lock().unwrap();
                let rows: Vec<SearchRow> = data
                    .iter()
                    .map(|m| {
                        let rel = m.latest_release.as_ref();
                        let title = display_title(&m.title, &m.name);
                        let thumb = k.portal_thumb_cached(&m.name);
                        let icon = load_image(thumb.as_ref());
                        SearchRow {
                            name: m.name.clone().into(),
                            letter: letter(&title),
                            title: title.into(),
                            owner: one_line(&m.owner).into(),
                            summary: short(&m.summary, 300).into(),
                            category: portal::category_label(m.category.as_deref().unwrap_or("")).into(),
                            is_modpack: portal::is_modpack_category(m.category.as_deref()),
                            downloads: thousands(m.downloads_count).into(),
                            factorio: rel.map(|r| r.factorio()).unwrap_or_default().into(),
                            version: rel.map(|r| r.version.clone()).unwrap_or_default().into(),
                            has_icon: icon.is_some(),
                            icon: icon.unwrap_or_default(),
                            installed: k
                                .index
                                .get(&m.name)
                                .map(|v| v[0].version.to_string())
                                .unwrap_or_default()
                                .into(),
                        }
                    })
                    .collect();
                ui.set_search_results(model(rows));
                ui.set_search_info(format!("{total} résultat(s) • index : {index_size} mods").into());
            });
            // Miniatures des premiers résultats
            let portal = c.lock().unwrap().portal.clone();
            let first: Vec<String> = names.into_iter().take(60).collect();
            for chunk in first.chunks(12) {
                let found = parallel_map(chunk, 6, |n| portal.thumbnail(n).map(|p| (n.clone(), p)));
                let _ = w.upgrade_in_event_loop(move |ui| {
                    let rows = ui.get_search_results();
                    for (name, path) in found {
                        for i in 0..rows.row_count() {
                            if let Some(mut r) = rows.row_data(i) {
                                if r.name.as_str() == name {
                                    if let Some(img) = load_image(Some(&path)) {
                                        r.icon = img;
                                        r.has_icon = true;
                                        rows.set_row_data(i, r);
                                    }
                                }
                            }
                        }
                    }
                });
            }
            if stale && loaded {
                // Index ancien : on le rafraîchit puis on relance la recherche avec la liste à jour.
                post_status(w, "Rafraîchissement de l'index du portail…");
                ensure_portal_list(c, true)?;
                let _ = w.upgrade_in_event_loop(|ui| ui.invoke_do_search());
                return Ok("Index du portail rafraîchi.".into());
            }
            Ok(format!("{total} résultat(s)."))
        });
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_refresh_index(move || {
        let ui = w.unwrap();
        task(&ui, &c, "Téléchargement de l'index du portail…", |c, _| {
            let l = ensure_portal_list(c, true)?;
            Ok(format!("Index du portail mis à jour ({} mods).", l.len()))
        });
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_open_download(move |name| {
        let ui = w.unwrap();
        open_download(&ui, &c, name.to_string(), None, DlTarget::SelectedPack, true);
    });
}

impl Core {
    fn portal_thumb_cached(&self, name: &str) -> Option<PathBuf> {
        if let Some(p) = self.icons.get(name) {
            return Some(p.clone());
        }
        let p = self.paths.cache.join(portal::THUMBS_DIR).join(format!("{}.png", portal::sanitize(name)));
        p.exists().then_some(p)
    }
}

fn wire_download(ui: &AppWindow, core: &Shared) {
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_dl_version_changed(move |idx| {
        let ui = w.unwrap();
        ui.set_dl_loading(true);
        let c = c.clone();
        let w = ui.as_weak();
        std::thread::spawn(move || load_dl_items(&c, &w, idx.max(0) as usize));
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_dl_toggle_dep(move |i, checked| {
        let _ = &w;
        let mut k = c.lock().unwrap();
        if let Some(item) = k.dl.as_mut().and_then(|d| d.items.get_mut(i as usize)) {
            if item.checkable {
                item.checked = checked;
            }
        }
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_dl_toggle_group(move |group, checked| {
        let ui = w.unwrap();
        let mut k = c.lock().unwrap();
        if let Some(d) = k.dl.as_mut() {
            for item in d.items.iter_mut().filter(|i| i.checkable && i.kind.group() == group) {
                item.checked = checked;
            }
        }
        refresh_dl(&ui, &k);
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_dl_cancel(move || {
        let ui = w.unwrap();
        c.lock().unwrap().dl = None;
        ui.set_dl_visible(false);
        reselect_mod(&ui);
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_dl_confirm(move || {
        let ui = w.unwrap();
        let (target, title) = {
            let k = c.lock().unwrap();
            let idx = ui.get_dl_target_index();
            let target = if idx > 0 { k.state.packs.get(idx as usize - 1).map(|p| p.id.clone()) } else { None };
            (target, k.dl.as_ref().map(|d| d.title.clone()).unwrap_or_default())
        };
        // La création de modpack n'est proposée que dans la fenêtre.
        let new_pack = (ui.get_dl_visible() && ui.get_dl_is_modpack() && ui.get_dl_make_pack()).then(|| {
            let n = ui.get_dl_pack_name().trim().to_string();
            if n.is_empty() { title.clone() } else { n }
        });
        let target = if new_pack.is_some() { None } else { target };
        ui.set_dl_visible(false);
        task(&ui, &c, &format!("Téléchargement de {title}…"), move |c, w| {
            let res = confirm_download(c, w, target, new_pack);
            let _ = w.upgrade_in_event_loop(|ui| reselect_mod(&ui));
            res
        });
    });
}

/// Recharge la liste des dépendances intégrée à la fiche du mod sélectionné (bibliothèque).
fn reselect_mod(ui: &AppWindow) {
    let sel = ui.get_selected_mod();
    if ui.get_page() == 0 && !sel.is_empty() && !ui.get_dl_visible() {
        ui.invoke_select_mod(sel);
    }
}

fn wire_packs(ui: &AppWindow, core: &Shared) {
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_new_pack(move || {
        let ui = w.unwrap();
        let mut k = c.lock().unwrap();
        let mut p = Modpack::new("Nouveau modpack");
        if k.config.feat_factorio_version {
            let t = k.target_factorio(None);
            p.factorio_version = (!t.is_empty()).then_some(t);
        }
        k.selected_pack = Some(p.id.clone());
        k.state.packs.push(p);
        k.save_state();
        refresh_all(&ui, &k);
        refresh_pack_detail(&ui, &k, true);
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_select_pack(move |id| {
        let ui = w.unwrap();
        let mut k = c.lock().unwrap();
        k.selected_pack = Some(id.to_string());
        ui.set_pack_add_query("".into());
        ui.set_pack_add_results(model(vec![]));
        ui.set_pack_mod_filter("".into());
        refresh_pack_detail(&ui, &k, true);
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_toggle_pack(move |id, on| {
        let ui = w.unwrap();
        toggle_pack(&ui, &c, id.as_str(), on);
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_save_pack_meta(move || {
        let ui = w.unwrap();
        let mut k = c.lock().unwrap();
        let Some(id) = k.selected_pack.clone() else { return };
        let name = ui.get_pack_name().trim().to_string();
        let desc = ui.get_pack_description().to_string();
        let fv = ui.get_pack_factorio().trim().to_string();
        if let Some(p) = k.state.pack_mut(&id) {
            if !name.is_empty() {
                p.name = name;
            }
            p.description = desc;
            p.factorio_version = (!fv.is_empty()).then_some(fv);
        }
        k.save_state();
        refresh_all(&ui, &k);
        ui.set_status("Modpack enregistré.".into());
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_delete_pack(move || {
        let ui = w.unwrap();
        let mut k = c.lock().unwrap();
        let Some(id) = k.selected_pack.clone() else { return };
        let name = k.state.pack(&id).map(|p| p.name.clone()).unwrap_or_default();
        ui.set_confirm_title(format!("Supprimer le modpack « {name} » ?").into());
        ui.set_confirm_text("Le modpack et ses réglages seront supprimés. Les mods restent dans le dépôt.".into());
        ui.set_confirm_ok("Supprimer".into());
        ui.set_confirm_visible(true);
        k.confirm = Some(Confirm::DeletePack(id));
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_duplicate_pack(move || {
        let ui = w.unwrap();
        let mut k = c.lock().unwrap();
        let Some(src) = k.selected_pack.as_ref().and_then(|id| k.state.pack(id)).cloned() else { return };
        let mut p = src.clone();
        p.id = modpack::new_id();
        p.name = format!("{} (copie)", src.name);
        let from = k.paths.pack_settings(&src.id);
        if from.exists() {
            let to = k.paths.pack_settings(&p.id);
            let _ = fs::create_dir_all(to.parent().unwrap());
            let _ = fs::copy(from, to);
        }
        k.selected_pack = Some(p.id.clone());
        k.state.packs.push(p);
        k.save_state();
        refresh_all(&ui, &k);
        refresh_pack_detail(&ui, &k, true);
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_pack_mod_version(move |i, vi| {
        let ui = w.unwrap();
        let mut k = c.lock().unwrap();
        let Some(id) = k.selected_pack.clone() else { return };
        let index = k.index.clone();
        if let Some(e) = k.state.pack_mut(&id).and_then(|p| p.mods.get_mut(i as usize)) {
            if let Some(v) = pack_mod_versions(&index, e).get(vi.max(0) as usize) {
                e.version = v.clone();
            }
        }
        k.save_state();
        refresh_all(&ui, &k);
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_pack_mod_enabled(move |i, on| {
        let ui = w.unwrap();
        let mut k = c.lock().unwrap();
        let Some(id) = k.selected_pack.clone() else { return };
        if let Some(e) = k.state.pack_mut(&id).and_then(|p| p.mods.get_mut(i as usize)) {
            e.enabled = on;
        }
        k.save_state();
        refresh_all(&ui, &k);
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_pack_mod_remove(move |i| {
        let ui = w.unwrap();
        let mut k = c.lock().unwrap();
        let Some(id) = k.selected_pack.clone() else { return };
        if let Some(p) = k.state.pack_mut(&id) {
            if (i as usize) < p.mods.len() {
                p.mods.remove(i as usize);
            }
        }
        k.save_state();
        refresh_all(&ui, &k);
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_pack_builtin(move |i, on| {
        let ui = w.unwrap();
        let mut k = c.lock().unwrap();
        let Some(id) = k.selected_pack.clone() else { return };
        let Some(name) = k.builtins.get(i as usize).cloned() else { return };
        if let Some(p) = k.state.pack_mut(&id) {
            p.disabled_builtins.retain(|b| b != &name);
            if !on {
                p.disabled_builtins.push(name);
            }
        }
        k.save_state();
        refresh_all(&ui, &k);
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_capture_settings(move || {
        let ui = w.unwrap();
        let k = c.lock().unwrap();
        let Some(id) = k.selected_pack.clone() else { return };
        let candidates: Vec<PathBuf> = [k.config.mods_dir(), Some(k.paths.profile.clone())]
            .into_iter()
            .flatten()
            .map(|d| d.join("mod-settings.dat"))
            .filter(|p| p.exists())
            .collect();
        let newest = candidates.into_iter().max_by_key(|p| fs::metadata(p).and_then(|m| m.modified()).ok());
        let msg = match newest {
            None => "Aucun mod-settings.dat trouvé (lance le jeu une fois).".to_string(),
            Some(src) => {
                let dst = k.paths.pack_settings(&id);
                let _ = fs::create_dir_all(dst.parent().unwrap());
                match fs::copy(&src, &dst) {
                    Ok(_) => format!("Réglages capturés depuis {}.", src.display()),
                    Err(e) => format!("Erreur : {e}"),
                }
            }
        };
        refresh_all(&ui, &k);
        ui.set_status(msg.into());
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_clear_settings(move || {
        let ui = w.unwrap();
        let k = c.lock().unwrap();
        if let Some(id) = &k.selected_pack {
            let _ = fs::remove_file(k.paths.pack_settings(id));
        }
        refresh_all(&ui, &k);
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_add_save(move || {
        let ui = w.unwrap();
        let dir = c.lock().unwrap().config.factorio_user_dir.as_ref().map(|d| d.join("saves"));
        let mut dlg = rfd::FileDialog::new().add_filter("Sauvegarde Factorio", &["zip"]);
        if let Some(d) = dir {
            dlg = dlg.set_directory(d);
        }
        let Some(path) = dlg.pick_file() else { return };
        let mut k = c.lock().unwrap();
        let Some(id) = k.selected_pack.clone() else { return };
        if let Some(p) = k.state.pack_mut(&id) {
            if !p.saves.contains(&path) {
                p.saves.push(path);
            }
        }
        k.save_state();
        refresh_all(&ui, &k);
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_remove_save(move |i| {
        let ui = w.unwrap();
        let mut k = c.lock().unwrap();
        let Some(id) = k.selected_pack.clone() else { return };
        if let Some(p) = k.state.pack_mut(&id) {
            if (i as usize) < p.saves.len() {
                p.saves.remove(i as usize);
            }
        }
        k.save_state();
        refresh_all(&ui, &k);
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_launch_save(move |i| {
        let ui = w.unwrap();
        let save = {
            let mut k = c.lock().unwrap();
            let Some(id) = k.selected_pack.clone() else { return };
            let Some(save) = k.state.pack(&id).and_then(|p| p.saves.get(i as usize)).cloned() else { return };
            if !k.state.is_active(&id) {
                k.state.active.push(id);
                k.save_state();
            }
            refresh_all(&ui, &k);
            save
        };
        proceed(&ui, &c, After::Launch(Some(save)));
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_export_pack(move || {
        let ui = w.unwrap();
        let (pack, settings) = {
            let k = c.lock().unwrap();
            let Some(p) = k.selected_pack.as_ref().and_then(|id| k.state.pack(id)).cloned() else { return };
            let s = k.config.feat_mod_settings.then(|| k.paths.pack_settings(&p.id));
            (p, s)
        };
        let Some(dest) = rfd::FileDialog::new()
            .add_filter("Modpack", &["fmpack"])
            .set_file_name(format!("{}.fmpack", portal::sanitize(&pack.name)))
            .save_file()
        else {
            return;
        };
        task(&ui, &c, "Export du modpack…", move |c, _| {
            let (index, include) = {
                let k = c.lock().unwrap();
                (k.index.clone(), k.config.export_include_mods)
            };
            transfer::export(&pack, &dest, &index, settings.as_deref(), include)?;
            Ok(format!("Modpack exporté vers {}.", dest.display()))
        });
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_import_pack(move || {
        let ui = w.unwrap();
        let Some(src) = rfd::FileDialog::new().add_filter("Modpack", &["fmpack"]).pick_file() else { return };
        task(&ui, &c, "Import du modpack…", move |c, w| import_pack(c, w, &src));
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_pack_mod_deps(move |i| {
        let ui = w.unwrap();
        let (id, entry) = {
            let k = c.lock().unwrap();
            let Some(id) = k.selected_pack.clone() else { return };
            let Some(e) = k.state.pack(&id).and_then(|p| p.mods.get(i as usize)).cloned() else { return };
            (id, e)
        };
        open_download(&ui, &c, entry.name, Some(entry.version), DlTarget::Pack(id), true);
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_pack_mod_filter_changed(move || {
        let ui = w.unwrap();
        refresh_pack_detail(&ui, &c.lock().unwrap(), false);
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_pack_add_search(move || {
        let ui = w.unwrap();
        let query = ui.get_pack_add_query().to_string();
        let needs_index = c.lock().unwrap().portal_list.is_none() && query.trim().chars().count() >= 2;
        let rows = pack_add_rows(&c.lock().unwrap(), &query);
        ui.set_pack_add_results(model(rows));
        if needs_index {
            // Charge l'index du portail en arrière-plan puis relance la recherche.
            task(&ui, &c, "Chargement de l'index du portail…", |c, w| {
                ensure_portal_list(c, false)?;
                let _ = w.upgrade_in_event_loop(|ui| ui.invoke_pack_add_search());
                Ok(String::new())
            });
        }
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_pack_add(move |name, installed| {
        let ui = w.unwrap();
        // Dans tous les cas, on propose les dépendances avant d'ajouter le mod au modpack.
        let preset = if installed {
            c.lock().unwrap().index.get(name.as_str()).map(|v| v[0].version.to_string())
        } else {
            None
        };
        ui.set_pack_add_query("".into());
        ui.set_pack_add_results(model(vec![]));
        open_download(&ui, &c, name.to_string(), preset, DlTarget::SelectedPack, true);
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_browse_modpacks(move || {
        let ui = w.unwrap();
        ui.set_page(1);
        ui.set_search_category(1);
        ui.invoke_do_search();
        let _ = &c;
    });
}

fn import_pack(c: &Shared, w: &Weak<AppWindow>, src: &Path) -> Result<String> {
    let (store, tmp, portal, creds, downloads) = {
        let k = c.lock().unwrap();
        (k.store.clone(), k.paths.downloads.join("import"), k.portal.clone(), k.credentials(), k.paths.downloads.clone())
    };
    let imported = transfer::import(src, &store, &tmp)?;
    let mut fetched = 0;
    let mut failed = Vec::new();
    if !imported.missing.is_empty() {
        let n = imported.missing.len();
        for (i, (name, ver)) in imported.missing.iter().enumerate() {
            post_status(w, format!("Téléchargement {}/{n} : {name} {ver}", i + 1));
            post_progress(w, i as f32 / n as f32);
            let r = (|| -> Result<()> {
                let (u, t) = creds.clone().ok_or_else(|| anyhow!("identifiants manquants"))?;
                let full = portal.full(name)?;
                let rel = full
                    .releases
                    .iter()
                    .find(|r| &r.version == ver)
                    .ok_or_else(|| anyhow!("version introuvable"))?;
                let p = portal.download(rel, &u, &t, &downloads, |_, _| {})?;
                store.add_file(&p, AddMode::Move)?;
                Ok(())
            })();
            match r {
                Ok(()) => fetched += 1,
                Err(e) => failed.push(format!("{name} {ver} ({e})")),
            }
        }
    }
    rescan(c);
    let mut k = c.lock().unwrap();
    let pack = imported.pack;
    if let Some(bytes) = imported.settings {
        let dst = k.paths.pack_settings(&pack.id);
        fs::create_dir_all(dst.parent().unwrap())?;
        fs::write(dst, bytes)?;
    }
    let name = pack.name.clone();
    k.selected_pack = Some(pack.id.clone());
    k.state.packs.push(pack);
    k.save_state();
    let mut msg = format!("Modpack « {name} » importé ({fetched} mod(s) téléchargé(s))");
    if !failed.is_empty() {
        msg.push_str(&format!(" — échecs : {}", failed.join(", ")));
    }
    Ok(msg)
}

fn wire_dialogs(ui: &AppWindow, core: &Shared) {
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_conflict_select(move |i, sel| {
        let _ = w.unwrap();
        let mut k = c.lock().unwrap();
        if let Some(s) = k.conflict_sel.get_mut(i as usize) {
            *s = sel.max(0) as usize;
        }
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_conflict_confirm(move || {
        let ui = w.unwrap();
        let after = {
            let mut k = c.lock().unwrap();
            let remember = ui.get_conflict_remember();
            let picks: Vec<(String, String)> = k
                .conflicts
                .iter()
                .zip(k.conflict_sel.iter())
                .filter_map(|(c, s)| c.options.get(*s).map(|o| (c.name.clone(), o.version.clone())))
                .collect();
            for (name, ver) in picks {
                if remember {
                    k.state.conflict_choices.insert(name.clone(), ver.clone());
                    k.session_choices.remove(&name);
                } else {
                    k.session_choices.insert(name, ver);
                }
            }
            k.conflicts.clear();
            k.save_state();
            refresh_all(&ui, &k);
            std::mem::replace(&mut k.after_conflict, After::Nothing)
        };
        ui.set_conflict_visible(false);
        match after {
            After::Nothing => ui.set_status("Conflits résolus.".into()),
            After::Sync => start_sync(&ui, &c),
            After::Launch(s) => start_launch(&ui, &c, s),
        }
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_conflict_cancel(move || {
        let ui = w.unwrap();
        let mut k = c.lock().unwrap();
        k.conflicts.clear();
        k.after_conflict = After::Nothing;
        ui.set_conflict_visible(false);
        ui.set_status("Conflits non résolus : la version la plus récente sera utilisée par défaut.".into());
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_confirm_cancel(move || {
        let ui = w.unwrap();
        c.lock().unwrap().confirm = None;
        ui.set_confirm_visible(false);
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_confirm_accept(move || {
        let ui = w.unwrap();
        ui.set_confirm_visible(false);
        let action = c.lock().unwrap().confirm.take();
        match action {
            None => {}
            Some(Confirm::Sync(plan, settings)) => {
                task(&ui, &c, "Synchronisation du dossier mods/…", move |c, _| {
                    let mods_dir = c.lock().unwrap().config.mods_dir().ok_or_else(|| anyhow!("dossier non configuré"))?;
                    sync::apply(&mods_dir, &plan, settings.as_deref())?;
                    Ok(format!(
                        "Factorio synchronisé : +{} / -{} mod(s). Sauvegarde : mod-list.json.fmm-backup",
                        plan.add.len(),
                        plan.remove.len()
                    ))
                });
            }
            Some(Confirm::DeletePack(id)) => {
                let mut k = c.lock().unwrap();
                k.state.packs.retain(|p| p.id != id);
                k.state.active.retain(|a| a != &id);
                let _ = fs::remove_dir_all(k.paths.packs.join(&id));
                k.selected_pack = k.state.packs.first().map(|p| p.id.clone());
                k.save_state();
                refresh_all(&ui, &k);
                refresh_pack_detail(&ui, &k, true);
                ui.set_status("Modpack supprimé.".into());
            }
            Some(Confirm::DeleteVersion(name, v)) => {
                let mut k = c.lock().unwrap();
                let info = store::find(&k.index, &name, &v).cloned();
                let msg = match info.map(|i| k.store.remove(&i)) {
                    Some(Ok(())) => {
                        if let Some(list) = k.index.get_mut(&name) {
                            list.retain(|m| m.version != v);
                            if list.is_empty() {
                                k.index.remove(&name);
                                k.selected_mod = None;
                            }
                        }
                        format!("{name} {v} supprimé du dépôt.")
                    }
                    Some(Err(e)) => format!("Erreur : {e:#}"),
                    None => "Version introuvable.".into(),
                };
                refresh_all(&ui, &k);
                ui.set_status(msg.into());
            }
        }
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_sync_now(move || {
        let ui = w.unwrap();
        proceed(&ui, &c, After::Sync);
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_launch_game(move || {
        let ui = w.unwrap();
        proceed(&ui, &c, After::Launch(None));
    });
}

fn wire_settings(ui: &AppWindow, core: &Shared) {
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_save_settings(move || {
        let ui = w.unwrap();
        let mut k = c.lock().unwrap();
        let path = |s: SharedString| {
            let s = s.trim().to_string();
            (!s.is_empty()).then(|| PathBuf::from(s))
        };
        k.config.factorio_user_dir = path(ui.get_s_user_dir());
        k.config.factorio_exe = path(ui.get_s_exe());
        k.config.username = ui.get_s_username().trim().to_string();
        k.config.token = ui.get_s_token().trim().to_string();
        k.config.target_factorio = ui.get_s_target().trim().to_string();
        k.config.feat_mod_settings = ui.get_s_feat_settings();
        k.config.feat_factorio_version = ui.get_s_feat_version();
        k.config.feat_export = ui.get_s_feat_export();
        k.config.feat_saves = ui.get_s_feat_saves();
        k.config.auto_check_updates = ui.get_s_auto_updates();
        k.config.auto_sync_on_activate = ui.get_s_auto_sync();
        k.config.export_include_mods = ui.get_s_export_mods();
        k.config.copy_back_settings = ui.get_s_copy_back();
        k.config.check_interval_minutes = ui.get_s_interval().trim().parse().unwrap_or(10).clamp(1, 1440);
        k.config.auto_update_app = ui.get_s_auto_app();
        k.refresh_factorio();
        k.save_config();
        refresh_settings(&ui, &k);
        refresh_all(&ui, &k);
        ui.set_status("Paramètres enregistrés.".into());
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_detect_factorio(move || {
        let ui = w.unwrap();
        let mut k = c.lock().unwrap();
        k.config.factorio_user_dir = factorio::default_user_dir().filter(|d| d.exists());
        k.config.factorio_exe = factorio::detect_exe(k.config.factorio_user_dir.as_deref());
        k.refresh_factorio();
        k.save_config();
        refresh_settings(&ui, &k);
        ui.set_status("Détection terminée.".into());
    });
    let w = ui.as_weak();
    ui.on_browse_user_dir(move || {
        let ui = w.unwrap();
        if let Some(d) = rfd::FileDialog::new().pick_folder() {
            ui.set_s_user_dir(d.display().to_string().into());
        }
    });
    let w = ui.as_weak();
    ui.on_browse_exe(move || {
        let ui = w.unwrap();
        if let Some(f) = rfd::FileDialog::new().pick_file() {
            ui.set_s_exe(f.display().to_string().into());
        }
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_install_app_update(move || {
        let ui = w.unwrap();
        let Some(rel) = c.lock().unwrap().app_update.clone() else { return };
        task(&ui, &c, &format!("Téléchargement de la version {}…", rel.version), move |c, w| {
            let dir = c.lock().unwrap().paths.downloads.join("app-update");
            let w2 = w.clone();
            let file = updater::download(&rel, &dir, |done, total| {
                if total > 0 {
                    post_progress(&w2, done as f32 / total as f32);
                }
            })?;
            if updater::install(&file)? {
                let _ = slint::invoke_from_event_loop(|| {
                    let _ = slint::quit_event_loop();
                });
                return Ok(format!("Installation de la version {}…", rel.version));
            }
            Ok(format!("Version {} téléchargée : glisse l'application dans Applications.", rel.version))
        });
    });
    let c = core.clone();
    ui.on_open_data_dir(move || {
        let _ = open::that(&c.lock().unwrap().paths.data);
    });
}
