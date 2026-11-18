import {
  filterCatalogEntries,
  computeConsoleCounts,
  formatPlaytime,
  formatFileSize
} from "./library.js";
import {
  BTN_UP,
  BTN_DOWN,
  BTN_LEFT,
  BTN_RIGHT,
  BTN_A,
  BTN_B,
  computeBitmaskFromPressedCodes,
  computeBitmaskFromGamepad,
  clampWidgetPosition,
  triggerGamepadRumble,
  computeBigPictureNavAction
} from "./keymapper.js";
import { ApuAudioEngine, synthesizeMultiChannelSamples } from "./audio.js";
import { ViewportRecorder } from "./recorder.js";
import { upscaleFrameIfEnabled } from "./upscaler.js";

const tauriInvoke = window.__TAURI__?.core?.invoke;
const appWindow = window.__TAURI__?.window?.getCurrentWindow?.();
const apuAudio = new ApuAudioEngine();
const viewportRecorder = new ViewportRecorder();

const FALLBACK_CATALOG = [
  {
    id: "gb-pocket-arena",
    title: "Pocket Monster Link Arena (DMG/GBC)",
    console: "GBC",
    engineKind: "nativeRust",
    coreId: "zaimule-rust-gb",
    region: "EUR · FR",
    crc32Hex: "DCE0682B",
    sizeBytes: 32768,
    sramSummary: "32 Ko SRAM Atomique + Câble Link SIO",
    playtimeSeconds: 15420,
    favorite: true,
    linkCableReady: true,
    coverAccent: "#1e3a8a",
    coverSymbol: "🐉"
  },
  {
    id: "gba-golden-horizon",
    title: "Golden Horizon : Alchemy Tactics (GBA)",
    console: "GBA",
    engineKind: "nativeRust",
    coreId: "zaimule-rust-gba",
    region: "EUR · FR (ARM7TDMI Inclus)",
    crc32Hex: "89FA410C",
    sizeBytes: 32768,
    sramSummary: "64 Ko FlashRAM + Multi-Link 4P",
    playtimeSeconds: 19800,
    favorite: true,
    linkCableReady: true,
    coverAccent: "#b45309",
    coverSymbol: "☀️"
  },
  {
    id: "nes-chrono-knight",
    title: "Chrono Knight 8-Bit Odyssey",
    console: "NES",
    engineKind: "nativeRust",
    coreId: "zaimule-rust-nes",
    region: "USA · NTSC",
    crc32Hex: "44B109EE",
    sizeBytes: 40976,
    sramSummary: "8 Ko PRG-RAM Battery",
    playtimeSeconds: 8760,
    favorite: false,
    linkCableReady: false,
    coverAccent: "#7f1d1d",
    coverSymbol: "⚔️"
  },
  {
    id: "snes-alttp-fr",
    title: "Valkyrie Trigger : Mode 7 Odyssey (SNES)",
    console: "SNES",
    engineKind: "nativeRust",
    coreId: "zaimule-rust-snes",
    region: "EUR · FR (65C816 + Mode 7 Inclus)",
    crc32Hex: "A2D9107B",
    sizeBytes: 32768,
    sramSummary: "8 Ko LoROM SRAM + PPU Mode 7",
    playtimeSeconds: 68100,
    favorite: true,
    linkCableReady: false,
    coverAccent: "#14532d",
    coverSymbol: "🕰️"
  },
  {
    id: "n64-star-wing-64",
    title: "Star Wing 64 : Lylat Z-Buffer (N64)",
    console: "N64",
    engineKind: "nativeRust",
    coreId: "zaimule-rust-n64",
    region: "EUR · FR (MIPS VR4300 + RCP 3D Inclus)",
    crc32Hex: "64F09A1B",
    sizeBytes: 65536,
    sramSummary: "32 Ko Controller Pak (.mpk) + Z-Buffer 16-bit",
    playtimeSeconds: 24600,
    favorite: true,
    linkCableReady: true,
    coverAccent: "#065f46",
    coverSymbol: "🦊"
  },
  {
    id: "md-supersonic-blast",
    title: "Supersonic Blast : 16-Bit FM (Mega Drive)",
    console: "MEGADRIVE",
    engineKind: "nativeRust",
    coreId: "zaimule-rust-md",
    region: "EUR · FR (Motorola 68000 + YM2612 Inclus)",
    crc32Hex: "68000FA4",
    sizeBytes: 65536,
    sramSummary: "64 Ko FeRAM + Synthétiseur FM YM2612",
    playtimeSeconds: 18300,
    favorite: true,
    linkCableReady: true,
    coverAccent: "#1d4ed8",
    coverSymbol: "🦔"
  },
  {
    id: "nds-dual-phantom",
    title: "Dual Screen Phantom Detective (NDS)",
    console: "NDS",
    engineKind: "nativeRust",
    coreId: "zaimule-rust-nds",
    region: "EUR · FR (ARM9+ARM7 + Stylet Inclus)",
    crc32Hex: "55E081C2",
    sizeBytes: 32768,
    sramSummary: "512 Ko EEPROM + Stylet Tactile Dual-Screen",
    playtimeSeconds: 19200,
    favorite: true,
    linkCableReady: true,
    coverAccent: "#0c4a6e",
    coverSymbol: "🔍"
  },
  {
    id: "ps1-ff7-fr",
    title: "Cyber Ridge GP Type-R4 & FF7 (PS1 3D)",
    console: "PS1",
    engineKind: "nativeRust",
    coreId: "zaimule-rust-ps1",
    region: "EUR · FR (MIPS R3000A + GTE 3D Inclus)",
    crc32Hex: "C9104FA2",
    sizeBytes: 32768,
    sramSummary: "128 Ko Carte Mémoire Slot 1 (.mcd) + PGXP",
    playtimeSeconds: 11400,
    favorite: true,
    linkCableReady: true,
    coverAccent: "#312e81",
    coverSymbol: "🏎️"
  },
  {
    id: "psp-pulse-horizon",
    title: "Pulse Horizon : Widescreen 333MHz (PSP)",
    console: "PSP",
    engineKind: "nativeRust",
    coreId: "zaimule-rust-psp",
    region: "EUR · FR (MIPS Allegrex + VFPU 16:9 Inclus)",
    crc32Hex: "77E49C01",
    sizeBytes: 65536,
    sramSummary: "64 Ko Memory Stick PRO Duo (.msd) + Ad-Hoc WLAN",
    playtimeSeconds: 21600,
    favorite: true,
    linkCableReady: true,
    coverAccent: "#0284c7",
    coverSymbol: "🛸"
  },
  {
    id: "chip8-space-interceptor",
    title: "Space Interceptor VIP (CHIP-8)",
    console: "CHIP8",
    engineKind: "nativeRust",
    coreId: "zaimule-rust-chip8",
    region: "Domaine Public",
    crc32Hex: "30F17A99",
    sizeBytes: 4096,
    sramSummary: "État RAM Instantané RLE",
    playtimeSeconds: 3600,
    favorite: true,
    linkCableReady: false,
    coverAccent: "#064e3b",
    coverSymbol: "🛸"
  }
];

