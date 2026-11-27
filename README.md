# Factorio Mod Manager

Gestionnaire de mods et de modpacks pour Factorio, en Rust avec une interface native [Slint](https://slint.dev).

## Fonctionnalités

- **Bibliothèque** : tous les mods du dépôt avec icône et description courte ; fiche détaillée (versions, modpacks qui les utilisent, dépendances, description).
- **Recherche sur le portail** : filtres par catégorie et version de Factorio ; choix des dépendances à télécharger (obligatoires toujours incluses, recommandées / optionnelles / optionnelles cachées au choix).
- **Dépôt multi-versions** : chaque version téléchargée est conservée, un modpack peut figer une version pendant qu'un autre passe à la suivante (rollback possible).
- **Modpacks** : plusieurs modpacks actifs en même temps, création depuis un mod de la catégorie « modpack », ajout de mods depuis la vue du pack, DLC par pack, réglages des mods (`mod-settings.dat`), sauvegardes liées, export / import (`.fmpack`).
- **Conflits** : si deux modpacks actifs demandent des versions différentes d'un mod, l'application demande laquelle charger.
- **Deux modes d'application** : synchroniser le dossier `mods/` de Factorio (avec aperçu et sauvegarde de `mod-list.json`) ou lancer le jeu avec un dossier de mods dédié (`--mod-directory`).
- **Mises à jour** : vérification régulière des mods (intervalle réglable), « Tout mettre à jour » avec le changelog de chaque mod et les nouvelles dépendances (obligatoires ajoutées d'office, optionnelles au choix) ; les modpacks suivent la dernière version sauf version fixée à la main. Mise à jour d'un seul modpack avec aperçu, et retour à l'état précédent. Mise à jour automatique de l'application depuis les releases GitHub.
- **Bibliothèque** : recherche par ID, nom, modpack et auteur (avec l'endroit trouvé), filtres (actifs, non utilisés, mises à jour, incompatibles), tri (nom, téléchargement, taille, dernière mise à jour), nettoyage des anciennes versions inutilisées avec la place libérée.
- **Fiabilité** : SHA1 vérifié et nouvel essai automatique des téléchargements, sauvegarde de `mod-list.json` et `mod-settings.dat` à chaque synchronisation, journal consultable dans l'app.
- **Confort** : `Ctrl+F` (filtre), `Échap` (fermer), `Suppr` (supprimer la sélection), glisser-déposer de zips de mods ou de `.fmpack`, icône dans la zone de notification (Windows) pour lancer le jeu avec les modpacks actifs.

Les identifiants du portail sont lus depuis `player-data.json` (connexion au portail faite dans le jeu), ou saisis dans les paramètres.

## Installation

Télécharger l'installateur de la [dernière release](https://github.com/YetAnotherRandomProject/FactorioModManager/releases/latest) :

| Plateforme | Fichier | Mise à jour automatique |
|---|---|---|
| Windows | `*_x64-setup.exe` (installation par utilisateur, sans droits admin) | oui |
| Linux | `*.AppImage` (ou `*.deb`) | oui pour l'AppImage |
| macOS | `*_aarch64.dmg` / `*_x64.dmg` | téléchargement + ouverture de l'image |

## Développement

```sh
cargo run            # lancer l'application
cargo test           # tests unitaires
cargo test -- --ignored download_real_mod   # test réseau (identifiants Factorio requis)
```

Sous Linux : `libfontconfig1-dev libxkbcommon-dev libxcb-shape0-dev libxcb-xfixes0-dev libwayland-dev`.

Installateur local : `cargo install cargo-packager --locked`, puis `cargo build --release` et `cargo packager --release`.

## Publier une version

1. Mettre à jour `version` dans `Cargo.toml` (et `Cargo.lock` via `cargo build`), commiter.
2. `git tag v0.2.0 && git push origin v0.2.0`.
3. Le workflow **Release** construit les installateurs et publie la release ; les applications installées la proposent au prochain contrôle.
