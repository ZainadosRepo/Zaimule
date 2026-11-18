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

## ✨ Architecture & Fonctionnalités (`v1.1.0`)

- **🦀 11 Cœurs 100% Rust Natifs + Hôte Dynamique Libretro C-ABI (`crates/zaimule-emu`)**
  - **Game Boy & Game Boy Color (`zaimule-rust-gb`)** : CPU Sharp LR35902 (256 opcodes `0xCB`), MMU MBC1/MBC3/MBC5 avec **Horloge Temps Réel (RTC `0x08..=0x0C` & protocole de Latch `0x00 → 0x01`)**, PPU 160×144 scanline, APU 4 canaux et Câble Link Série `SB`/`SC` (`0xFF01`/`0xFF02`).
  - **Game Boy Advance (`zaimule-rust-gba`)** : CPU ARM7TDMI 32-bit + Thumb 16-bit, PPU Mode 3/4 `240×160` RGB555, FlashRAM 64 Ko et Multi-SIO 4 joueurs.
  - **NES / Famicom (`zaimule-rust-nes`)** : CPU MOS 6502, PPU 2C02 `256×240` et commutation de banques multi-mappers (**NROM 0**, **MMC1 / SxROM 1**, **UxROM 2**, **CNROM 3**, **MMC3 / TxROM 4** avec compteur IRQ scanline).
  - **Super Nintendo (`zaimule-rust-snes`)** : CPU Ricoh 5A22 / WDC 65C816 16-bit, PPU `256×224` avec matrice de perspective **Mode 7** (`M7A`..`M7D`), DMA et synthèse audio DSP SPC700.
  - **Nintendo 64 (`zaimule-rust-n64`)** : CPU MIPS VR4300 64-bit @ 93.75 MHz, Reality Coprocessor (RSP/RDP) avec **Z-Buffer 16-bit** `320×240` et Controller Pak (`.mpk`).
  - **Sega Mega Drive / Genesis (`zaimule-rust-md`)** : CPU Motorola 68000 16/32-bit, VDP `320×224` Dual-Plane + Sprites, synthétiseur **FM Yamaha YM2612 6 canaux** + PSG SN76489 et FeRAM 64 Ko.
  - **Nintendo DS Dual-Screen (`zaimule-rust-nds`)** : Architecture double processeur **ARM946E-S + ARM7TDMI** synchronisée par FIFO IPC, double écran `256×384` et **Stylet Tactile interactif** à la souris.
  - **PlayStation 1 (`zaimule-rust-ps1`)** : CPU MIPS R3000A 32-bit, coprocesseur géométrique **GTE (`COP2`)** avec projection 3D **PGXP Subpixel** anti-tremblement `320×240` et Carte Mémoire 128 Ko (`.mcd`).
  - **PlayStation Portable (`zaimule-rust-psp`)** : CPU MIPS R4000 **Allegrex 32-bit + Vector FPU (`VFPU` `VADD.Q`)** @ 333 MHz, Graphics Engine (`GU`) Widescreen **16:9 `480×272`**, Memory Stick PRO Duo 64 Ko (`.msd`) et Ad-Hoc WLAN.
  - **CHIP-8 / Super-CHIP (`zaimule-rust-chip8`)** : Interpréteur déterministe 35 opcodes avec support d'import de ROMs locales.
  - **Pont Libretro C-ABI (`libretro.rs`)** : Inspecteur PE/ELF/Mach-O, convertisseurs `0RGB1555`, `RGB565` & `XRGB8888 → RGBA8888` et registre pour **mGBA**, **Snes9x**, **melonDS**, **SwanStation**, **Mupen64Plus-Next**, **Genesis Plus GX** et **PPSSPP**.
- **⚡ Ingénierie, Upscaling & ROM Hacking Studio (Héritage Yuzu & Mesen)**
  - **Upscalers Temps Réel (`ui/upscaler.js`)** : **Scale2x / EPX**, **xBRZ 2x** (vectorisation directionnelle par distance luma YUV) et **CRT Phosphor Bloom**, combinables avec 5 shaders matériels (CRT Scanlines, Trinitron Courbé, Game Boy DMG, Matrice LCD, Pixel-Perfect).
  - **Profils d'Overclock CPU & PGXP par Jeu (`overrides.rs`)** : Cadence CPU (`100% Natif`, `150%`, `200% Turbo`), précision sous-pixel PGXP 3D et résolution hiérarchique `GlobalSettings → Console → GameProfileOverride`.
  - **ROM Hacking Studio, Créateur `.IPS` & Hub Communautaire (`debugger.rs` & `patch.rs`)** : Éditeur de tuiles VRAM 2bpp interactif (`128×64`), désassembleur multi-architectures (LR35902, ARM7TDMI, 65C816, MIPS R3000A/VR4300/Allegrex VFPU, Motorola 68000) avec points d'arrêt, création de patchs `.IPS` par diff ROM et installation 1-clic de mods communautaires en RAM.
  - **Watchpoints RAM & Inspecteur Hexadécimal SRAM (`cheats.rs` & `storage.rs`)** : Scanner RAM Cheat-Engine, codes GameShark / Action Replay, **Watchpoints RAM avec gel/épinglage de valeur en direct** (`0xC110=7F`), éditeur hexadécimal SRAM/Carte Mémoire et transfert **J1 ➔ J2**.
- **🛠️ Boîte à Outils Power-User, TAS Studio & Tournois P2P (Héritage BlueStacks & RomStation)**
  - **Visual Keymapper & Mode Salon Big Picture (`keymap.rs`)** : Superposition glisser-déposer des touches, boutons **Turbo 20 Hz**, support manettes W3C Gamepad API avec vibrations Dual-Rumble et navigation 100% manette.
  - **TAS Studio Timeline & Classements Fantômes (`macros.rs`)** : Éditeur segment par segment (RLE) des séquences TAS avec bascule de boutons à la volée, export de records déterministes vérifiés par CRC32 et mode **Défi Fantôme TAS en direct**.
  - **Multi-Instance 2P / 4P Split-Screen & Arbre de Tournoi (`link.rs` & `netplay.rs`)** : Câble Link 2P et **Quad-Link 4P (grille 2×2)** en lockstep, salons P2P Rollback UDP (`ZML2`) avec découverte **STUN UDP Hole-Punching**, chat avec partage 1-clic de Save State, et arbre de tournoi à élimination directe avec mode Spectateur.
- **🔐 Persistance Atomique Anti-Corruption, Cloud Pack & Rewind 60s (`storage.rs` & `rewind.rs`)**
  - **Sauvegardes Atomiques & Archive Portable `.zaimule-pack`** : Écriture `.tmp → fs::rename`, rotation automatique `.srm.bak1`..`.srm.bak3`, isolation des JSON corrompus en `.json.bad` et synchronisation Cloud en 1 fichier `.zaimule-pack` (SRAM RLE + 6 Save States + Cheats + Macros TAS + Keymapper).
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

# Exécuter les 56 tests unitaires et d'intégration Rust (11 cœurs natifs, mappers NES, RTC MBC3, Link 2P/4P, TAS Timeline, Watchpoints, STUN/ZML2, persistance atomique)
cargo test --workspace --locked

# Exécuter les 21 tests unitaires JavaScript (UI ES Modules : Scale2x/xBRZ/Bloom, APU FM/SPC700, Keymapper, Recorder, Bibliothèque)
node --test "tests/ui/*.test.js"
```