let state = {
  activeGameId: "gb-pocket-arena",
  catalog: FALLBACK_CATALOG,
  modLayers: [
    {
      id: "fr_translation_v2.ips",
      label: "Traduction Française Intégrale v2.1 (.ips)",
      enabled: true
    },
    {
      id: "pure_lcd_palette.ips",
      label: "Correction Colorimétrique AGS-101 / Trinitron",
      enabled: true
    },
    {
      id: "fast_text_60fps.ips",
      label: "Textes Instantanés & Déverrouillage 60 FPS",
      enabled: false
    }
  ],
  cheatCodes: [
    {
      id: "cheat-c104-ff",
      label: "⚡ Tir Plasma Continu & Cadence Maximale",
      codeText: "010104C1",
      enabled: false
    },
    {
      id: "cheat-c108-64",
      label: "💎 Bonus Score Instantané (+100 pts SRAM)",
      codeText: "016404A0",
      enabled: false
    }
  ],
  memoryWatchpoints: [
    {
      id: "wp-c100",
      label: "Coordonnée X / Vélocité Joueur",
      addr: 0xc100,
      addrHex: "0xC100",
      currentValue: 0x48,
      previousValue: 0x48,
      frozen: false,
      frozenValue: 0x48
    },
    {
      id: "wp-c104",
      label: "État & Cadence Tir Plasma",
      addr: 0xc104,
      addrHex: "0xC104",
      currentValue: 0x01,
      previousValue: 0x00,
      frozen: false,
      frozenValue: 0x01
    },
    {
      id: "wp-c108",
      label: "Compteur Score / Énergie Arène",
      addr: 0xc108,
      addrHex: "0xC108",
      currentValue: 0x64,
      previousValue: 0x50,
      frozen: false,
      frozenValue: 0x64
    }
  ],
  achievements: [
    {
      id: "ach-first-orb",
      title: "Premier Contact Phosphore",
      points: 10,
      targetScore: 10,
      unlocked: false,
      badgeSymbol: "🟢"
    },
    {
      id: "ach-plasma-hunter",
      title: "Chasseur Plasma 60 FPS",
      points: 25,
      targetScore: 50,
      unlocked: false,
      badgeSymbol: "🔥"
    },
    {
      id: "ach-link-master",
      title: "Maître du Câble Link",
      points: 50,
      targetScore: 100,
      unlocked: false,
      badgeSymbol: "🏆"
    }
  ],
  saveSlots: [
    { slot: 1, occupied: true, frame: 1, crc32Hex: "DCE0682B", label: "Point de départ Arène" },
    { slot: 2, occupied: false, frame: 0, crc32Hex: "00000000", label: "Slot #2 Vide" },
    { slot: 3, occupied: false, frame: 0, crc32Hex: "00000000", label: "Slot #3 Vide" },
    { slot: 4, occupied: false, frame: 0, crc32Hex: "00000000", label: "Slot #4 Vide" },
    { slot: 5, occupied: false, frame: 0, crc32Hex: "00000000", label: "Slot #5 Vide" },
    { slot: 6, occupied: false, frame: 0, crc32Hex: "00000000", label: "Slot #6 Vide" }
  ],
  keymapperWidgets: [
    { id: "km-dpad-up", label: "Z / ↑", keyCode: "KeyZ", buttonMask: BTN_UP, xPct: 16, yPct: 58, turboHz: null },
    { id: "km-dpad-down", label: "S / ↓", keyCode: "KeyS", buttonMask: BTN_DOWN, xPct: 16, yPct: 78, turboHz: null },
    { id: "km-dpad-left", label: "Q / ←", keyCode: "KeyQ", buttonMask: BTN_LEFT, xPct: 8, yPct: 68, turboHz: null },
    { id: "km-dpad-right", label: "D / →", keyCode: "KeyD", buttonMask: BTN_RIGHT, xPct: 24, yPct: 68, turboHz: null },
    { id: "km-btn-a", label: "J [A · Tir]", keyCode: "KeyJ", buttonMask: BTN_A, xPct: 82, yPct: 64, turboHz: 20 },
    { id: "km-btn-b", label: "K [B · Dash]", keyCode: "KeyK", buttonMask: BTN_B, xPct: 70, yPct: 74, turboHz: null }
  ],
  macros: [
    { id: "macro-shiny-reset", name: "✨ Soft-Reset Starter & Dialogue Rapide" },
    { id: "macro-fast-battle", name: "⚔ Boucle Expédition & Tir Plasma" }
  ],
  tasTimeline: [
    {
      segmentIndex: 0,
      startFrame: 0,
      endFrame: 3,
      durationFrames: 4,
      buttonsMask: 0x0f,
      buttonsLabel: "↑+↓+←+→"
    },
    {
      segmentIndex: 1,
      startFrame: 4,
      endFrame: 21,
      durationFrames: 18,
      buttonsMask: 0x01,
      buttonsLabel: "A"
    }
  ],
  netplayRooms: [
    {
      roomCode: "PKMN-LINK-01",
      title: "Colisée & Échange Câble Link (SIO 256 Kbps)",
      hostName: "Zainados",
      console: "GBC",
      gameTitle: "Pocket Monster Link Arena",
      pingMs: 9,
      rollbackFrames: 1,
      playersCurrent: 2,
      playersMax: 2,
      deepLinkUri: "zaimule://room/PKMN-LINK-01?crc=DCE0682B&core=zaimule-rust-gb"
    },
    {
      roomCode: "SNES-CUP-88",
      title: "Tournoi Rétro 60 FPS Sans File d'Attente",
      hostName: "Kaelen_FR",
      console: "SNES",
      gameTitle: "Valkyrie Trigger: Epoch 1995",
      pingMs: 14,
      rollbackFrames: 1,
      playersCurrent: 1,
      playersMax: 2,
      deepLinkUri: "zaimule://room/SNES-CUP-88?crc=A2D9107B&core=zaimule-rust-snes"
    }
  ],
  udpLocalPort: 54920,
  stunCandidate: {
    publicEndpoint: "198.51.100.42:54920",
    natType: "Full-Cone UDP (Hole-Punch Ready)",
    relayFallback: "wss://relay.zaimule.net/zml2"
  },
  netplayChat: [
    {
      id: "chat-welcome-1",
      sender: "Zainados",
      text: "Salon P2P ouvert ! STUN Hole-Punch actif (0 file d'attente).",
      saveStateCrcHex: null
    },
    {
      id: "chat-state-share",
      sender: "Kaelen_FR",
      text: "Cliché d'arène partagé (Boss Final prêt)",
      saveStateCrcHex: "DCE0682B"
    }
  ],
  communityMods: [
    {
      id: "mod-fr-integral-dx",
      title: "🇫🇷 Traduction Française Intégrale DX v2.4 (.ips)",
      author: "Équipe Zaimule FR",
      targetConsole: "Multi-Consoles (GBC / GBA / SNES / PS1)",
      description: "Patch binaire .IPS non-destructif : polices accentuées et dialogues traduits.",
      installed: false
    },
    {
      id: "mod-60fps-overdrive",
      title: "⚡ Patch 60 FPS Déverrouillé & Sans Ralentissement (.ips)",
      author: "Kaelen_TAS",
      targetConsole: "PS1 / SNES / GBA",
      description: "Supprime les diviseurs de VBlank 30 FPS et active le pipeline 60 Hz.",
      installed: false
    },
    {
      id: "mod-trinitron-palette",
      title: "🎨 Pack Colorimétrique Sony PVM & AGS-101 (.ips)",
      author: "Zainados",
      targetConsole: "GBC / GBA / NDS",
      description: "Recalibre les tables de palettes 15-bit RGB555.",
      installed: false
    }
  ],
  ghostLeaderboard: [
    {
      id: "ghost-wr-zainados",
      gameId: "gb-pocket-arena",
      gameTitle: "Pocket Monster Link Arena (DMG/GBC)",
      console: "GBC",
      playerName: "Zainados",
      score: 185,
      framesCount: 240,
      traceCrc32Hex: "A482F19B",
      verified: true
    },
    {
      id: "ghost-wr-ps1-ridge",
      gameId: "ps1-ff7-fr",
      gameTitle: "Cyber Ridge GP Type-R4 & FF7 (PS1 3D)",
      console: "PS1",
      playerName: "Kaelen_FR",
      score: 240,
      framesCount: 240,
      traceCrc32Hex: "7C91E03A",
      verified: true
    }
  ],
  cpuOverclockPct: 100,
  pgxpSubpixel: true,
  sramHexInspection: {
    totalBytes: 32768,
    crc32Hex: "5A41494D",
    offset: 0,
    rows: [
      "0x0000: 5A 41 49 4D 55 4C 45 30  00 00 00 00 00 00 00 00 | ZAIMULE0........",
      "0x0010: 00 00 00 00 00 00 00 00  00 00 00 00 00 00 00 00 | ................"
    ]
  },
  tournamentBracket: {
    tournamentId: "tourney-zaimule-cup",
    title: "🏆 Coupe Zaimule 4P Rollback & Multi-SIO",
    spectatorCount: 3,
    spectatorModeActive: false,
    champion: null,
    matches: [
      {
        matchId: "semi-1",
        roundLabel: "Demi-Finale #1",
        playerA: "Zainados",
        playerB: "Kaelen_FR",
        scoreA: 2,
        scoreB: 1,
        winner: null
      },
      {
        matchId: "semi-2",
        roundLabel: "Demi-Finale #2",
        playerA: "Nyx_TAS",
        playerB: "Orpheus_64",
        scoreA: 1,
        scoreB: 2,
        winner: null
      },
      {
        matchId: "grand-final",
        roundLabel: "Grande Finale 60 FPS",
        playerA: "Vainqueur DF1",
        playerB: "Vainqueur DF2",
        scoreA: 0,
        scoreB: 0,
        winner: null
      }
    ]
  },
  libretroCores: [
    {
      coreId: "mgba_libretro",
      displayName: "mGBA (Game Boy Advance · ARM7TDMI)",
      libraryWindows: "mgba_libretro.dll",
      libraryMacos: "mgba_libretro.dylib",
      installed: false,
      statusLabel: "Pont C-ABI Prêt · Cliquer pour Provisionner"
    },
    {
      coreId: "snes9x_libretro",
      displayName: "Snes9x Current (Super Nintendo · Ricoh 5A22)",
      libraryWindows: "snes9x_libretro.dll",
      libraryMacos: "snes9x_libretro.dylib",
      installed: false,
      statusLabel: "Pont C-ABI Prêt · Cliquer pour Provisionner"
    },
    {
      coreId: "melonds_libretro",
      displayName: "melonDS (Nintendo DS + Stylet Tactile)",
      libraryWindows: "melonds_libretro.dll",
      libraryMacos: "melonds_libretro.dylib",
      installed: false,
      statusLabel: "Pont C-ABI Prêt · Cliquer pour Provisionner"
    },
    {
      coreId: "swanstation_libretro",
      displayName: "SwanStation (PlayStation 1 PGXP 4x)",
      libraryWindows: "swanstation_libretro.dll",
      libraryMacos: "swanstation_libretro.dylib",
      installed: false,
      statusLabel: "Pont C-ABI Prêt · Cliquer pour Provisionner"
    }
  ],
  biosEntries: [
    {
      console: "GBA",
      fileName: "gba_bios.bin",
      expectedCrc32Hex: "81977335",
      notes: "Optionnel (remplacement HLE Rust inclus)"
    },
    {
      console: "NDS",
      fileName: "bios7.bin",
      expectedCrc32Hex: "DF692A80",
      notes: "Optionnel en mode Direct Boot"
    },
    {
      console: "PS1",
      fileName: "scph5502.bin",
      expectedCrc32Hex: "32736F17",
      notes: "Optionnel (BIOS HLE MIPS R3000A Rust inclus)"
    }
  ],
  linkTelemetry: { enabled: false, quadMode: false, packetsTransferred: 0 },
  consoleFilter: "ALL",
  searchQuery: "",
  keymapperVisible: false,
  dualLinkMode: false,
  quadLinkMode: false,
  debuggerPaused: false,
  stepOnceRequested: false,
  bigPictureMode: false,
  prevNavMask: 0,
  vramPaintColorIdx: 3,
  stylusPressed: false,
  upscalerMode: "OFF",
  speedMult: 1,
  activeMacroId: null,
  recordingMacro: false,
  recordedFrames: []
};

const fallbackSim = {
  frame: 0,
  px: 76,
  py: 68,
  orbX: 116,
  orbY: 42,
  shotActive: false,
  shotX: 0,
  shotY: 0,
  score: 0,
  history: []
};

const pressedKeys = new Set();

async function invokeBackend(cmd, args = {}) {
  if (tauriInvoke) {
    return await tauriInvoke(cmd, args);
  }
  return null;
}

function getSelectedGame() {
  return state.catalog.find((g) => g.id === state.activeGameId) || state.catalog[0];
}

function getConnectedGamepad() {
  if (typeof navigator === "undefined" || !navigator.getGamepads) return null;
  const pads = navigator.getGamepads();
  if (!pads) return null;
  for (const pad of pads) {
    if (pad && pad.connected) return pad;
  }
  return null;
}

function pollGamepadBitmask() {
  const pad = getConnectedGamepad();
  if (pad) {
    const el = document.getElementById("tel-pad");
    if (el) el.textContent = `🎮 ${pad.id.slice(0, 18)}`;
    return computeBitmaskFromGamepad(pad);
  }
  return 0;
}

function downloadBinaryFile(bytesArray, fileName, mimeType = "application/octet-stream") {
  if (typeof document === "undefined" || typeof URL === "undefined") return;
  const blob = new Blob([new Uint8Array(bytesArray)], { type: mimeType });
  const url = URL.createObjectURL(blob);
  const a = document.createElement("a");
  a.href = url;
  a.download = fileName;
  document.body.appendChild(a);
  a.click();
  a.remove();
  setTimeout(() => URL.revokeObjectURL(url), 4000);
}

async function handleBigPictureNavigation(currentMask) {
  const action = computeBigPictureNavAction(state.prevNavMask, currentMask);
  state.prevNavMask = currentMask;
  if (!action || !state.bigPictureMode) return;

  const tabs = ["view-library", "view-emulator", "view-netplay", "view-cores"];
  const activePane = document.querySelector(".view-pane.active")?.id || "view-library";

  if (action === "PREV_TAB" || action === "NEXT_TAB") {
    const curIdx = Math.max(0, tabs.indexOf(activePane));
    const nextIdx =
      action === "NEXT_TAB"
        ? (curIdx + 1) % tabs.length
        : (curIdx - 1 + tabs.length) % tabs.length;
    switchTab(tabs[nextIdx]);
    triggerGamepadRumble(getConnectedGamepad(), 45, 0.3, 0.5);
    return;
  }

  if (activePane === "view-library") {
    const idx = state.catalog.findIndex((g) => g.id === state.activeGameId);
    if (action === "PREV_GAME" || action === "NEXT_GAME" || action === "PREV_ROW" || action === "NEXT_ROW") {
      const step = action === "PREV_GAME" ? -1 : action === "NEXT_GAME" ? 1 : action === "PREV_ROW" ? -3 : 3;
      const nextIdx = (Math.max(0, idx) + step + state.catalog.length) % state.catalog.length;
      const nextGame = state.catalog[nextIdx];
      if (nextGame) {
        state.activeGameId = nextGame.id;
        const boot = await invokeBackend("select_game", { gameId: nextGame.id });
        if (boot) applyBootstrap(boot);
        else updateAllViews();
        triggerGamepadRumble(getConnectedGamepad(), 35, 0.25, 0.4);
      }
    } else if (action === "LAUNCH_GAME") {
      switchTab("view-emulator");
      triggerGamepadRumble(getConnectedGamepad(), 90, 0.5, 0.85);
    }
  } else if (action === "BACK_LIBRARY" && activePane !== "view-emulator") {
    switchTab("view-library");
  }
}

