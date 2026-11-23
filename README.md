# Zaimule

[![Release](https://img.shields.io/github/v/release/ZainadosRepo/Zaimule)](https://github.com/ZainadosRepo/Zaimule/releases/latest)
[![CI](https://github.com/ZainadosRepo/Zaimule/actions/workflows/ci.yml/badge.svg)](https://github.com/ZainadosRepo/Zaimule/actions/workflows/ci.yml)

Station d'émulation native en **Rust + Tauri 2** dédiée aux consoles portables **Game Boy, Game Boy Color, Game Boy Advance et Nintendo DS** en Rust pur, accompagnée d'un **Coffre Switch isolé (Option A)** permettant à chaque utilisateur d'importer ses propres builds **Torzu** (Switch 1), **Fritchii / Lichii** (Switch 2) et ses clés de chiffrement.

## Télécharger

Dernière version : **[Releases › Latest](https://github.com/ZainadosRepo/Zaimule/releases/latest)**

| Système | Fichier |
| --- | --- |
| Windows 10 / 11 (64 bits) | `Zaimule_<version>_x64-setup.exe` |
| macOS (Intel et Apple Silicon) | `Zaimule_<version>_universal.dmg` |

### Windows

1. Lancer `Zaimule_<version>_x64-setup.exe`.
2. L'installeur n'est pas signé : si Windows SmartScreen s'affiche, cliquer sur **Informations complémentaires**, puis **Exécuter quand même**.
3. Suivre l'installeur ; Zaimule apparaît dans le menu Démarrer.

### macOS

1. Ouvrir le `.dmg` et glisser **Zaimule** dans **Applications**.
2. L'appli n'est pas signée : au premier lancement, macOS la bloque. Ouvrir **Réglages Système › Confidentialité et sécurité**, puis cliquer sur **Ouvrir quand même** en bas de la page.

### Mettre à jour

Au lancement, l'appli regarde sur GitHub si une version plus récente est publiée et propose de l'installer : **Installer** la télécharge, vérifie sa signature et redémarre l'appli ; **Plus tard** la reproposera au prochain lancement. Hors ligne, rien ne s'affiche. Les sauvegardes SRAM, Save States et réglages sont conservés.

---

## ✨ Architecture & Fonctionnalités (`v1.7.1`)

- **🧹 Démarrage Propre, Bibliothèque & Dossiers ROMs (`.zip`, `.7z`, Fichiers & Dossiers)**
  - **Zéro contenu propriétaire inclus & zéro scan automatique** : la bibliothèque démarre vide sans parcourir vos dossiers personnels ; aucune ROM, aucune clé Switch ni aucun binaire externe n'est distribué avec Zaimule.
  - **Importation directe & gestion de dossiers (`scanner.rs` & `archive.rs`)** : sélecteurs natifs de fichiers/dossiers (`tauri-plugin-dialog`), glisser-déposer direct, extraction streaming **`.zip` / ZIP64** et **`.7z`**, boutons **Rescanner**, **Tout rescanner** et **Retirer** par dossier, **Favoris**, tri dynamique, temps de jeu natif persisté et navigation clavier/manette 2D filtrée.
- **🦀 Cœurs d'Émulation 100% Rust Natifs (`crates/zaimule-emu` & `vendor/termboy-*`)**
  - **Game Boy & Game Boy Color (`GB` / `GBC`, `zaimule-rust-gb`)** : motorisé par `termboy-gb` + `termboy-core` (MIT) — affichage `160×144` RGBA8888 à `59.73 Hz`, APU 4 canaux `48 kHz` stéréo, sauvegarde batterie MBC (`.srm`) avec suivi de modifications, Save States `"TBSS"` et Câble Link Série `SB`/`SC`.
  - **Game Boy Advance (`GBA`, `zaimule-rust-gba`)** : motorisé par `termboy-gba` + `termboy-core` (MIT) — CPU ARM7TDMI + BIOS HLE SWI/DMA, PPU `240×160` RGBA8888 à `59.73 Hz` avec correction couleur LCD, APU `48 kHz` stéréo, sauvegarde SRAM/Flash et Save States `"TBSS"`.
  - **Nintendo DS (`NDS`, `zaimule-rust-nds`)** : motorisé par `nds_core` (`x0cero/nds` MIT) — double CPU **ARM946E-S + ARM7TDMI**, support optionnel de vos propres dumps **BIOS / Firmware** (`bios7.bin`, `bios9.bin`, `firmware.bin`) avec vérification CRC32 et calibration tactile **TSC2046**, **4 dispositions d'écrans** (Vertical, Côte à côte, Inversés) avec stylet tactile adapté, **SPU 16 canaux `48 kHz` stéréo**, **moteur 3D** (`4×4`, `Decal`/`Toon`/`Highlight`), **PPU 2D complet**, télémétrie `VCOUNT`/cycles en direct, Flash 512 Ko et Save States RLE `"ZMND"`.
- **🔐 Coffre Nintendo Switch Option A — Torzu & Fritchii (`external.rs`)**
  - **Séparation Switch 1 & Switch 2** : `SWITCH` (`.nsp`, `.xci`, `.nsz`, `.xcz`, `.nro`, `.nso`) cible par défaut **Torzu**, tandis que `SWITCH2` (`.nx`, `.nup`) cible par défaut **Fritchii / Lichii**, avec **surcharge par jeu** (`Auto` / `Torzu` / `Fritchii`).
  - **Coffre isolé (`<app_config_dir>/switch`)** : importation de vos builds (dossier, exécutable, `.zip`/`.7z`), de vos clés (`prod.keys` / `title.keys`) et de votre **Firmware Nintendo Switch (`.nca`)** synchronisés vers Torzu et Fritchii avant exécution (`-f -g <rom>`), purge du cache shaders et comptabilisation du temps de jeu (`switch-session-ended`).
- **⚡ Optimisations Moteur, Pacing Matériel, IPC Binaire & Audio Temps Réel (`pacing.js`, `session.rs`, `viewport_ipc.js`, `audio_worklet.js`)**
  - **Cadencement matériel (`FramePacer`)** : verrouillage de la cadence sur `59.73 Hz` (GB/GBC/GBA) et `59.83 Hz` (NDS) sur écrans 60/120/144/240 Hz, plus mode **Pause (`F9`)**, **Reset matériel à froid** (préservant la SRAM), **Plein écran (`F11`)** et **Capture PNG (`F12`)**.
  - **Protocole Binaire `"ZMFR"` v1 (`step_viewport_binary`) & Audio `AudioWorkletNode`** : transport binaire sans JSON pour les pixels et le PCM `f32`, rééchantillonnage linéaire stéréo, réglage de volume persisté et décimation audio propre en avance rapide (`1x → 2x → 4x → 8x`).
- **🎨 Upscalers, Atelier, Netplay LAN & Persistance Atomique**
  - **Upscalers & Shaders (`ui/upscaler.js`)** : **Scale2x / EPX**, **xBRZ 2x**, **CRT Bloom** et 5 modes visuels persistés (CRT Scanlines, Trinitron, Game Boy DMG, LCD, Pixel-Perfect).
  - **Outils Power-User (`cheats.rs`, `debugger.rs`, `patch.rs`, `keymap.rs`, `macros.rs`, `link.rs`, `netplay.rs`)** : Keymapper visuel + manettes W3C Gamepad API, TAS Studio, Link 2P/4P & **Netplay UDP LAN P2P (`ZML2`)** avec connexion/déconnexion de pair `IP:PORT`, inspecteur CPU / tuiles VRAM 2bpp, patchs `.IPS`, gestion complète (ajout/suppression) des codes triche, watchpoints RAM et Save States (9 slots), rembobinage 60 s (`Rewind`), sauvegardes atomiques `.tmp → fs::rename` et archive portable `.zaimule-pack`.

| Action | Raccourci |
| --- | --- |
| Déplacements (D-Pad) | `ZQSD` ou `Flèches directionnelles` |
| Bouton A / Bouton B | `J` (ou `Espace`) / `K` (ou `Maj gauche`) |
| Gâchettes L / R (GBA / NDS) | `E` / `R` (ou `Q` / `P`) |
| Boutons X / Y (NDS) | `I` / `O` |
| Start / Select | `Entrée` / `N` (ou `Maj droit`) |
| Pause / Reprendre | `F9` (ou `Pause`) |
| Sauvegarde / Chargement rapide (Slot #1) | `F5` / `F8` |
| Plein écran / Capture PNG | `F11` / `F12` |
| Rembobinage temporel (Rewind) | `Retour arrière` (`Backspace`) |

---

## 🚀 Développement & Vérification

```bash
# Formater et vérifier la conformité Clippy des cœurs et du backend
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings

# Exécuter les 391 tests Rust (termboy-core, termboy-gb, termboy-gba, zaimule-core, zaimule-emu)
# Note : sous Linux, la compilation du crate Tauri `zaimule` requiert les en-têtes système WebKitGTK / libsoup-3.0 ;
# la CI exécute le workspace complet sur `windows-latest`.
cargo test -p termboy-core -p termboy-gb -p termboy-gba -p zaimule-core -p zaimule-emu --locked

# Exécuter les 27 tests unitaires JavaScript (catalogue, filtres GB/GBC/Switch, IPC binaire ZMFR v1 & audio)
node --test "tests/ui/*.test.js"
```
