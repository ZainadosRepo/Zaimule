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

## ✨ Architecture & Fonctionnalités (`v1.3.0`)

- **🧹 Démarrage Propre & Importation Manuelle (`.zip`, `.7z`, Fichiers & Dossiers)**
  - **Zéro contenu propriétaire inclus & zéro scan automatique** : la bibliothèque démarre vide sans parcourir vos dossiers personnels ; aucune ROM, aucun BIOS propriétaire, aucune clé Switch ni aucun binaire externe n'est distribué avec Zaimule.
  - **Importation directe (`scanner.rs` & `archive.rs`)** : sélecteurs natifs de fichiers ou dossiers (`tauri-plugin-dialog`) et glisser-déposer direct avec extraction streaming des archives **`.zip` / ZIP64** et **`.7z`** vers le cache applicatif local.
- **🦀 Cœurs d'Émulation 100% Rust Natifs (`crates/zaimule-emu` & `vendor/termboy-*`)**
  - **Game Boy & Game Boy Color (`GB` / `GBC`, `zaimule-rust-gb`)** : motorisé par `termboy-gb` + `termboy-core` (MIT) — affichage `160×144` RGBA8888, APU 4 canaux `48 kHz` stéréo, sauvegarde batterie MBC (`.srm`), Save States `"TBSS"` et Câble Link Série `SB`/`SC`.
  - **Game Boy Advance (`GBA`, `zaimule-rust-gba`)** : motorisé par `termboy-gba` + `termboy-core` (MIT) — CPU ARM7TDMI + BIOS HLE SWI/DMA, PPU `240×160` RGBA8888 avec correction couleur LCD, APU `48 kHz` stéréo, sauvegarde SRAM/Flash et Save States `"TBSS"`.
  - **Nintendo DS (`NDS`, `zaimule-rust-nds`)** : motorisé par `nds_core` (`x0cero/nds` MIT) — double CPU **ARM946E-S + ARM7TDMI**, BIOS HLE, déchiffrement Blowfish `key1.bin`, firmware synthétique, double écran `256×384` (avec permutation d'affichage `powcnt1`), **Stylet Tactile** sur l'écran inférieur, **SPU 16 canaux `48 kHz` stéréo** (`PCM8`, `PCM16`, `IMA-ADPCM`, PSG, Bruit), **moteur 3D** (textures compressées `4×4` Format 5 sur Slots 0/2, modes `Decal`/`Toon`/`Highlight` + matrice texture), **PPU 2D complet** (modes Affines/Étendus, Grand Bitmap Mode 6, Bitmap OBJ 1D/2D avec alpha), Flash 512 Ko et Save States RLE `"ZMND"`.
- **🔐 Coffre Nintendo Switch Option A — Torzu & Fritchii (`external.rs`)**
  - **Séparation Switch 1 & Switch 2** : `SWITCH` (`.nsp`, `.xci`, `.nsz`, `.xcz`, `.nro`, `.nso`) cible par défaut **Torzu**, tandis que `SWITCH2` (`.nx`, `.nup`) cible par défaut **Fritchii / Lichii**, avec **surcharge par jeu** (`Auto` / `Torzu` / `Fritchii`).
  - **Coffre isolé (`<app_config_dir>/switch`)** : importation par dossier, exécutable ou archive `.zip`/`.7z` de chaque build, plus importation manuelle de vos clés `prod.keys` et `title.keys`.
  - **Synchronisation & télémétrie de session** : copie automatique des clés vers `<engine_dir>/user/keys` avant exécution (`-f -g <rom>`), purge dédiée du cache shaders et comptabilisation du temps de jeu à la fermeture (`switch-session-ended`).
- **⚡ Optimisations Moteur, IPC Binaire & Audio Temps Réel (`session.rs`, `viewport_ipc.js`, `audio_worklet.js`)**
  - **Protocole Binaire `"ZMFR"` v1 (`step_viewport_binary`)** : transport via `tauri::ipc::Response` transférant directement en binaire les tampons pixels RGBA et les échantillons audio PCM `f32` (sans sérialisation JSON de pixels ni d'audio).
  - **Audio Faible Latence (`AudioWorkletNode`)** : processeur audio dédié (`ui/audio_worklet.js`) avec tampon circulaire et rééchantillonnage linéaire stéréo, plus repli `AudioBufferSourceNode` et cache `ImageData` réutilisable dans le viewport.
  - **Vitesse Turbo & Profils Cargo (`Cargo.toml`)** : multiplicateur d'exécution `1x → 2x → 4x → 8x` (`speed_mult`) et `opt-level = 2` activé sur les cœurs et dépendances dès le profil `dev` (en plus de `lto = true` en `release`).
- **🎨 Upscalers, Atelier & Persistance Atomique**
  - **Upscalers & Shaders (`ui/upscaler.js`)** : **Scale2x / EPX**, **xBRZ 2x**, **CRT Bloom** et 5 modes visuels (CRT Scanlines, Trinitron, Game Boy DMG, LCD, Pixel-Perfect).
  - **Outils Power-User (`cheats.rs`, `debugger.rs`, `patch.rs`, `keymap.rs`, `macros.rs`, `link.rs`, `netplay.rs`)** : Keymapper visuel + manettes W3C Gamepad API, TAS Studio & Link 2P/4P / salons P2P UDP (`ZML2`), inspecteur CPU / tuiles VRAM 2bpp, patchs `.IPS`, Watchpoints RAM, rembobinage 60 s (`Rewind`), sauvegardes atomiques `.tmp → fs::rename` et archive portable `.zaimule-pack`.

| Action | Raccourci |
| --- | --- |
| Déplacements (D-Pad) | `ZQSD` ou `Flèches directionnelles` |
| Bouton A / Bouton B | `J` (ou `Espace`) / `K` (ou `Maj`) |
| Start / Select | `Entrée` / `Retour arrière` |
| Sauvegarde rapide (Save State Slot #1) | `F5` |
| Chargement rapide (Load State Slot #1) | `F8` |
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
