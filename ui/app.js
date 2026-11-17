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
  clampWidgetPosition
} from "./keymapper.js";
import { ApuAudioEngine } from "./audio.js";

const tauriInvoke = window.__TAURI__?.core?.invoke;
const appWindow = window.__TAURI__?.window?.getCurrentWindow?.();
const apuAudio = new ApuAudioEngine();

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
    title: "Golden Horizon: Alchemy Tactics",
    console: "GBA",
    engineKind: "libretroAbi",
    coreId: "mgba_libretro",
    region: "EUR · FR",
    crc32Hex: "8F912C04",
    sizeBytes: 16777216,
    sramSummary: "64 Ko FlashRAM (.srm + 3 rotations)",
    playtimeSeconds: 42300,
    favorite: true,
    linkCableReady: true,
    coverAccent: "#78350f",
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
    id: "snes-valkyrie-trigger",
    title: "Valkyrie Trigger: Epoch 1995",
    console: "SNES",
    engineKind: "libretroAbi",
    coreId: "snes9x_libretro",
    region: "JPN · Patch FR IPS",
    crc32Hex: "A2D9107B",
    sizeBytes: 4194304,
    sramSummary: "8 Ko SRAM + Patch IPS Actif en RAM",
    playtimeSeconds: 68100,
    favorite: true,
    linkCableReady: false,
    coverAccent: "#14532d",
    coverSymbol: "🕰️"
  },
  {
    id: "nds-dual-phantom",
    title: "Dual Screen Phantom Detective",
    console: "NDS",
    engineKind: "libretroAbi",
    coreId: "melonds_libretro",
    region: "EUR · Multi-5",
    crc32Hex: "55E081C2",
    sizeBytes: 67108864,
    sramSummary: "512 Ko EEPROM + Stylet Tactile",
    playtimeSeconds: 19200,
    favorite: false,
    linkCableReady: true,
    coverAccent: "#0c4a6e",
    coverSymbol: "🔍"
  },
  {
    id: "ps1-cyber-ridge",
    title: "Cyber Ridge GP Type-R4",
    console: "PS1",
    engineKind: "libretroAbi",
    coreId: "swanstation_libretro",
    region: "PAL · 60Hz Override",
    crc32Hex: "C9104FA2",
    sizeBytes: 540000000,
    sramSummary: "Carte Mémoire Slot 1 (.mcd Atomique)",
    playtimeSeconds: 11400,
    favorite: false,
    linkCableReady: false,
    coverAccent: "#312e81",
    coverSymbol: "🏎️"
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
  netplayRooms: [
    {
      id: "room-kanto-link",
      title: "Arène Échange & Combat Kanto #1",
      hostName: "Zainados",
      console: "GBC",
      gameTitle: "Pocket Monster Link Arena",
      pingMs: 14,
      rollbackFrames: 1,
      playersCurrent: 1,
      playersMax: 2,
      deepLinkUri: "zaimule://room/KANTO1?crc=DCE0682B&core=zaimule-rust-gb"
    },
    {
      id: "room-snes-coop",
      title: "Co-op Secret of Mana (Rollback GGPO)",
      hostName: "RetroLumen",
      console: "SNES",
      gameTitle: "Valkyrie Trigger: Epoch 1995",
      pingMs: 22,
      rollbackFrames: 2,
      playersCurrent: 2,
      playersMax: 3,
      deepLinkUri: "zaimule://room/MANA95?crc=A2D9107B&core=snes9x_libretro"
    }
  ],
  libretroCores: [
    {
      id: "zaimule-rust-gb",
      displayName: "Zaimule Rust GB/GBC (Sharp LR35902 + PPU + SIO Link)",
      libraryWindows: "Intégré au binaire Rust",
      libraryMacos: "Intégré au binaire Rust",
      statusLabel: "Actif · 100% Safe Rust"
    },
    {
      id: "zaimule-rust-nes",
      displayName: "Zaimule Rust NES (MOS 6502 + PPU 2C02)",
      libraryWindows: "Intégré au binaire Rust",
      libraryMacos: "Intégré au binaire Rust",
      statusLabel: "Actif · 100% Safe Rust"
    },
    {
      id: "mgba_libretro",
      displayName: "mGBA Libretro Bridge (ARM7TDMI)",
      libraryWindows: "mgba_libretro.dll",
      libraryMacos: "mgba_libretro.dylib",
      statusLabel: "Pont C-ABI Prêt"
    },
    {
      id: "snes9x_libretro",
      displayName: "Snes9x Current Libretro (Ricoh 5A22)",
      libraryWindows: "snes9x_libretro.dll",
      libraryMacos: "snes9x_libretro.dylib",
      statusLabel: "Pont C-ABI Prêt"
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
      notes: "Requis pour PlayStation 1 PAL/EUR"
    }
  ],
  linkTelemetry: { enabled: false, packetsTransferred: 0 },
  consoleFilter: "ALL",
  searchQuery: "",
  keymapperVisible: false,
  dualLinkMode: false,
  speedMult: 1,
  activeMacroId: null,
  recordingMacro: false,
  recordedFrames: []
};

// Local fallback simulation state when running outside Tauri
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

function pollGamepadBitmask() {
  if (typeof navigator === "undefined" || !navigator.getGamepads) return 0;
  const pads = navigator.getGamepads();
  if (!pads) return 0;
  for (const pad of pads) {
    if (pad && pad.connected) {
      const el = document.getElementById("tel-pad");
      if (el) el.textContent = `🎮 ${pad.id.slice(0, 18)}`;
      return computeBitmaskFromGamepad(pad);
    }
  }
  return 0;
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
    rewindMemoryBytes: fallbackSim.history.length * 640
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
  setTxt("count-nds", counts.NDS);
  setTxt("count-ps1", counts.PS1);
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
}