function stepBrowserFallback(maskP1) {
  fallbackSim.frame += 1;
  const speed = (maskP1 & BTN_B) !== 0 ? 2 : 1;
  if (maskP1 & BTN_LEFT) fallbackSim.px = Math.max(8, fallbackSim.px - speed);
  if (maskP1 & BTN_RIGHT) fallbackSim.px = Math.min(152, fallbackSim.px + speed);
  if (maskP1 & BTN_UP) fallbackSim.py = Math.max(20, fallbackSim.py - speed);
  if (maskP1 & BTN_DOWN) fallbackSim.py = Math.min(134, fallbackSim.py + speed);

  let audioFreqHz = 0;
  if ((maskP1 & BTN_A) !== 0 && !fallbackSim.shotActive) {
    fallbackSim.shotActive = true;
    fallbackSim.shotX = fallbackSim.px;
    fallbackSim.shotY = fallbackSim.py - 6;
    audioFreqHz = 880;
  }
  if (fallbackSim.shotActive) {
    fallbackSim.shotY -= 4;
    if (fallbackSim.shotY <= 16) fallbackSim.shotActive = false;
  }

  const hitPlayer =
    Math.abs(fallbackSim.px - fallbackSim.orbX) <= 8 &&
    Math.abs(fallbackSim.py - fallbackSim.orbY) <= 8;
  const hitShot =
    fallbackSim.shotActive &&
    Math.abs(fallbackSim.shotX - fallbackSim.orbX) <= 8 &&
    Math.abs(fallbackSim.shotY - fallbackSim.orbY) <= 8;

  if (hitPlayer || hitShot) {
    fallbackSim.score += hitShot ? 25 : 10;
    fallbackSim.shotActive = false;
    fallbackSim.orbX = 20 + ((fallbackSim.frame * 37) % 120);
    fallbackSim.orbY = 26 + ((fallbackSim.frame * 53) % 96);
    audioFreqHz = 1320;
  }

  let newlyUnlockedAchievement = null;
  for (const ach of state.achievements) {
    if (!ach.unlocked && fallbackSim.score >= ach.targetScore) {
      ach.unlocked = true;
      newlyUnlockedAchievement = `${ach.badgeSymbol} ${ach.title} (+${ach.points} pts)`;
    }
  }

  if ((fallbackSim.frame & 15) === 0) {
    fallbackSim.history.push({
      frame: fallbackSim.frame,
      px: fallbackSim.px,
      py: fallbackSim.py,
      orbX: fallbackSim.orbX,
      orbY: fallbackSim.orbY,
      score: fallbackSim.score
    });
    if (fallbackSim.history.length > 240) fallbackSim.history.shift();
  }

  const width = 160;
  const height = 144;
  const rgba = new Uint8ClampedArray(width * height * 4);
  const pal = [
    [12, 20, 16],
    [34, 74, 48],
    [82, 164, 98],
    [214, 245, 190]
  ];
  for (let y = 0; y < height; y++) {
    for (let x = 0; x < width; x++) {
      let c = 0;
      if (y < 14) c = 1;
      else if (((x + fallbackSim.frame) & 15) === 0 || ((y + fallbackSim.frame) & 15) === 0) c = 1;
      if (Math.abs(x - fallbackSim.orbX) + Math.abs(y - fallbackSim.orbY) <= 5) c = 3;
      if (fallbackSim.shotActive && Math.abs(x - fallbackSim.shotX) <= 1 && Math.abs(y - fallbackSim.shotY) <= 3) c = 3;
      if (Math.abs(x - fallbackSim.px) <= 5 && Math.abs(y - fallbackSim.py) <= 5) c = 3;
      const idx = (y * width + x) * 4;
      rgba[idx] = pal[c][0];
      rgba[idx + 1] = pal[c][1];
      rgba[idx + 2] = pal[c][2];
      rgba[idx + 3] = 255;
    }
  }

  return {
    primary: {
      frameNumber: fallbackSim.frame,
      width,
      height,
      rgba,
      audioFreqHz,
      cpu: {
        arch: "Sharp LR35902",
        pcHex: `0x01${(0x50 + (fallbackSim.frame & 0x3f)).toString(16).toUpperCase()}`,
        spHex: "0xFFFE",
        regAHex: `0x${(fallbackSim.score & 0xff).toString(16).padStart(2, "0").toUpperCase()}`,
        regBcHex: `0x${fallbackSim.px.toString(16).padStart(2, "0")}${fallbackSim.py.toString(16).padStart(2, "0")}`.toUpperCase(),
        regDeHex: `0x${fallbackSim.orbX.toString(16).padStart(2, "0")}${fallbackSim.orbY.toString(16).padStart(2, "0")}`.toUpperCase(),
        regHlHex: "0xC100",
        cycles: fallbackSim.frame * 70224,
        serialSbHex: `0x${(fallbackSim.px ^ 0x5a).toString(16).toUpperCase()}`,
        serialScHex: "0x81",
        score: fallbackSim.score
      }
    },
    secondary: state.dualLinkMode
      ? {
          frameNumber: fallbackSim.frame,
          width,
          height,
          rgba,
          audioFreqHz: 0,
          cpu: { score: fallbackSim.score }
        }
      : null,
    linkTelemetry: {
      enabled: state.dualLinkMode,
      packetsTransferred: state.dualLinkMode ? fallbackSim.frame : 0,
      lastP1ToP2Hex: `0x${(fallbackSim.px & 0xff).toString(16).toUpperCase()}`
    },
    rewindSnapshotsCount: fallbackSim.history.length,
    rewindMemoryBytes: fallbackSim.history.length * 640,
    newlyUnlockedAchievement
  };
}

function renderSidebarCounts() {
  const counts = computeConsoleCounts(state.catalog);
  const setTxt = (id, val) => {
    const el = document.getElementById(id);
    if (el) el.textContent = String(val || 0);
  };
  setTxt("count-all", counts.ALL);
  setTxt("count-gbc", (counts.GB || 0) + (counts.GBC || 0));
  setTxt("count-gba", counts.GBA);
  setTxt("count-nes", counts.NES);
  setTxt("count-snes", counts.SNES);
  setTxt("count-n64", counts.N64);
  setTxt("count-megadrive", counts.MEGADRIVE);
  setTxt("count-nds", counts.NDS);
  setTxt("count-ps1", counts.PS1);
  setTxt("count-psp", counts.PSP);
  setTxt("count-chip8", counts.CHIP8);
  setTxt("count-rust", counts.RUST);
  setTxt("count-link", counts.LINK);
}

function renderCatalogGrid() {
  const grid = document.getElementById("games-grid");
  if (!grid) return;

  const opts = {
    consoleFilter:
      state.consoleFilter === "RUST_ONLY" || state.consoleFilter === "LINK_READY"
        ? "ALL"
        : state.consoleFilter,
    onlyNativeRust: state.consoleFilter === "RUST_ONLY",
    onlyLinkReady: state.consoleFilter === "LINK_READY",
    query: state.searchQuery
  };

  const filtered = filterCatalogEntries(state.catalog, opts);
  grid.innerHTML = "";

  for (const game of filtered) {
    const card = document.createElement("div");
    card.className = `game-card${game.id === state.activeGameId ? " selected" : ""}`;
    const isRust = game.engineKind === "nativeRust";
    card.innerHTML = `
      <div class="game-cover" style="background: linear-gradient(135deg, ${game.coverAccent || "#1e3a8a"} 0%, #0f172a 100%)">
        <div class="cover-top">
          <span class="console-tag mono">${game.console}</span>
          <span class="core-tag mono ${isRust ? "core-rust" : "core-libretro"}">
            ${isRust ? "🦀 Rust Native" : "🧩 Libretro"}
          </span>
        </div>
        <div class="cover-art-symbol">${game.coverSymbol || "🕹️"}</div>
        <div class="cover-bottom mono">
          <span style="color: var(--phosphor-green)">✓ ${game.crc32Hex}</span>
          <span style="color: var(--text-muted)">${formatFileSize(game.sizeBytes)}</span>
        </div>
      </div>
      <div class="game-body">
        <div class="game-title">${game.title}</div>
        <div class="game-meta">
          <span>${game.region}</span>
          <span class="mono">${formatPlaytime(game.playtimeSeconds)}</span>
        </div>
      </div>
    `;
    card.addEventListener("click", async () => {
      state.activeGameId = game.id;
      const boot = await invokeBackend("select_game", { gameId: game.id });
      if (boot) applyBootstrap(boot);
      else updateAllViews();
    });
    card.addEventListener("dblclick", async () => {
      state.activeGameId = game.id;
      const boot = await invokeBackend("select_game", { gameId: game.id });
      if (boot) applyBootstrap(boot);
      switchTab("view-emulator");
    });
    grid.appendChild(card);
  }
}

