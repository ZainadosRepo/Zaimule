# Zaimule

[![Release](https://img.shields.io/github/v/release/ZainadosRepo/Zaimule)](https://github.com/ZainadosRepo/Zaimule/releases/latest)
[![CI](https://github.com/ZainadosRepo/Zaimule/actions/workflows/ci.yml/badge.svg)](https://github.com/ZainadosRepo/Zaimule/actions/workflows/ci.yml)

Station d'émulation multi-consoles native en **Rust + Tauri 2** réunissant un catalogue unifié et des salons P2P (façon **RomStation** sans publicité ni file d'attente), la surcharge granulaire par jeu et le patching IPS en mémoire vive (façon **Yuzu**), ainsi qu'un éditeur visuel de touches en surimpression, un moteur de macros TAS déterministes et un mode Multi-Instance Câble Link 2 joueurs (façon **BlueStacks**).

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

## ✨ Architecture & Fonctionnalités

- **🦀 Architecture Hybride : Cœurs 100% Rust Natifs + Hôte Libretro C-ABI (`crates/zaimule-emu`)**
  - **Cœurs Rust Natifs compilés dans le binaire** : **Game Boy / Game Boy Color** (CPU Sharp LR35902, PPU 160×144 scanline, registres série `SB`/`SC` `0xFF01`/`0xFF02`), **NES / Famicom** (MOS 6502 + PPU 2C02), et **CHIP-8 / Super-CHIP** (interpréteur 35 opcodes) avec ROMs homebrew jouables embarquées dès le premier lancement.
  - **Pont Libretro C-ABI (`libretro.rs`)** : Convertisseurs de framebuffers (`RGB565`, `XRGB8888` → `RGBA8888`) et registre pour **GBA** (`mgba`), **SNES** (`snes9x`), **Nintendo DS** (`melonds`) et **PlayStation 1** (`swanstation`).
- **⚡ Ingénierie & Overrides par Jeu (Héritage Yuzu & Albion Stats)**
  - **Surcharges par Jeu (`overrides.rs`)** : Résolution hiérarchique `GlobalSettings → Console → GameProfileOverride` (shaders CRT Trinitron, Matrice LCD, Pixel-Perfect, ScaleFX, échelle interne `1x`–`8x`, vitesse `1x`–`4x`).
  - **Patching IPS en Mémoire Vive (`patch.rs`)** : Application à la volée des traductions et hacks `.ips` (blocs standards et RLE) sans jamais altérer la ROM originale sur le disque (CRC32 No-Intro préservé).
  - **Barre de Télémétrie 60 FPS & Débogueur CPU** : Cadence FPS, Frame-Time, registres matériels en direct (`PC`, `SP`, `AF`, `BC`, `DE`, `HL`, `SB`/`SC`) et statut du bus Link.
- **🛠️ Boîte à Outils Power-User (Héritage BlueStacks)**
  - **Visual Keymapper Overlay (`keymap.rs`)** : Superposition sur l'écran des contrôles clavier/manette, zones tactiles NDS et boutons **Turbo/Autofire réglables (5 à 30 Hz)**.
  - **Moteur de Macros TAS Frame-Accurate (`macros.rs`)** : Enregistrement et lecture déterministe de séquences d'entrées compressées par segments.
  - **Multi-Instance Split-Screen 2P & Câble Link Virtuel (`link.rs`)** : Exécution simultanée en lockstep de deux instances reliées par un câble Link virtuel (`262 Kbps`) échangeant les octets série `0xFF01`/`0xFF02`.
- **🔐 Persistance Atomique Anti-Corruption & Rewind 60s (`storage.rs` & `rewind.rs`)**
  - **Sauvegardes Atomiques** : Écriture dans `.tmp` suivie d'un `fs::rename` atomique, rotation automatique des backups batterie `.srm.bak1`..`.srm.bak3` et mise de côté des fichiers JSON endommagés en `.json.bad`.
  - **Rembobinage Temporel (Rewind 60s)** : Tampon circulaire compressé en RLE permettant de rembobiner l'action en direct (`Backspace`) avec vérification CRC32.

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
# Formater et vérifier le code Rust
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings

# Exécuter la suite de tests Rust (catalogue CRC32, patcher IPS, rewind RLE, macros TAS, cœurs GB/NES/CHIP-8, Link Cable, persistance atomique)
cargo test --workspace --locked

# Exécuter la suite de tests unitaires JavaScript (UI ES Modules)
node --test "tests/ui/*.test.js"
```