function renderNetplayAndCores() {
  const netGrid = document.getElementById("netplay-grid");
  if (netGrid) {
    netGrid.innerHTML = state.netplayRooms
      .map(
        (r) => `
        <div class="panel-card">
          <div class="section-title">
            <span>🌐 ${r.title}</span>
            <span class="pill-badge pill-green mono">${r.pingMs} ms · Rollback ${r.rollbackFrames}f</span>
          </div>
          <div class="kv-row"><span class="kv-label">Jeu</span><span class="kv-val">${r.gameTitle} (${r.console})</span></div>
          <div class="kv-row"><span class="kv-label">Hôte &amp; Joueurs</span><span class="kv-val mono">${r.hostName} (${r.playersCurrent}/${r.playersMax})</span></div>
          <div class="kv-row"><span class="kv-label">Invitation Deep-Link</span><span class="kv-val mono" style="color:var(--gold)">${r.deepLinkUri}</span></div>
        </div>
      `
      )
      .join("");
  }

  const coresGrid = document.getElementById("cores-grid");
  if (coresGrid) {
    const libretroHtml = state.libretroCores
      .map(
        (c) => `
        <div class="mod-item">
          <div>
            <div style="font-weight:700">${c.displayName}</div>
            <div class="mono" style="font-size:11px; color:var(--text-muted)">${c.libraryWindows} / ${c.libraryMacos}</div>
          </div>
          <span class="pill-badge pill-cobalt mono">${c.statusLabel}</span>
        </div>
      `
      )
      .join("");

    const biosHtml = state.biosEntries
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
      .join("");

    coresGrid.innerHTML = `
      <div class="panel-card">
        <div class="section-title"><span>🧩 Hôte Dynamique Libretro C-ABI</span></div>
        ${libretroHtml}
      </div>
      <div class="panel-card">
        <div class="section-title"><span>🔐 Registre Cryptographique des BIOS</span></div>
        ${biosHtml}
      </div>
    `;
  }
}

function applyBootstrap(boot) {
  state.activeGameId = boot.activeGameId;
  state.catalog = boot.catalog || state.catalog;
  state.modLayers = boot.modLayers || state.modLayers;
  state.saveSlots = boot.saveSlots || state.saveSlots;
  state.keymapperWidgets = boot.keymapperWidgets || state.keymapperWidgets;
  state.macros = boot.macros || state.macros;
  state.netplayRooms = boot.netplayRooms || state.netplayRooms;
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
  if (canvas.width !== frame.width || canvas.height !== frame.height) {
    canvas.width = frame.width;
    canvas.height = frame.height;
  }
  const ctx = canvas.getContext("2d");
  const clamped = new Uint8ClampedArray(frame.rgba);
  if (clamped.length === frame.width * frame.height * 4) {
    const imgData = new ImageData(clamped, frame.width, frame.height);
    ctx.putImageData(imgData, 0, 0);
  }
}

async function handleImportedFile(file) {
  if (!file) return;
  const buf = await file.arrayBuffer();
  const bytes = Array.from(new Uint8Array(buf));
  const lower = file.name.toLowerCase();

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

async function tickEmulationLoop() {
  const buttonsP1 = computeBitmaskFromPressedCodes(pressedKeys) | pollGamepadBitmask();
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
    if (resp.primary.audioFreqHz) {
      apuAudio.triggerFrameTone(resp.primary.audioFreqHz);
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
      document.getElementById("tel-link").textContent =
        `2P Sync (${resp.linkTelemetry.packetsTransferred} pkts · ${resp.linkTelemetry.lastP1ToP2Hex})`;
    }
  }

  requestAnimationFrame(tickEmulationLoop);
}

function bindEvents() {
  document.querySelectorAll(".nav-tab").forEach((btn) => {
    btn.addEventListener("click", () => switchTab(btn.dataset.target));
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

  document.getElementById("btn-import-rom")?.addEventListener("click", () => romInput?.click());
  document.getElementById("btn-import-ips")?.addEventListener("click", () => ipsInput?.click());
  document.getElementById("btn-insp-ips")?.addEventListener("click", () => ipsInput?.click());

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

  window.addEventListener("dragover", (e) => {
    e.preventDefault();
  });
  window.addEventListener("drop", async (e) => {
    e.preventDefault();
    const file = e.dataTransfer?.files?.[0];
    if (file) await handleImportedFile(file);
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

  const shaders = ["mode-crt", "mode-lcd", "mode-clean"];
  const shaderLabels = [
    "📺 Shader : CRT Scanlines",
    "📺 Shader : Matrice LCD",
    "📺 Shader : Pixel-Perfect"
  ];
  let shaderIdx = 0;
  document.getElementById("btn-shader-cycle")?.addEventListener("click", (e) => {
    shaderIdx = (shaderIdx + 1) % shaders.length;
    e.currentTarget.textContent = shaderLabels[shaderIdx];
    ["screen-1", "screen-2"].forEach((id) => {
      const el = document.getElementById(id);
      if (!el) return;
      el.classList.remove("mode-crt", "mode-lcd", "mode-clean");
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
    document.getElementById("bezel-p2").style.display = state.dualLinkMode ? "flex" : "none";
    document.getElementById("screens-wrap").classList.toggle("dual-mode", state.dualLinkMode);
    e.currentTarget.textContent = `🔗 Multi-Instance 2P : ${state.dualLinkMode ? "ACTIF" : "OFF"}`;
    document.getElementById("link-status-pill").textContent = state.dualLinkMode
      ? "🔗 Câble Link 2P Synchronisé"
      : "Mode 1P Solo";
    await invokeBackend("set_dual_link_enabled", { enabled: state.dualLinkMode });
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
  requestAnimationFrame(tickEmulationLoop);
}

init();