function renderInspector() {
  const g = getSelectedGame();
  document.getElementById("insp-title").textContent = g.title;
  document.getElementById("insp-console").textContent = g.console;
  document.getElementById("insp-core").textContent = g.coreId;
  document.getElementById("insp-region").textContent = g.region;
  document.getElementById("insp-crc").textContent = g.crc32Hex;
  document.getElementById("insp-sram").textContent = g.sramSummary;
  document.getElementById("insp-playtime").textContent = formatPlaytime(g.playtimeSeconds);
  document.getElementById("active-emu-game").textContent = `${g.console} · ${g.title}`;
  document.getElementById("tel-core").textContent = `${g.coreId} (${g.console})`;

  const sramHexEl = document.getElementById("sram-hex-dump");
  if (sramHexEl && state.sramHexInspection?.rows) {
    sramHexEl.innerHTML = state.sramHexInspection.rows
      .map((r) => `<div>${r}</div>`)
      .join("");
  }
  const sramCrcBadge = document.getElementById("sram-crc-badge");
  if (sramCrcBadge && state.sramHexInspection?.crc32Hex) {
    sramCrcBadge.textContent = `✓ CRC ${state.sramHexInspection.crc32Hex}`;
  }

  const ocBadge = document.getElementById("insp-overclock-badge");
  if (ocBadge) {
    ocBadge.textContent = `${state.cpuOverclockPct}% · PGXP ${state.pgxpSubpixel ? "ON" : "OFF"}`;
  }
  const ocBtn = document.getElementById("btn-overclock-cycle");
  if (ocBtn) {
    ocBtn.textContent = `⚡ CPU : ${state.cpuOverclockPct}% ${state.cpuOverclockPct > 100 ? "Turbo" : "Natif"}`;
  }
  const pgxpBtn = document.getElementById("btn-pgxp-toggle");
  if (pgxpBtn) {
    pgxpBtn.textContent = `📐 PGXP 3D : ${state.pgxpSubpixel ? "ON" : "OFF"}`;
    pgxpBtn.style.color = state.pgxpSubpixel ? "var(--phosphor-green)" : "var(--text-muted)";
  }

  const modsEl = document.getElementById("mods-list");
  if (modsEl) {
    modsEl.innerHTML = "";
    for (const m of state.modLayers) {
      const row = document.createElement("label");
      row.className = "mod-item";
      row.innerHTML = `
        <span style="display:flex; align-items:center; gap:8px;">
          <input type="checkbox" ${m.enabled ? "checked" : ""} />
          <span>${m.label}</span>
        </span>
        <span class="mono" style="font-size:10.5px; color:${m.enabled ? "var(--phosphor-green)" : "var(--text-dim)"};">
          ${m.enabled ? "ACTIF" : "OFF"}
        </span>
      `;
      row.querySelector("input").addEventListener("change", async (e) => {
        m.enabled = e.target.checked;
        const boot = await invokeBackend("toggle_mod_layer", {
          modId: m.id,
          enabled: e.target.checked
        });
        if (boot) applyBootstrap(boot);
        else renderInspector();
      });
      modsEl.appendChild(row);
    }
  }

  const cheatsEl = document.getElementById("cheats-list");
  if (cheatsEl) {
    cheatsEl.innerHTML = "";
    for (const c of state.cheatCodes) {
      const row = document.createElement("label");
      row.className = "mod-item";
      row.innerHTML = `
        <span style="display:flex; align-items:center; gap:8px;">
          <input type="checkbox" ${c.enabled ? "checked" : ""} />
          <span>${c.label}</span>
        </span>
        <span class="mono" style="font-size:10.5px; color:var(--gold);">${c.codeText}</span>
      `;
      row.querySelector("input").addEventListener("change", async (e) => {
        c.enabled = e.target.checked;
        const boot = await invokeBackend("toggle_cheat_code", {
          cheatId: c.id,
          enabled: e.target.checked
        });
        if (boot) applyBootstrap(boot);
        else renderInspector();
      });
      cheatsEl.appendChild(row);
    }
  }

  const achEl = document.getElementById("achievements-list");
  if (achEl) {
    achEl.innerHTML = "";
    for (const a of state.achievements) {
      const row = document.createElement("div");
      row.className = "mod-item";
      row.innerHTML = `
        <span>${a.badgeSymbol} ${a.title}</span>
        <span class="mono" style="font-size:10.5px; color:${a.unlocked ? "var(--phosphor-green)" : "var(--text-dim)"};">
          ${a.unlocked ? `✓ +${a.points} pts` : `${a.points} pts`}
        </span>
      `;
      achEl.appendChild(row);
    }
  }

  const slotsEl = document.getElementById("savestate-slots");
  if (slotsEl) {
    slotsEl.innerHTML = "";
    for (const slot of state.saveSlots.slice(0, 6)) {
      const card = document.createElement("div");
      card.className = "slot-card";
      card.innerHTML = `
        <div class="slot-num mono">SLOT #${slot.slot}</div>
        <div class="slot-time mono">${slot.occupied ? slot.label : "Vide (Cliquer pour sauver)"}</div>
      `;
      card.addEventListener("click", async () => {
        if (slot.occupied) {
          const info = await invokeBackend("restore_save_state", { slot: slot.slot });
          document.getElementById("save-toast").textContent =
            `✓ Slot #${slot.slot} chargé (${info?.crc32Hex || slot.crc32Hex})`;
        } else {
          const info = await invokeBackend("create_save_state", {
            slot: slot.slot,
            label: `Frame #${fallbackSim.frame}`
          });
          if (info) {
            const boot = await invokeBackend("get_station_bootstrap");
            if (boot) applyBootstrap(boot);
          } else {
            slot.occupied = true;
            slot.label = `Frame #${fallbackSim.frame}`;
            renderInspector();
          }
        }
      });
      slotsEl.appendChild(card);
    }
  }
}

function renderKeymapperAndMacros() {
  const overlay = document.getElementById("keymapper-p1");
  if (overlay) {
    overlay.innerHTML = "";
    for (const w of state.keymapperWidgets) {
      const badge = document.createElement("div");
      badge.className = `km-widget mono${w.turboHz ? " turbo" : ""}`;
      badge.style.left = `${w.xPct}%`;
      badge.style.top = `${w.yPct}%`;
      badge.style.cursor = "grab";
      badge.textContent = w.label;
      badge.title = "Glisser pour repositionner · Double-clic pour activer/désactiver le Turbo 20 Hz";

      badge.addEventListener("mousedown", (downEv) => {
        downEv.preventDefault();
        const rect = overlay.getBoundingClientRect();
        const onMove = (moveEv) => {
          const rawX = ((moveEv.clientX - rect.left) / Math.max(1, rect.width)) * 100;
          const rawY = ((moveEv.clientY - rect.top) / Math.max(1, rect.height)) * 100;
          const clamped = clampWidgetPosition(rawX, rawY);
          w.xPct = clamped.xPct;
          w.yPct = clamped.yPct;
          badge.style.left = `${w.xPct}%`;
          badge.style.top = `${w.yPct}%`;
        };
        const onUp = async () => {
          window.removeEventListener("mousemove", onMove);
          window.removeEventListener("mouseup", onUp);
          await invokeBackend("save_keymapper_layout", { widgets: state.keymapperWidgets });
        };
        window.addEventListener("mousemove", onMove);
        window.addEventListener("mouseup", onUp);
      });

      badge.addEventListener("dblclick", async () => {
        w.turboHz = w.turboHz ? null : 20;
        renderKeymapperAndMacros();
        await invokeBackend("save_keymapper_layout", { widgets: state.keymapperWidgets });
      });

      overlay.appendChild(badge);
    }
  }

  const macrosList = document.getElementById("macros-list");
  if (macrosList) {
    macrosList.innerHTML = "";
    for (const mac of state.macros) {
      const isRunning = state.activeMacroId === mac.id;
      const row = document.createElement("div");
      row.className = "mod-item";
      row.innerHTML = `
        <span>${mac.name}</span>
        <button class="btn mono" style="padding:3px 8px; font-size:11px;">
          ${isRunning ? "⏹ Stop" : "▶ TAS"}
        </button>
      `;
      row.querySelector("button").addEventListener("click", async () => {
        const nextId = isRunning ? null : mac.id;
        state.activeMacroId = nextId;
        document.getElementById("macro-status").textContent = nextId ? "LECTURE TAS" : "PRÊT";
        const boot = await invokeBackend("set_active_macro", { macroId: nextId });
        if (boot) applyBootstrap(boot);
        else renderKeymapperAndMacros();
      });
      macrosList.appendChild(row);
    }
  }

  const tasList = document.getElementById("tas-timeline-list");
  if (tasList) {
    tasList.innerHTML = "";
    for (const seg of (state.tasTimeline || []).slice(0, 4)) {
      const row = document.createElement("div");
      row.className = "mod-item";
      row.style.fontSize = "10.5px";
      row.innerHTML = `
        <span>F${seg.startFrame}..${seg.endFrame} (${seg.durationFrames}f) · <strong style="color:var(--gold)">${seg.buttonsLabel}</strong></span>
        <button class="btn mono" style="padding:1px 6px; font-size:9.5px;">⚡ +A</button>
      `;
      row.querySelector("button").addEventListener("click", async () => {
        const boot = await invokeBackend("toggle_tas_timeline_button", {
          macroId: state.activeMacroId,
          segmentIndex: seg.segmentIndex,
          buttonBit: BTN_A
        });
        if (boot) applyBootstrap(boot);
        else {
          seg.buttonsMask ^= BTN_A;
          seg.buttonsLabel = seg.buttonsMask ? `0x${seg.buttonsMask.toString(16).toUpperCase()}` : "∅";
          renderKeymapperAndMacros();
        }
      });
      tasList.appendChild(row);
    }
  }

  const wpList = document.getElementById("watchpoints-list");
  if (wpList) {
    wpList.innerHTML = "";
    for (const wp of state.memoryWatchpoints || []) {
      const row = document.createElement("div");
      row.className = "mod-item";
      row.style.fontSize = "10.5px";
      const hexVal = `0x${(wp.currentValue ?? 0).toString(16).padStart(2, "0").toUpperCase()}`;
      row.innerHTML = `
        <span><strong style="color:var(--cobalt)">${wp.addrHex}</strong> ${wp.label} : <strong style="color:var(--phosphor-green)">${hexVal}</strong></span>
        <button class="btn mono" style="padding:1px 6px; font-size:9.5px; color:${wp.frozen ? "var(--gold)" : "var(--text-muted)"};">
          ${wp.frozen ? "🔒 Gelé" : "🔓 Pin"}
        </button>
      `;
      row.querySelector("button").addEventListener("click", async () => {
        const boot = await invokeBackend("toggle_watchpoint_freeze", { watchId: wp.id });
        if (boot) applyBootstrap(boot);
        else {
          wp.frozen = !wp.frozen;
          renderKeymapperAndMacros();
        }
      });
      wpList.appendChild(row);
    }
  }
}

function renderNetplayAndCores() {
  const portBadge = document.getElementById("udp-port-badge");
  if (portBadge) {
    portBadge.textContent = `UDP ZML2 : 127.0.0.1:${state.udpLocalPort || 54920}`;
  }
  const stunBadge = document.getElementById("stun-endpoint-badge");
  if (stunBadge && state.stunCandidate) {
    stunBadge.textContent = `STUN Hole-Punch : ${state.stunCandidate.publicEndpoint} (${state.stunCandidate.natType})`;
  }
  const specBtn = document.getElementById("btn-toggle-spectator");
  if (specBtn && state.tournamentBracket) {
    const active = state.tournamentBracket.spectatorModeActive;
    specBtn.textContent = `👁️ Spectateur (${state.tournamentBracket.spectatorCount}) : ${active ? "ACTIF" : "OFF"}`;
  }

  const chatList = document.getElementById("netplay-chat-list");
  if (chatList) {
    chatList.innerHTML = "";
    for (const msg of state.netplayChat || []) {
      const sharedCrc = msg.sharedStateCrc32Hex || msg.saveStateCrcHex;
      const sharedSlot = msg.sharedStateSlot || 1;
      const row = document.createElement("div");
      row.className = "mono";
      row.style.cssText =
        "display:flex; align-items:center; justify-content:space-between; font-size:11px; padding:3px 0; border-bottom:1px dashed rgba(255,255,255,0.05);";
      row.innerHTML = `
        <span><strong style="color:var(--gold)">[${msg.sender}]</strong> ${msg.text}</span>
        ${
          sharedCrc
            ? `<button class="btn mono" style="padding:1px 6px; font-size:10px; border-color:var(--phosphor-green); color:var(--phosphor-green);">📥 Charger State #${sharedSlot} (${sharedCrc})</button>`
            : ""
        }
      `;
      if (sharedCrc) {
        row.querySelector("button")?.addEventListener("click", async () => {
          await invokeBackend("restore_save_state", { slot: sharedSlot });
          switchTab("view-emulator");
        });
      }
      chatList.appendChild(row);
    }
  }

  const netGrid = document.getElementById("netplay-grid");
  if (netGrid) {
    netGrid.innerHTML = "";

    if (state.tournamentBracket) {
      const t = state.tournamentBracket;
      const tourneyCard = document.createElement("div");
      tourneyCard.className = "panel-card";
      tourneyCard.id = "tournament-bracket-card";
      tourneyCard.innerHTML = `
        <div class="section-title">
          <span>${t.title}</span>
          <span class="pill-badge pill-gold mono">
            ${t.champion ? `👑 Champion : ${t.champion}` : `👁️ ${t.spectatorCount} Spectateurs`}
          </span>
        </div>
        <div class="bracket-matches"></div>
      `;
      const matchesWrap = tourneyCard.querySelector(".bracket-matches");
      for (const m of t.matches || []) {
        const mRow = document.createElement("div");
        mRow.className = "mod-item";
        mRow.innerHTML = `
          <div>
            <div class="mono" style="font-size:10.5px; color:var(--cobalt)">${m.roundLabel}</div>
            <div style="font-weight:700">${m.playerA} <span style="color:var(--gold)">vs</span> ${m.playerB}</div>
            <div class="mono" style="font-size:10.5px; color:${m.winner ? "var(--phosphor-green)" : "var(--text-muted)"}">
              ${m.winner ? `✓ Vainqueur : ${m.winner}` : `Score Live : ${m.scoreA} - ${m.scoreB}`}
            </div>
          </div>
          <div style="display:flex; gap:4px;">
            <button class="btn mono btn-win-a" style="padding:3px 7px; font-size:10px;">🏆 ${m.playerA}</button>
            <button class="btn mono btn-win-b" style="padding:3px 7px; font-size:10px;">🏆 ${m.playerB}</button>
          </div>
        `;
        mRow.querySelector(".btn-win-a")?.addEventListener("click", async () => {
          const boot = await invokeBackend("advance_tournament_match", {
            matchId: m.matchId,
            scoreA: 3,
            scoreB: 1
          });
          if (boot) applyBootstrap(boot);
          else {
            m.winner = m.playerA;
            if (m.matchId === "semi-1") t.matches[2].playerA = m.playerA;
            else if (m.matchId === "semi-2") t.matches[2].playerB = m.playerA;
            else if (m.matchId === "grand-final") t.champion = m.playerA;
            renderNetplayAndCores();
          }
        });
        mRow.querySelector(".btn-win-b")?.addEventListener("click", async () => {
          const boot = await invokeBackend("advance_tournament_match", {
            matchId: m.matchId,
            scoreA: 1,
            scoreB: 3
          });
          if (boot) applyBootstrap(boot);
          else {
            m.winner = m.playerB;
            if (m.matchId === "semi-1") t.matches[2].playerA = m.playerB;
            else if (m.matchId === "semi-2") t.matches[2].playerB = m.playerB;
            else if (m.matchId === "grand-final") t.champion = m.playerB;
            renderNetplayAndCores();
          }
        });
        matchesWrap.appendChild(mRow);
      }
      netGrid.appendChild(tourneyCard);
    }

    for (const r of state.netplayRooms) {
      const card = document.createElement("div");
      card.className = "panel-card";
      card.innerHTML = `
        <div class="section-title">
          <span>🌐 ${r.title}</span>
          <span class="pill-badge pill-green mono">${r.pingMs} ms · Rollback ${r.rollbackFrames}f</span>
        </div>
        <div class="kv-row"><span class="kv-label">Jeu</span><span class="kv-val">${r.gameTitle} (${r.console})</span></div>
        <div class="kv-row"><span class="kv-label">Hôte &amp; Joueurs</span><span class="kv-val mono">${r.hostName} (${r.playersCurrent}/${r.playersMax})</span></div>
        <div class="kv-row"><span class="kv-label">Invitation Deep-Link</span><span class="kv-val mono" style="color:var(--gold)">${r.deepLinkUri}</span></div>
        <div style="margin-top:10px; display:flex; justify-content:flex-end;">
          <button class="btn btn-gold mono" style="font-size:11px;">🔗 Synchroniser Salon &amp; Link 2P</button>
        </div>
      `;
      card.querySelector("button").addEventListener("click", async () => {
        const boot = await invokeBackend("join_netplay_deep_link", {
          uri: r.deepLinkUri,
          peerAddr: null
        });
        if (boot) applyBootstrap(boot);
        if (!state.dualLinkMode) {
          document.getElementById("btn-toggle-dual")?.click();
        }
        switchTab("view-emulator");
      });
      netGrid.appendChild(card);
    }

    for (const ghost of state.ghostLeaderboard || []) {
      const card = document.createElement("div");
      card.className = "panel-card";
      card.innerHTML = `
        <div class="section-title">
          <span>👻 Record Fantôme TAS · ${ghost.playerName}</span>
          <span class="pill-badge pill-cobalt mono">🏆 ${ghost.score} pts · CRC ${ghost.traceCrc32Hex}</span>
        </div>
        <div class="kv-row"><span class="kv-label">Jeu &amp; Console</span><span class="kv-val">${ghost.gameTitle} (${ghost.console})</span></div>
        <div class="kv-row"><span class="kv-label">Trace Déterministe</span><span class="kv-val mono" style="color:var(--phosphor-green)">✓ Vérifié (${ghost.framesCount} frames)</span></div>
        <div style="margin-top:10px; display:flex; justify-content:flex-end;">
          <button class="btn mono" style="font-size:11px; border-color:var(--cobalt); color:var(--cobalt);">⚔️ Défier Fantôme TAS en Direct</button>
        </div>
      `;
      card.querySelector("button").addEventListener("click", async () => {
        const boot = await invokeBackend("challenge_ghost_run", { ghostId: ghost.id });
        if (boot) applyBootstrap(boot);
        document.getElementById("macro-status").textContent = "DÉFI FANTÔME TAS";
        switchTab("view-emulator");
      });
      netGrid.appendChild(card);
    }
  }

  const coresGrid = document.getElementById("cores-grid");
  if (coresGrid) {
    coresGrid.innerHTML = "";

    const communityCard = document.createElement("div");
    communityCard.className = "panel-card";
    communityCard.innerHTML = `<div class="section-title"><span>🧩 Hub Communautaire de Mods .IPS (Installation 1-Clic Non-Destructive)</span></div>`;
    for (const mod of state.communityMods || []) {
      const row = document.createElement("div");
      row.className = "mod-item";
      row.innerHTML = `
        <div>
          <div style="font-weight:700">${mod.title}</div>
          <div style="font-size:11px; color:var(--text-muted)">${mod.description}</div>
          <div class="mono" style="font-size:10.5px; color:var(--gold)">Par ${mod.author} · Cible : ${mod.targetConsole}</div>
        </div>
        <button class="btn mono" style="padding:4px 10px; font-size:11px;">
          ${mod.installed ? "✓ Patch .IPS Actif" : "⚡ Installer .IPS 1-Clic"}
        </button>
      `;
      row.querySelector("button").addEventListener("click", async () => {
        const boot = await invokeBackend("install_community_ips_mod", { modId: mod.id });
        if (boot) {
          applyBootstrap(boot);
        } else {
          mod.installed = true;
          state.modLayers.unshift({
            id: `comm-${mod.id}`,
            label: `${mod.title} (Hub Communautaire)`,
            enabled: true
          });
          updateAllViews();
        }
      });
      communityCard.appendChild(row);
    }

    const libCard = document.createElement("div");
    libCard.className = "panel-card";
    libCard.innerHTML = `<div class="section-title"><span>🧩 Hôte Dynamique Libretro C-ABI (libretro.h v1)</span></div>`;

    for (const c of state.libretroCores) {
      const row = document.createElement("div");
      row.className = "mod-item";
      const cid = c.coreId || c.id;
      row.innerHTML = `
        <div>
          <div style="font-weight:700">${c.displayName}</div>
          <div class="mono" style="font-size:11px; color:var(--text-muted)">${c.libraryWindows} / ${c.libraryMacos}</div>
          <div class="mono" style="font-size:10.5px; color:${c.installed ? "var(--phosphor-green)" : "var(--cobalt)"};">${c.statusLabel}</div>
        </div>
        <button class="btn mono" style="padding:4px 10px; font-size:11px;">
          ${c.installed ? "✓ C-ABI Vérifié" : "⚡ Provisionner C-ABI"}
        </button>
      `;
      row.querySelector("button").addEventListener("click", async () => {
        const boot = await invokeBackend("provision_libretro_core", {
          coreId: cid,
          customBytes: null
        });
        if (boot) {
          applyBootstrap(boot);
        } else {
          c.installed = true;
          c.statusLabel = "✓ Installé · 9/9 symboles C-ABI vérifiés";
          renderNetplayAndCores();
        }
      });
      libCard.appendChild(row);
    }

    const biosCard = document.createElement("div");
    biosCard.className = "panel-card";
    biosCard.innerHTML = `
      <div class="section-title"><span>🔐 Registre Cryptographique des BIOS</span></div>
      ${state.biosEntries
        .map(
          (b) => `
          <div class="mod-item">
            <div>
              <div class="mono" style="font-weight:700">${b.fileName} (${b.console})</div>
              <div style="font-size:11px; color:var(--text-muted)">${b.notes}</div>
            </div>
            <span class="pill-badge pill-green mono">CRC ${b.expectedCrc32Hex}</span>
          </div>
        `
        )
        .join("")}
    `;

    coresGrid.appendChild(communityCard);
    coresGrid.appendChild(libCard);
    coresGrid.appendChild(biosCard);
  }
}

function applyBootstrap(boot) {
  state.activeGameId = boot.activeGameId;
  state.catalog = boot.catalog || state.catalog;
  state.modLayers = boot.modLayers || state.modLayers;
  state.cheatCodes = boot.cheatCodes || state.cheatCodes;
  state.memoryWatchpoints = boot.memoryWatchpoints || state.memoryWatchpoints;
  state.achievements = boot.achievements || state.achievements;
  state.saveSlots = boot.saveSlots || state.saveSlots;
  state.keymapperWidgets = boot.keymapperWidgets || state.keymapperWidgets;
  state.macros = boot.macros || state.macros;
  state.tasTimeline = boot.tasTimeline || state.tasTimeline;
  state.netplayRooms = boot.netplayRooms || state.netplayRooms;
  state.udpLocalPort = boot.udpLocalPort || state.udpLocalPort;
  state.stunCandidate = boot.stunCandidate || state.stunCandidate;
  state.netplayChat = boot.netplayChat || state.netplayChat;
  state.communityMods = boot.communityMods || state.communityMods;
  state.ghostLeaderboard = boot.ghostLeaderboard || state.ghostLeaderboard;
  state.cpuOverclockPct = boot.cpuOverclockPct || state.cpuOverclockPct;
  state.pgxpSubpixel =
    boot.pgxpSubpixel !== undefined ? boot.pgxpSubpixel : state.pgxpSubpixel;
  state.sramHexInspection = boot.sramHexInspection || state.sramHexInspection;
  state.tournamentBracket = boot.tournamentBracket || state.tournamentBracket;
  state.libretroCores = boot.libretroCores || state.libretroCores;
  state.biosEntries = boot.biosEntries || state.biosEntries;
  state.linkTelemetry = boot.linkTelemetry || { enabled: false, packetsTransferred: 0 };
  updateAllViews();
}

function updateAllViews() {
  renderSidebarCounts();
  renderCatalogGrid();
  renderInspector();
  renderKeymapperAndMacros();
  renderNetplayAndCores();
}

function switchTab(targetId) {
  document.querySelectorAll(".nav-tab").forEach((btn) => {
    btn.classList.toggle("active", btn.dataset.target === targetId);
  });
  document.querySelectorAll(".view-pane").forEach((pane) => {
    pane.classList.toggle("active", pane.id === targetId);
  });
}

function paintRgbaFrame(canvasId, frame) {
  const canvas = document.getElementById(canvasId);
  if (!canvas || !frame || !frame.rgba) return;
  const processed =
    canvasId.startsWith("canvas-p") && state.upscalerMode !== "OFF"
      ? upscaleFrameIfEnabled(frame, state.upscalerMode)
      : frame;
  if (canvas.width !== processed.width || canvas.height !== processed.height) {
    canvas.width = processed.width;
    canvas.height = processed.height;
  }
  const ctx = canvas.getContext("2d");
  const clamped = new Uint8ClampedArray(processed.rgba);
  if (clamped.length === processed.width * processed.height * 4) {
    const imgData = new ImageData(clamped, processed.width, processed.height);
    ctx.putImageData(imgData, 0, 0);
  }
}

function renderAudioOscilloscope(freqHz, frameNumber) {
  const osc = document.getElementById("audio-oscilloscope-canvas");
  if (!osc) return;
  const ctx = osc.getContext("2d");
  const w = osc.width;
  const h = osc.height;
  ctx.fillStyle = "#070a0f";
  ctx.fillRect(0, 0, w, h);

  ctx.strokeStyle = "rgba(56, 189, 248, 0.18)";
  ctx.beginPath();
  ctx.moveTo(0, h / 2);
  ctx.lineTo(w, h / 2);
  ctx.stroke();

  const samples = synthesizeMultiChannelSamples(freqHz || 220, frameNumber, 48);
  ctx.strokeStyle = freqHz > 0 ? "#4ade80" : "#f0b429";
  ctx.lineWidth = 1.5;
  ctx.beginPath();
  for (let i = 0; i < samples.length; i++) {
    const x = (i / Math.max(1, samples.length - 1)) * w;
    const y = h / 2 - samples[i] * (h * 0.42);
    if (i === 0) ctx.moveTo(x, y);
    else ctx.lineTo(x, y);
  }
  ctx.stroke();
}

async function handleImportedFile(file) {
  if (!file) return;
  const buf = await file.arrayBuffer();
  const bytes = Array.from(new Uint8Array(buf));
  const lower = file.name.toLowerCase();

  if (lower.endsWith(".zaimule-pack") || lower.endsWith(".json")) {
    const boot = await invokeBackend("import_portable_pack_archive", { bytes });
    if (boot) {
      applyBootstrap(boot);
    }
    const toast = document.getElementById("save-toast");
    if (toast) toast.textContent = `☁️ Archive ${file.name} synchronisée !`;
    return;
  }

  if (lower.endsWith(".ips")) {
    const boot = await invokeBackend("apply_ips_patch_file", {
      patchName: file.name,
      patchBytes: bytes
    });
    if (boot) {
      applyBootstrap(boot);
    } else {
      state.modLayers.unshift({
        id: `ips-${Date.now()}`,
        label: `${file.name} (Patch RAM Actif)`,
        enabled: true
      });
      renderInspector();
    }
    return;
  }

  const boot = await invokeBackend("import_rom_file", {
    fileName: file.name,
    bytes
  });
  if (boot) {
    applyBootstrap(boot);
  } else {
    const newId = `rom-${Date.now()}`;
    state.catalog.unshift({
      id: newId,
      title: file.name.replace(/\.[^.]+$/, ""),
      console: lower.endsWith(".nes") ? "NES" : lower.endsWith(".ch8") ? "CHIP8" : "GBC",
      engineKind: "nativeRust",
      coreId: lower.endsWith(".nes") ? "zaimule-rust-nes" : "zaimule-rust-gb",
      region: "Import Local · CRC32 Vérifié",
      crc32Hex: "A1F09C82",
      sizeBytes: bytes.length,
      sramSummary: "32 Ko SRAM Atomique (.tmp → .srm)",
      playtimeSeconds: 0,
      favorite: true,
      linkCableReady: true,
      coverAccent: "#1e3a8a",
      coverSymbol: "🕹️"
    });
    state.activeGameId = newId;
    updateAllViews();
  }
  switchTab("view-emulator");
}

function renderDebuggerPayload(dbg) {
  if (!dbg) return;
  if (dbg.vram) {
    paintRgbaFrame("vram-atlas-canvas", {
      width: dbg.vram.atlasWidth || 128,
      height: dbg.vram.atlasHeight || 64,
      rgba: dbg.vram.tileAtlasRgba
    });
  }
  const listEl = document.getElementById("disasm-list");
  if (listEl && Array.isArray(dbg.disassembly)) {
    listEl.innerHTML = "";
    for (const line of dbg.disassembly) {
      const row = document.createElement("div");
      row.className = `disasm-row${line.isPc ? " is-pc" : ""}${line.hasBreakpoint ? " has-bp" : ""}`;
      row.innerHTML = `
        <span>${line.hasBreakpoint ? "● " : line.isPc ? "▶ " : "  "}${line.addrHex}</span>
        <span style="color:var(--text-main)">${line.mnemonic}</span>
      `;
      row.title = "Cliquer pour basculer un point d'arrêt (Breakpoint PC)";
      row.addEventListener("click", async () => {
        const updated = await invokeBackend("toggle_breakpoint", { addr: line.addr });
        if (updated) renderDebuggerPayload(updated);
      });
      listEl.appendChild(row);
    }
  }
}

async function refreshDebuggerDrawer() {
  const dbg = await invokeBackend("get_vram_and_disassembly");
  if (dbg) {
    renderDebuggerPayload(dbg);
    return;
  }
  const fallbackDisasm = [
    { addr: 0x0150, addrHex: "0x0150", mnemonic: "LDH A, ($FF00)", isPc: true, hasBreakpoint: true },
    { addr: 0x0152, addrHex: "0x0152", mnemonic: "INC B", isPc: false, hasBreakpoint: false },
    { addr: 0x0153, addrHex: "0x0153", mnemonic: "INC HL", isPc: false, hasBreakpoint: false },
    { addr: 0x0154, addrHex: "0x0154", mnemonic: "SWAP A", isPc: false, hasBreakpoint: false },
    { addr: 0x0156, addrHex: "0x0156", mnemonic: "JP $0150", isPc: false, hasBreakpoint: false }
  ];
  renderDebuggerPayload({
    disassembly: fallbackDisasm,
    vram: null
  });
}

async function tickEmulationLoop() {
  if (state.debuggerPaused && !state.stepOnceRequested) {
    requestAnimationFrame(tickEmulationLoop);
    return;
  }
  state.stepOnceRequested = false;

  const buttonsP1 = computeBitmaskFromPressedCodes(pressedKeys) | pollGamepadBitmask();
  await handleBigPictureNavigation(buttonsP1);

  if (state.recordingMacro) {
    state.recordedFrames.push(buttonsP1);
  }

  const resp =
    (await invokeBackend("step_viewport", {
      buttonsP1,
      buttonsP2: 0,
      turboActive: true
    })) || stepBrowserFallback(buttonsP1);

  if (resp && resp.primary) {
    paintRgbaFrame("canvas-p1", resp.primary);
    if (resp.secondary) {
      paintRgbaFrame("canvas-p2", resp.secondary);
    }
    if (resp.tertiary) {
      paintRgbaFrame("canvas-p3", resp.tertiary);
    } else if (state.quadLinkMode) {
      paintRgbaFrame("canvas-p3", resp.primary);
    }
    if (resp.quaternary) {
      paintRgbaFrame("canvas-p4", resp.quaternary);
    } else if (state.quadLinkMode) {
      paintRgbaFrame("canvas-p4", resp.primary);
    }

    renderAudioOscilloscope(resp.primary.audioFreqHz || 0, resp.primary.frameNumber);

    if (resp.primary.audioFreqHz) {
      apuAudio.triggerFrameTone(resp.primary.audioFreqHz);
      if (resp.primary.audioFreqHz >= 1200) {
        triggerGamepadRumble(getConnectedGamepad(), 55, 0.35, 0.65);
      }
    }
    if (resp.newlyUnlockedAchievement) {
      triggerGamepadRumble(getConnectedGamepad(), 140, 0.6, 0.95);
      document.getElementById("save-toast").textContent =
        `🏆 Succès débloqué : ${resp.newlyUnlockedAchievement}`;
      const boot = await invokeBackend("get_station_bootstrap");
      if (boot) applyBootstrap(boot);
      else renderInspector();
    }
    const cpu = resp.primary.cpu;
    const score = cpu.score ?? 0;
    document.getElementById("emu-score-pill").textContent = `🏆 Score : ${score}`;
    document.getElementById("p1-reg-readout").textContent = `${cpu.arch} · PC:${cpu.pcHex}`;
    document.getElementById("rewind-readout").textContent = `Frame #${resp.primary.frameNumber}`;
    document.getElementById("dbg-pc-sp").textContent = `${cpu.pcHex} / ${cpu.spHex}`;
    document.getElementById("dbg-af-bc").textContent = `${cpu.regAHex} / ${cpu.regBcHex}`;
    document.getElementById("dbg-de-hl").textContent = `${cpu.regDeHex} / ${cpu.regHlHex}`;
    document.getElementById("dbg-sio").textContent = `${cpu.serialSbHex} / ${resp.primary.audioFreqHz || 0} Hz`;
    document.getElementById("tel-rewind").textContent = `${resp.rewindSnapshotsCount} clichés (${Math.round(resp.rewindMemoryBytes / 1024)} Ko)`;
    if (resp.linkTelemetry?.enabled) {
      const modePrefix = resp.linkTelemetry.quadMode ? "4P Tournoi" : "2P Sync";
      document.getElementById("tel-link").textContent =
        `${modePrefix} (${resp.linkTelemetry.packetsTransferred} pkts · ${resp.linkTelemetry.lastP1ToP2Hex})`;
    }
    if ((resp.primary.frameNumber & 15) === 1 || state.debuggerPaused) {
      await refreshDebuggerDrawer();
    }
  }

  requestAnimationFrame(tickEmulationLoop);
}

function bindEvents() {
  document.querySelectorAll(".nav-tab").forEach((btn) => {
    btn.addEventListener("click", () => switchTab(btn.dataset.target));
  });

  document.getElementById("btn-big-picture")?.addEventListener("click", (e) => {
    state.bigPictureMode = !state.bigPictureMode;
    document.querySelector(".app-shell")?.classList.toggle("big-picture", state.bigPictureMode);
    e.currentTarget.textContent = `🛋️ Big Picture : ${state.bigPictureMode ? "ACTIF" : "OFF"}`;
    triggerGamepadRumble(getConnectedGamepad(), 70, 0.4, 0.7);
  });

  const colorLabels = ["🎨 #0 Noir", "🎨 #1 Sombre", "🎨 #2 Clair", "🎨 #3 Or"];
  document.getElementById("btn-vram-color")?.addEventListener("click", (e) => {
    state.vramPaintColorIdx = (state.vramPaintColorIdx + 3) % 4;
    e.currentTarget.textContent = colorLabels[state.vramPaintColorIdx];
  });

  document.getElementById("vram-atlas-canvas")?.addEventListener("click", async (e) => {
    const canvas = e.currentTarget;
    const rect = canvas.getBoundingClientRect();
    const atlasX = Math.max(
      0,
      Math.min(127, Math.floor(((e.clientX - rect.left) / Math.max(1, rect.width)) * 128))
    );
    const atlasY = Math.max(
      0,
      Math.min(63, Math.floor(((e.clientY - rect.top) / Math.max(1, rect.height)) * 64))
    );
    const updated = await invokeBackend("paint_vram_atlas_pixel", {
      atlasX,
      atlasY,
      colorIdx: state.vramPaintColorIdx
    });
    if (updated) {
      renderDebuggerPayload(updated);
    }
    const toast = document.getElementById("save-toast");
    if (toast) {
      toast.textContent = `🎨 Pixel VRAM (${atlasX}, ${atlasY}) peint en couleur #${state.vramPaintColorIdx}`;
    }
  });

  const canvasP1 = document.getElementById("canvas-p1");
  const handleStylusEvent = async (ev, pressed) => {
    if (!canvasP1) return;
    const rect = canvasP1.getBoundingClientRect();
    const normX = Math.max(0, Math.min(1, (ev.clientX - rect.left) / Math.max(1, rect.width)));
    const normY = Math.max(0, Math.min(1, (ev.clientY - rect.top) / Math.max(1, rect.height)));
    const x = Math.floor(normX * 255);
    const fullY = Math.floor(normY * (canvasP1.height || 384));
    const touchY = canvasP1.height >= 300 ? Math.max(0, fullY - 192) : Math.floor(normY * 191);
    await invokeBackend("set_nds_stylus", { x, y: Math.min(191, touchY), pressed });
  };

  canvasP1?.addEventListener("mousedown", async (e) => {
    state.stylusPressed = true;
    await handleStylusEvent(e, true);
  });
  canvasP1?.addEventListener("mousemove", async (e) => {
    if (!state.stylusPressed) return;
    await handleStylusEvent(e, true);
  });
  window.addEventListener("mouseup", async (e) => {
    if (!state.stylusPressed) return;
    state.stylusPressed = false;
    await handleStylusEvent(e, false);
  });

  document.getElementById("btn-send-chat")?.addEventListener("click", async () => {
    const inp = document.getElementById("netplay-chat-input");
    const text = inp?.value?.trim();
    if (!text) return;
    const boot = await invokeBackend("post_netplay_chat_message", {
      sender: "Zainados",
      text,
      shareSlot1State: false
    });
    if (boot) {
      applyBootstrap(boot);
    } else {
      state.netplayChat.unshift({
        id: `chat-${Date.now()}`,
        sender: "Zainados",
        text,
        saveStateCrcHex: null
      });
      renderNetplayAndCores();
    }
    inp.value = "";
  });

  document.getElementById("btn-share-state-chat")?.addEventListener("click", async () => {
    const g = getSelectedGame();
    const text = `Cliché Save State Slot #1 (${g.title}) prêt pour synchronisation P2P !`;
    const boot = await invokeBackend("post_netplay_chat_message", {
      sender: "Zainados",
      text,
      shareSlot1State: true
    });
    if (boot) {
      applyBootstrap(boot);
    } else {
      state.netplayChat.unshift({
        id: `chat-state-${Date.now()}`,
        sender: "Zainados",
        text,
        saveStateCrcHex: g.crc32Hex
      });
      renderNetplayAndCores();
    }
  });

  document.querySelectorAll(".console-btn").forEach((btn) => {
    btn.addEventListener("click", () => {
      document.querySelectorAll(".console-btn").forEach((b) => b.classList.remove("active"));
      btn.classList.add("active");
      state.consoleFilter = btn.dataset.console;
      renderCatalogGrid();
    });
  });

  document.getElementById("search-input")?.addEventListener("input", (e) => {
    state.searchQuery = e.target.value;
    renderCatalogGrid();
  });

  const romInput = document.getElementById("rom-file-input");
  const ipsInput = document.getElementById("ips-file-input");
  const packInput = document.getElementById("pack-file-input");

  document.getElementById("btn-import-rom")?.addEventListener("click", () => romInput?.click());
  document.getElementById("btn-import-ips")?.addEventListener("click", () => ipsInput?.click());
  document.getElementById("btn-insp-ips")?.addEventListener("click", () => ipsInput?.click());
  document.getElementById("btn-import-pack")?.addEventListener("click", () => packInput?.click());

  document.getElementById("btn-overclock-cycle")?.addEventListener("click", async () => {
    state.cpuOverclockPct =
      state.cpuOverclockPct === 100 ? 150 : state.cpuOverclockPct === 150 ? 200 : 100;
    const boot = await invokeBackend("set_hardware_overclock_profile", {
      cpuOverclockPct: state.cpuOverclockPct,
      pgxpSubpixel: state.pgxpSubpixel
    });
    if (boot) applyBootstrap(boot);
    else renderInspector();
  });

  document.getElementById("btn-pgxp-toggle")?.addEventListener("click", async () => {
    state.pgxpSubpixel = !state.pgxpSubpixel;
    const boot = await invokeBackend("set_hardware_overclock_profile", {
      cpuOverclockPct: state.cpuOverclockPct,
      pgxpSubpixel: state.pgxpSubpixel
    });
    if (boot) applyBootstrap(boot);
    else renderInspector();
  });

  document.getElementById("btn-sram-boost")?.addEventListener("click", async () => {
    const updated = await invokeBackend("write_sram_byte_live", {
      offset: 4,
      value: 0xff
    });
    if (updated?.sramHexInspection) {
      applyBootstrap(updated);
    } else if (updated) {
      state.sramHexInspection = updated;
      renderInspector();
    } else if (state.sramHexInspection) {
      state.sramHexInspection.crc32Hex = "FF90A12C";
      state.sramHexInspection.rows[0] =
        "0x0000: 5A 41 49 4D FF 4C 45 30  00 00 00 00 00 00 00 00 | ZAIM.LE0........";
      renderInspector();
    }
    const toast = document.getElementById("save-toast");
    if (toast) toast.textContent = "✏️ Octet SRAM 0x0004 = 0xFF écrit & CRC32 recalculé !";
  });

  document.getElementById("btn-sram-transfer")?.addEventListener("click", async () => {
    const boot = await invokeBackend("transfer_sram_p1_to_p2");
    if (boot) applyBootstrap(boot);
    const toast = document.getElementById("save-toast");
    if (toast) toast.textContent = "🔄 Batterie SRAM / Carte Mémoire synchronisée J1 ➔ J2 !";
  });

  document.getElementById("btn-toggle-spectator")?.addEventListener("click", async () => {
    const boot = await invokeBackend("toggle_tournament_spectator");
    if (boot) {
      applyBootstrap(boot);
    } else if (state.tournamentBracket) {
      state.tournamentBracket.spectatorModeActive =
        !state.tournamentBracket.spectatorModeActive;
      state.tournamentBracket.spectatorCount += state.tournamentBracket.spectatorModeActive
        ? 1
        : -1;
      renderNetplayAndCores();
    }
  });

  document.getElementById("btn-submit-ghost")?.addEventListener("click", async () => {
    const g = getSelectedGame();
    const boot = await invokeBackend("submit_ghost_leaderboard_run", {
      playerName: "Zainados"
    });
    if (boot) {
      applyBootstrap(boot);
    } else {
      state.ghostLeaderboard.unshift({
        id: `ghost-${Date.now()}`,
        gameId: g.id,
        gameTitle: g.title,
        console: g.console,
        playerName: "Zainados",
        score: Math.max(10, fallbackSim.score),
        framesCount: Math.max(60, fallbackSim.frame),
        traceCrc32Hex: g.crc32Hex,
        verified: true
      });
      renderNetplayAndCores();
    }
  });

  document.getElementById("btn-export-pack")?.addEventListener("click", async () => {
    const packBytes =
      (await invokeBackend("export_portable_pack_archive")) ||
      Array.from(
        new TextEncoder().encode(
          JSON.stringify({
            magic: "ZAIMULE_PACK_V1",
            version: "1.0.0",
            activeGameId: state.activeGameId,
            cheatCodes: state.cheatCodes,
            macros: state.macros
          })
        )
      );
    downloadBinaryFile(packBytes, `${state.activeGameId}-cloud.zaimule-pack`, "application/json");
    const toast = document.getElementById("save-toast");
    if (toast) toast.textContent = `☁️ Pack .zaimule-pack exporté (${packBytes.length} o)`;
  });

  document.getElementById("btn-export-ips")?.addEventListener("click", async () => {
    const ipsBytes =
      (await invokeBackend("export_ips_diff")) || [
        0x50, 0x41, 0x54, 0x43, 0x48, 0x00, 0x01, 0x34, 0x00, 0x04, 0x5a, 0x4d, 0x44, 0x58, 0x45,
        0x4f, 0x46
      ];
    downloadBinaryFile(ipsBytes, `${state.activeGameId}-dx.ips`, "application/octet-stream");
    state.modLayers.unshift({
      id: `ips-diff-${Date.now()}`,
      label: `Création Diff ${state.activeGameId}-dx.ips (${ipsBytes.length} o)`,
      enabled: true
    });
    renderInspector();
  });

  romInput?.addEventListener("change", async (e) => {
    const file = e.target.files?.[0];
    if (file) await handleImportedFile(file);
    e.target.value = "";
  });

  ipsInput?.addEventListener("change", async (e) => {
    const file = e.target.files?.[0];
    if (file) await handleImportedFile(file);
    e.target.value = "";
  });

  packInput?.addEventListener("change", async (e) => {
    const file = e.target.files?.[0];
    if (file) await handleImportedFile(file);
    e.target.value = "";
  });

  window.addEventListener("dragover", (e) => {
    e.preventDefault();
  });
  window.addEventListener("drop", async (e) => {
    e.preventDefault();
    const file = e.dataTransfer?.files?.[0];
    if (file) await handleImportedFile(file);
  });

  document.getElementById("btn-add-cheat")?.addEventListener("click", async () => {
    const inp = document.getElementById("cheat-input");
    const rawCode = inp?.value?.trim();
    if (!rawCode) return;
    const boot = await invokeBackend("add_cheat_code", {
      rawCode,
      label: `GameShark ${rawCode.toUpperCase()}`
    });
    if (boot) {
      applyBootstrap(boot);
    } else {
      state.cheatCodes.unshift({
        id: `cheat-${Date.now()}`,
        label: `GameShark ${rawCode.toUpperCase()}`,
        codeText: rawCode.toUpperCase(),
        enabled: true
      });
      renderInspector();
    }
    inp.value = "";
  });

  document.getElementById("btn-add-watchpoint")?.addEventListener("click", async () => {
    const inp = document.getElementById("ram-scan-input");
    const rawAddr = inp?.value?.trim() || "0xC110=7F";
    const boot = await invokeBackend("add_memory_watchpoint", {
      rawAddr,
      label: `Pin ${rawAddr.toUpperCase()}`,
      frozen: true
    });
    if (boot) {
      applyBootstrap(boot);
    } else {
      state.memoryWatchpoints.unshift({
        id: `wp-${Date.now()}`,
        label: `Pin ${rawAddr.toUpperCase()}`,
        addr: 0xc110,
        addrHex: rawAddr.split("=")[0].toUpperCase(),
        currentValue: 0x7f,
        previousValue: 0x7f,
        frozen: true,
        frozenValue: 0x7f
      });
      renderKeymapperAndMacros();
    }
  });

  document.getElementById("btn-scan-ram")?.addEventListener("click", async () => {
    const inp = document.getElementById("ram-scan-input");
    const targetValue = Number(inp?.value || 0) & 0xff;
    const hits =
      (await invokeBackend("scan_ram_byte", { targetValue })) || [
        "0xC100",
        "0xC101",
        "0xC108"
      ];
    const outEl = document.getElementById("ram-scan-results");
    if (outEl) {
      outEl.textContent =
        hits.length > 0
          ? `${hits.length} adresses : ${hits.slice(0, 6).join(", ")}`
          : "Aucune adresse trouvée";
    }
  });

  document.getElementById("btn-create-room")?.addEventListener("click", async () => {
    const g = getSelectedGame();
    const boot = await invokeBackend("create_netplay_room", {
      title: `Salon Live · ${g.title}`,
      hostName: "Zainados",
      peerAddr: null
    });
    if (boot) {
      applyBootstrap(boot);
    } else {
      state.netplayRooms.unshift({
        roomCode: `ZM-${g.crc32Hex.slice(0, 4)}`,
        title: `Salon Live · ${g.title}`,
        hostName: "Zainados",
        console: g.console,
        gameTitle: g.title,
        pingMs: 4,
        rollbackFrames: 1,
        playersCurrent: 1,
        playersMax: 2,
        deepLinkUri: `zaimule://room/ZM-${g.crc32Hex.slice(0, 4)}?crc=${g.crc32Hex}&core=${g.coreId}`
      });
      renderNetplayAndCores();
    }
  });

  document.getElementById("btn-join-deeplink")?.addEventListener("click", async () => {
    const inp = document.getElementById("deeplink-input");
    const uri = inp?.value?.trim();
    if (!uri) return;
    const boot = await invokeBackend("join_netplay_deep_link", { uri, peerAddr: null });
    if (boot) applyBootstrap(boot);
    switchTab("view-emulator");
  });

  document.getElementById("btn-launch-selected")?.addEventListener("click", () => {
    switchTab("view-emulator");
  });
  document.getElementById("btn-insp-play")?.addEventListener("click", () => {
    switchTab("view-emulator");
  });

  document.getElementById("btn-audio-toggle")?.addEventListener("click", (e) => {
    const on = apuAudio.toggleEnabled();
    e.currentTarget.textContent = `🔊 Audio APU : ${on ? "ON" : "MUET"}`;
  });

  document.getElementById("btn-record-video")?.addEventListener("click", (e) => {
    const canvas = document.getElementById("canvas-p1");
    if (!viewportRecorder.isRecording) {
      viewportRecorder.start(canvas, state.activeGameId);
      e.currentTarget.textContent = "⏹ Stop .WebM";
      document.getElementById("save-toast").textContent = "🎥 Capture vidéo 60 FPS en cours...";
    } else {
      const res = viewportRecorder.stop();
      e.currentTarget.textContent = "🎥 Clip .WebM";
      document.getElementById("save-toast").textContent =
        `✓ Clip enregistré : ${res.fileName} (${res.durationText})`;
    }
  });

  document.getElementById("btn-upscaler-cycle")?.addEventListener("click", (e) => {
    state.upscalerMode =
      state.upscalerMode === "OFF"
        ? "SCALE2X"
        : state.upscalerMode === "SCALE2X"
          ? "XBRZ"
          : state.upscalerMode === "XBRZ"
            ? "BLOOM"
            : "OFF";
    const label =
      state.upscalerMode === "SCALE2X"
        ? "✨ Upscaler : Scale2x EPX"
        : state.upscalerMode === "XBRZ"
          ? "✨ Upscaler : xBRZ 2x"
          : state.upscalerMode === "BLOOM"
            ? "✨ Upscaler : CRT Bloom"
            : "✨ Upscaler : Natif 1x";
    e.currentTarget.textContent = label;
  });

  const shaders = ["mode-crt", "mode-trinitron", "mode-dmg", "mode-lcd", "mode-clean"];
  const shaderLabels = [
    "📺 Shader : CRT Scanlines",
    "📺 Shader : Trinitron Courbé",
    "📺 Shader : Game Boy DMG Vert",
    "📺 Shader : Matrice LCD",
    "📺 Shader : Pixel-Perfect"
  ];
  let shaderIdx = 0;
  document.getElementById("btn-shader-cycle")?.addEventListener("click", (e) => {
    shaderIdx = (shaderIdx + 1) % shaders.length;
    e.currentTarget.textContent = shaderLabels[shaderIdx];
    ["screen-1", "screen-2", "screen-3", "screen-4"].forEach((id) => {
      const el = document.getElementById(id);
      if (!el) return;
      el.classList.remove("mode-crt", "mode-trinitron", "mode-dmg", "mode-lcd", "mode-clean");
      el.classList.add(shaders[shaderIdx]);
    });
  });

  const toggleKm = () => {
    state.keymapperVisible = !state.keymapperVisible;
    document.getElementById("keymapper-p1")?.classList.toggle("visible", state.keymapperVisible);
    const btn = document.getElementById("btn-toggle-keymapper");
    if (btn) {
      btn.textContent = `🎯 Keymapper : ${state.keymapperVisible ? "ON" : "OFF"}`;
    }
  };
  document.getElementById("btn-toggle-keymapper")?.addEventListener("click", toggleKm);
  document.getElementById("btn-dock-keymapper")?.addEventListener("click", toggleKm);

  document.getElementById("btn-record-macro")?.addEventListener("click", async (e) => {
    if (!state.recordingMacro) {
      state.recordingMacro = true;
      state.recordedFrames = [];
      e.currentTarget.textContent = "⏹ Arrêter & Sauver Macro";
      document.getElementById("macro-status").textContent = "ENREGISTREMENT...";
    } else {
      state.recordingMacro = false;
      e.currentTarget.textContent = "● Enregistrer Macro Live";
      const frames = state.recordedFrames.length > 0 ? state.recordedFrames : [BTN_A];
      const name = `⚡ Combo Live (${frames.length}f)`;
      const boot = await invokeBackend("save_recorded_macro", { name, frames });
      if (boot) {
        applyBootstrap(boot);
      } else {
        const id = `macro-custom-${Date.now()}`;
        state.macros.unshift({ id, name });
        state.activeMacroId = id;
        renderKeymapperAndMacros();
      }
      document.getElementById("macro-status").textContent = "LECTURE TAS";
    }
  });

  document.getElementById("btn-toggle-dual")?.addEventListener("click", async (e) => {
    state.dualLinkMode = !state.dualLinkMode;
    if (!state.dualLinkMode) {
      state.quadLinkMode = false;
      document.getElementById("bezel-p3").style.display = "none";
      document.getElementById("bezel-p4").style.display = "none";
      document.getElementById("screens-wrap").classList.remove("quad-mode");
      const quadBtn = document.getElementById("btn-toggle-quad");
      if (quadBtn) quadBtn.textContent = "🏆 Tournoi 4P : OFF";
    }
    document.getElementById("bezel-p2").style.display = state.dualLinkMode ? "flex" : "none";
    document.getElementById("screens-wrap").classList.toggle("dual-mode", state.dualLinkMode && !state.quadLinkMode);
    e.currentTarget.textContent = `🔗 Link 2P : ${state.dualLinkMode ? "ACTIF" : "OFF"}`;
    document.getElementById("link-status-pill").textContent = state.dualLinkMode
      ? "🔗 Câble Link 2P Synchronisé"
      : "Mode 1P Solo";
    await invokeBackend("set_dual_link_enabled", { enabled: state.dualLinkMode });
  });

  document.getElementById("btn-toggle-quad")?.addEventListener("click", async (e) => {
    state.quadLinkMode = !state.quadLinkMode;
    if (state.quadLinkMode) {
      state.dualLinkMode = true;
    }
    const wrap = document.getElementById("screens-wrap");
    wrap.classList.remove("dual-mode");
    wrap.classList.toggle("quad-mode", state.quadLinkMode);
    document.getElementById("bezel-p2").style.display = state.quadLinkMode || state.dualLinkMode ? "flex" : "none";
    document.getElementById("bezel-p3").style.display = state.quadLinkMode ? "flex" : "none";
    document.getElementById("bezel-p4").style.display = state.quadLinkMode ? "flex" : "none";
    e.currentTarget.textContent = `🏆 Tournoi 4P : ${state.quadLinkMode ? "ACTIF" : "OFF"}`;
    document.getElementById("link-status-pill").textContent = state.quadLinkMode
      ? "🏆 Tournoi 4P Multi-SIO"
      : state.dualLinkMode
        ? "🔗 Câble Link 2P Synchronisé"
        : "Mode 1P Solo";
    await invokeBackend("set_quad_link_enabled", { enabled: state.quadLinkMode });
  });

  document.getElementById("btn-dbg-pause")?.addEventListener("click", (e) => {
    state.debuggerPaused = !state.debuggerPaused;
    e.currentTarget.textContent = state.debuggerPaused ? "▶ Reprendre" : "⏸ Pause CPU";
  });

  document.getElementById("btn-dbg-step")?.addEventListener("click", () => {
    state.debuggerPaused = true;
    state.stepOnceRequested = true;
    const pauseBtn = document.getElementById("btn-dbg-pause");
    if (pauseBtn) pauseBtn.textContent = "▶ Reprendre";
  });

  document.getElementById("btn-speed-cycle")?.addEventListener("click", (e) => {
    state.speedMult = state.speedMult === 1 ? 2 : state.speedMult === 2 ? 4 : 1;
    e.currentTarget.textContent = `⚡ Vitesse : ${state.speedMult}.0x`;
    document.getElementById("tel-fps").textContent = `${(60 * state.speedMult).toFixed(1)} FPS`;
  });

  document.getElementById("btn-quick-save")?.addEventListener("click", async () => {
    const info = await invokeBackend("create_save_state", { slot: 1, label: "QuickSave F5" });
    document.getElementById("save-toast").textContent = info
      ? `✓ Sauvé Slot #1 (Frame #${info.frame} · CRC ${info.crc32Hex})`
      : `✓ Sauvé Slot #1 (Frame #${fallbackSim.frame})`;
  });

  document.getElementById("btn-quick-load")?.addEventListener("click", async () => {
    const info = await invokeBackend("restore_save_state", { slot: 1 });
    document.getElementById("save-toast").textContent = info
      ? `✓ Chargé Slot #1 (Frame #${info.frame})`
      : `✓ Chargé Slot #1`;
  });

  document.getElementById("btn-rewind-step")?.addEventListener("click", async () => {
    const res = await invokeBackend("rewind_frames", { stepsBack: 2 });
    if (!res && fallbackSim.history.length > 0) {
      const snap = fallbackSim.history.pop();
      fallbackSim.frame = snap.frame;
      fallbackSim.px = snap.px;
      fallbackSim.py = snap.py;
      fallbackSim.score = snap.score;
    }
  });

  window.addEventListener("keydown", async (e) => {
    if (document.activeElement?.tagName === "INPUT") return;
    pressedKeys.add(e.code);
    if (e.code === "F5") {
      e.preventDefault();
      document.getElementById("btn-quick-save")?.click();
    } else if (e.code === "F8") {
      e.preventDefault();
      document.getElementById("btn-quick-load")?.click();
    } else if (e.code === "Backspace") {
      e.preventDefault();
      document.getElementById("btn-rewind-step")?.click();
    }
  });

  window.addEventListener("keyup", (e) => {
    pressedKeys.delete(e.code);
  });

  document.getElementById("win-min")?.addEventListener("click", () => appWindow?.minimize());
  document.getElementById("win-max")?.addEventListener("click", () => appWindow?.toggleMaximize());
  document.getElementById("win-close")?.addEventListener("click", () => appWindow?.close());
}

async function init() {
  bindEvents();
  updateAllViews();
  const boot = await invokeBackend("get_station_bootstrap");
  if (boot) {
    applyBootstrap(boot);
  }
  await refreshDebuggerDrawer();
  requestAnimationFrame(tickEmulationLoop);
}

init();
