import {
  filterCatalogEntries,
  computeConsoleCounts,
  formatPlaytime,
  formatFileSize,
  formatSwitchEngineLabel
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
import { decodeViewportBinaryPacket } from "./viewport_ipc.js";

const tauriInvoke = window.__TAURI__?.core?.invoke;
const appWindow = window.__TAURI__?.window?.getCurrentWindow?.();
const apuAudio = new ApuAudioEngine();
const viewportRecorder = new ViewportRecorder();
const canvasImageCache = new Map();

let toastTimer = null;
let lastSurfacedWarning = "";
let stepInFlight = false;

let state = {
  activeGameId: "",
  catalog: [],
  isEmulationRunning: false,
  currentFrame: 0,
  modLayers: [],
  cheatCodes: [],
  memoryWatchpoints: [],
  achievements: [],
  romDirectories: [],
  lastScanReport: null,
  warning: null,
  nativeCores: [
    {
      coreId: "zaimule-rust-gb",
      name: "Game Boy / Game Boy Color Natif",
      consoles: "GB · GBC",
      engine: "termboy-gb (Rust MIT)",
      audioSummary: "APU 4 canaux 48 kHz PCM stéréo + Câble Link",
      limitations: null
    },
    {
      coreId: "zaimule-rust-gba",
      name: "Game Boy Advance Natif",
      consoles: "GBA",
      engine: "termboy-gba (ARM7TDMI Rust MIT)",
      audioSummary: "PSG + Direct Sound DMA FIFO 48 kHz stéréo",
      limitations: "Correction LCD active ; BIOS HLE intégré."
    },
    {
      coreId: "zaimule-rust-nds",
      name: "Nintendo DS Natif",
      consoles: "NDS",
      engine: "nds_core (x0cero/nds Rust MIT + extensions SPU/3D/2D)",
      audioSummary: "SPU 16 canaux (PCM8/PCM16/ADPCM/PSG/Bruit) 48 kHz stéréo",
      limitations: "Stylet tactile inférieur, textures 3D 4×4, Decal/Toon & PPU 2D complet."
    }
  ],
  switchVault: {
    vaultRoot: "",
    torzu: {
      target: "torzu",
      installed: false,
      engineDir: "",
      executablePath: null,
      versionHint: null,
      shaderCacheBytes: 0,
      notes: "Importez votre dossier, exécutable ou archive .zip/.7z Torzu (Switch 1)."
    },
    fritchii: {
      target: "fritchii",
      installed: false,
      engineDir: "",
      executablePath: null,
      versionHint: null,
      shaderCacheBytes: 0,
      notes: "Importez votre build, exécutable ou archive .zip/.7z Fritchii / Lichii (Switch 2)."
    },
    keys: {
      keysDir: "",
      prodKeysFound: false,
      titleKeysFound: false,
      prodKeysPath: null,
      titleKeysPath: null
    }
  },
  saveSlots: [
    { slot: 1, occupied: false, frame: 0, crc32Hex: "00000000", label: "Slot #1 Vide" },
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
    { id: "km-btn-a", label: "J [A]", keyCode: "KeyJ", buttonMask: BTN_A, xPct: 82, yPct: 64, turboHz: 20 },
    { id: "km-btn-b", label: "K [B]", keyCode: "KeyK", buttonMask: BTN_B, xPct: 70, yPct: 74, turboHz: null }
  ],
  macros: [],
  tasTimeline: [],
  netplayRooms: [],
  udpLocalPort: 54920,
  stunCandidate: null,
  netplayChat: [],
  communityMods: [],
  ghostLeaderboard: [],
  cpuOverclockPct: 100,
  pgxpSubpixel: true,
  sramHexInspection: {
    totalBytes: 0,
    crc32Hex: "00000000",
    offset: 0,
    rows: []
  },
  tournamentBracket: null,
  biosEntries: [
    {
      console: "GB",
      fileName: "dmg_boot.bin",
      expectedCrc32Hex: "59C8598E",
      notes: "Séquence de démarrage DMG/CGB Rust intégrée (termboy-gb 0.5.0)."
    },
    {
      console: "GBA",
      fileName: "gba_bios.bin",
      expectedCrc32Hex: "81977335",
      notes: "BIOS GBA HLE Rust intégré (termboy-gba 0.5.0, SWI + DMA complets)."
    },
    {
      console: "NDS",
      fileName: "bios7.bin / bios9.bin",
      expectedCrc32Hex: "DF692A80",
      notes: "BIOS HLE ARM9/ARM7 + key1.bin + firmware synthétique Zaimule (SPU 16 canaux 48 kHz + 3D 4×4/Toon/Decal + PPU affine/étendu)."
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

const pressedKeys = new Set();

function escapeHtml(raw) {
  return String(raw ?? "")
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#39;");
}

function showToast(message, kind = "info", durationMs = 5400) {
  const text = String(message || "").trim();
  if (!text) return;
  const toastEl = document.getElementById("app-toast");
  if (toastEl) {
    toastEl.textContent = text;
    toastEl.classList.remove("error", "ok");
    if (kind === "error") toastEl.classList.add("error");
    else if (kind === "ok") toastEl.classList.add("ok");
    toastEl.classList.add("visible");
    if (toastTimer) clearTimeout(toastTimer);
    toastTimer = setTimeout(() => {
      toastEl.classList.remove("visible");
    }, durationMs);
  }
  const dockToast = document.getElementById("save-toast");
  if (dockToast) {
    dockToast.textContent = text;
  }
}

async function invokeBackend(cmd, args = {}, options = {}) {
  if (!tauriInvoke) return null;
  try {
    return await tauriInvoke(cmd, args);
  } catch (err) {
    const msg = typeof err === "string" ? err : err?.message || String(err);
    console.error(`[zaimule] ${cmd} failed:`, err);
    if (!options.silent) {
      showToast(msg, "error", 6800);
    }
    return null;
  }
}

function getSelectedGame() {
  return state.catalog.find((g) => g.id === state.activeGameId) || state.catalog[0] || null;
}

function isSwitchGame(game) {
  if (!game) return false;
  const c = String(game.console || "").toUpperCase();
  return c === "SWITCH" || c === "SWITCH2" || game.engineKind === "externalLauncher";
}

function resolveTargetEngineForGame(game) {
  if (!game) return "torzu";
  const override = String(game.switchEngine || "auto").toLowerCase();
  if (override === "torzu" || override === "fritchii") {
    return override;
  }
  return String(game.console || "").toUpperCase() === "SWITCH2" ? "fritchii" : "torzu";
}

async function handleImportRomsTrigger() {
  if (tauriInvoke) {
    const boot = await invokeBackend("pick_and_import_roms");
    if (boot) {
      applyBootstrap(boot);
      const added = boot.lastScanReport?.importedEntries?.length || 0;
      if (added > 0) {
        showToast(`${added} jeu(x) importé(s) dans la bibliothèque.`, "ok");
      }
    }
    return;
  }
  document.getElementById("rom-file-input")?.click();
}

async function handleScanFolderTrigger() {
  if (!tauriInvoke) {
    showToast("La sélection de dossier requiert l'application desktop Zaimule.", "error");
    return;
  }
  const boot = await invokeBackend("pick_and_scan_folder");
  if (boot) {
    applyBootstrap(boot);
    const added = boot.lastScanReport?.importedEntries?.length || 0;
    showToast(
      added > 0
        ? `${added} jeu(x) trouvé(s) dans le dossier scanné.`
        : "Dossier scanné (aucun nouveau jeu reconnu).",
      added > 0 ? "ok" : "info"
    );
  }
}

async function handleImportSwitchEngine(target, mode, buttonEl = null) {
  if (!tauriInvoke) {
    showToast("L'importation d'un émulateur Switch requiert l'application desktop Zaimule.", "error");
    return;
  }
  const prevText = buttonEl ? buttonEl.textContent : "";
  if (buttonEl) {
    buttonEl.disabled = true;
    buttonEl.textContent = "Importation...";
  }
  const label = target === "fritchii" ? "Fritchii (Switch 2)" : "Torzu (Switch 1)";
  showToast(`Importation de ${label} dans le coffre isolé...`, "info", 8000);
  const boot = await invokeBackend("pick_and_import_switch_engine", { target, mode });
  if (buttonEl) {
    buttonEl.disabled = false;
    buttonEl.textContent = prevText;
  }
  if (boot) {
    applyBootstrap(boot);
    const install = target === "fritchii" ? boot.switchVault?.fritchii : boot.switchVault?.torzu;
    if (install?.installed) {
      showToast(`${label} prêt (${install.versionHint || "binaire vérifié"}).`, "ok");
    }
  }
}

async function handleImportSwitchKeys(buttonEl = null) {
  if (!tauriInvoke) {
    showToast("L'importation de clés Switch (.keys) requiert l'application desktop Zaimule.", "error");
    return;
  }
  const prevText = buttonEl ? buttonEl.textContent : "";
  if (buttonEl) {
    buttonEl.disabled = true;
    buttonEl.textContent = "Importation...";
  }
  const boot = await invokeBackend("pick_and_import_switch_keys");
  if (buttonEl) {
    buttonEl.disabled = false;
    buttonEl.textContent = prevText;
  }
  if (boot) {
    applyBootstrap(boot);
    if (boot.switchVault?.keys?.prodKeysFound) {
      showToast("Clés Switch synchronisées dans le coffre isolé.", "ok");
    }
  }
}

async function handleClearSwitchShaders(target, buttonEl = null) {
  if (!tauriInvoke) {
    showToast("Le nettoyage du cache shaders requiert l'application desktop Zaimule.", "error");
    return;
  }
  const prevText = buttonEl ? buttonEl.textContent : "";
  if (buttonEl) {
    buttonEl.disabled = true;
    buttonEl.textContent = "Nettoyage...";
  }
  const res = await invokeBackend("clear_switch_shader_cache", { target });
  if (buttonEl) {
    buttonEl.disabled = false;
    buttonEl.textContent = prevText;
  }
  if (res) {
    if (res.bootstrap) applyBootstrap(res.bootstrap);
    const label = target === "fritchii" ? "Fritchii" : "Torzu";
    showToast(
      `Cache shaders ${label} vidé (${formatFileSize(res.clearedBytes || 0)} libérés).`,
      "ok"
    );
  }
}

function hideEmuBanner() {
  const banner = document.getElementById("emu-external-banner");
  if (banner) {
    banner.style.display = "none";
    banner.classList.remove("info");
    banner.dataset.modeKey = "";
  }
}

function showEmuBanner(modeKey, title, desc, actions = [], variant = "warn") {
  const banner = document.getElementById("emu-external-banner");
  if (!banner) return;
  banner.style.display = "flex";
  banner.classList.toggle("info", variant === "info");
  if (banner.dataset.modeKey === modeKey) return;
  banner.dataset.modeKey = modeKey;

  const titleEl = document.getElementById("emu-banner-title");
  const descEl = document.getElementById("emu-banner-desc");
  const actionsEl = document.getElementById("emu-banner-actions");
  if (titleEl) titleEl.textContent = title;
  if (descEl) descEl.textContent = desc;
  if (actionsEl) {
    actionsEl.innerHTML = "";
    for (const act of actions) {
      const btn = document.createElement("button");
      btn.type = "button";
      btn.className = `btn ${act.primary ? "btn-primary" : ""}`;
      btn.textContent = act.label;
      btn.addEventListener("click", () => act.onClick(btn));
      actionsEl.appendChild(btn);
    }
  }
}

function syncViewportScreenAspect(game) {
  const isNds = Boolean(game && String(game.console).toUpperCase() === "NDS");
  ["screen-1", "screen-2", "screen-3", "screen-4"].forEach((id) => {
    document.getElementById(id)?.classList.toggle("mode-nds", isNds);
  });
}

async function launchSelectedGame() {
  const game = getSelectedGame();
  if (!game) {
    await handleImportRomsTrigger();
    return;
  }

  if (isSwitchGame(game)) {
    state.isEmulationRunning = false;
    if (!tauriInvoke) {
      showToast(
        "Le lancement Switch (Torzu / Fritchii) requiert l'application desktop Zaimule.",
        "error"
      );
      return;
    }
    const status = await invokeBackend("launch_switch_game", { gameId: game.id });
    if (status) {
      showToast(status, "ok", 6500);
    } else {
      switchTab("view-cores");
    }
    return;
  }

  apuAudio.ensureContext();
  apuAudio.reset();

  const consoleKey = String(game.console || "").toUpperCase();
  if (consoleKey === "NDS") {
    showEmuBanner(
      `nds-info-${game.id}`,
      "Nintendo DS Natif (nds_core Rust)",
      "Exécution locale ARM9/ARM7 256×384 : stylet tactile inférieur, audio SPU 16 canaux 48 kHz, moteur 3D (textures 4×4, Decal/Toon) et PPU 2D affine/étendu.",
      [
        {
          label: "Masquer",
          onClick: () => hideEmuBanner()
        }
      ],
      "info"
    );
  } else {
    hideEmuBanner();
  }

  syncViewportScreenAspect(game);
  state.isEmulationRunning = true;
  const fpsEl = document.getElementById("tel-fps");
  if (fpsEl) fpsEl.textContent = `${(60 * state.speedMult).toFixed(0)} FPS`;
  const ftEl = document.getElementById("tel-ft");
  if (ftEl) ftEl.textContent = `${(16.6 / state.speedMult).toFixed(1)} ms`;
  switchTab("view-emulator");
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
    if (el) el.textContent = pad.id.slice(0, 18);
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
    if (state.catalog.length === 0) {
      if (action === "LAUNCH_GAME") {
        await launchSelectedGame();
      }
      return;
    }
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
      await launchSelectedGame();
      triggerGamepadRumble(getConnectedGamepad(), 90, 0.5, 0.85);
    }
  } else if (action === "BACK_LIBRARY" && activePane !== "view-emulator") {
    switchTab("view-library");
  }
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
  setTxt("count-nds", counts.NDS);
  setTxt("count-switch", counts.SWITCH);
  setTxt("count-switch2", counts.SWITCH2);
  setTxt("count-rust", counts.RUST);
  setTxt("count-link", counts.LINK);
}

const EDITORIAL_COVER_PALETTE = {
  GB: {
    bg: "#1F2A33",
    spine: "DMG-GB",
    svg: `<svg viewBox="0 0 64 64"><circle cx="32" cy="32" r="20"/><circle cx="32" cy="32" r="7"/><line x1="12" y1="32" x2="25" y2="32"/><line x1="39" y1="32" x2="52" y2="32"/></svg>`
  },
  GBC: {
    bg: "#21323F",
    spine: "CGB-GBC",
    svg: `<svg viewBox="0 0 64 64"><circle cx="32" cy="32" r="20"/><circle cx="32" cy="32" r="7"/><line x1="12" y1="32" x2="25" y2="32"/><line x1="39" y1="32" x2="52" y2="32"/></svg>`
  },
  GBA: {
    bg: "#3A271C",
    spine: "AGB-GBA",
    svg: `<svg viewBox="0 0 64 64"><circle cx="32" cy="32" r="18"/><path d="M32 8v10M32 46v10M8 32h10M46 32h10M15 15l7 7M42 42l7 7M49 15l-7 7M22 42l-7 7"/></svg>`
  },
  NDS: {
    bg: "#1F2C38",
    spine: "NTR-NDS",
    svg: `<svg viewBox="0 0 64 64"><rect x="16" y="10" width="32" height="19" rx="2"/><rect x="16" y="35" width="32" height="19" rx="2"/><circle cx="32" cy="44.5" r="4"/></svg>`
  },
  SWITCH: {
    bg: "#4A181D",
    spine: "HAC-NSW",
    svg: `<svg viewBox="0 0 64 64"><rect x="14" y="12" width="14" height="40" rx="6"/><rect x="36" y="12" width="14" height="40" rx="6"/><circle cx="21" cy="24" r="3"/><circle cx="43" cy="38" r="3"/></svg>`
  },
  SWITCH2: {
    bg: "#11312F",
    spine: "NX2-NS2",
    svg: `<svg viewBox="0 0 64 64"><rect x="12" y="12" width="14" height="40" rx="6"/><rect x="38" y="12" width="14" height="40" rx="6"/><circle cx="19" cy="24" r="3"/><circle cx="45" cy="38" r="3"/><path d="M29 24h6l-6 12h6"/></svg>`
  }
};

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

  if (filtered.length === 0) {
    const emptyEl = document.createElement("div");
    emptyEl.className = "empty-catalog-state";
    if (state.catalog.length === 0) {
      emptyEl.innerHTML = `
        <div class="empty-catalog-icon" aria-hidden="true">
          <svg viewBox="0 0 24 24"><rect x="4" y="3" width="16" height="18" rx="2"/><path d="M8 7h8M9 14h6M12 11v6"/></svg>
        </div>
        <div class="empty-catalog-title">Aucune cartouche ou image dans votre bibliothèque</div>
        <div class="empty-catalog-desc">
          Importez vos jeux <strong>Game Boy / GBC</strong> (.gb, .gbc), <strong>GBA</strong> (.gba), <strong>Nintendo DS</strong> (.nds),
          <strong>Switch 1</strong> (.nsp, .xci, .nsz) ou <strong>Switch 2</strong> (.nx, .nup) — y compris compressés en <strong>.zip</strong> ou <strong>.7z</strong> —
          ou glissez-déposez vos fichiers directement dans cette fenêtre.
        </div>
        <div class="empty-catalog-actions">
          <button class="btn btn-primary" id="btn-empty-import-rom" type="button">Importer ROM / Archive...</button>
          <button class="btn" id="btn-empty-scan-folder" type="button">Scanner un dossier...</button>
          <button class="btn btn-ghost" id="btn-empty-open-vault" type="button">Configurer Coffre Switch (Torzu / Fritchii)</button>
        </div>
      `;
      emptyEl.querySelector("#btn-empty-import-rom")?.addEventListener("click", () => {
        handleImportRomsTrigger();
      });
      emptyEl.querySelector("#btn-empty-scan-folder")?.addEventListener("click", () => {
        handleScanFolderTrigger();
      });
      emptyEl.querySelector("#btn-empty-open-vault")?.addEventListener("click", () => {
        switchTab("view-cores");
      });
    } else {
      emptyEl.innerHTML = `
        <div class="empty-catalog-title">Aucune ROM ne correspond au filtre actif</div>
        <div class="empty-catalog-desc">Modifiez votre recherche ou sélectionnez « Catalogue complet ».</div>
      `;
    }
    grid.appendChild(emptyEl);
    return;
  }

  for (const game of filtered) {
    const consoleKey = String(game.console || "GBC").toUpperCase();
    const spec = EDITORIAL_COVER_PALETTE[consoleKey] || EDITORIAL_COVER_PALETTE.GBC;
    const switchMode = isSwitchGame(game);
    const hasSpine = consoleKey === "SWITCH" || consoleKey === "SWITCH2";
    const engineClass = switchMode ? "core-external" : "core-rust";
    const engineLabel = switchMode
      ? resolveTargetEngineForGame(game) === "fritchii"
        ? "FRITCHII"
        : "TORZU"
      : "RUST";
    const archiveType = String(game.romPath || "").toLowerCase().endsWith(".7z")
      ? "7Z → ROM"
      : "ZIP → ROM";
    const subTag = game.titleId ? game.titleId : game.archiveEntry ? archiveType : "";

    const card = document.createElement("article");
    card.className = `game-card${game.id === state.activeGameId ? " selected" : ""}`;
    card.dataset.console = consoleKey;
    card.setAttribute("role", "listitem");
    card.tabIndex = 0;

    card.innerHTML = `
      <div class="game-jacket-stage">
        <div class="game-cover" style="--cover-bg: ${spec.bg};">
          <button class="game-remove-btn" type="button" title="Retirer de la bibliothèque" aria-label="Retirer ${escapeHtml(game.title)}">✕</button>
          ${
            hasSpine
              ? `<div class="cover-spine"><span class="cover-spine-label">${escapeHtml(spec.spine)}</span></div>`
              : ""
          }
          <div class="cover-top">
            <span class="console-tag mono">${escapeHtml(consoleKey === "SWITCH2" ? "SWITCH 2" : consoleKey)}</span>
            <span class="core-tag mono ${engineClass}">
              ${escapeHtml(engineLabel)}
            </span>
          </div>
          ${subTag ? `<div class="cover-subtag mono">${escapeHtml(subTag)}</div>` : ""}
          <div class="cover-art-graphic" aria-hidden="true">${spec.svg}</div>
          <span class="cover-action-prompt">[A] Lancer</span>
          <div class="cover-bottom mono">
            <span>${escapeHtml(game.crc32Hex)}${game.fastFingerprint ? " · FAST" : ""}</span>
            <span>${escapeHtml(formatFileSize(game.sizeBytes))}</span>
          </div>
        </div>
      </div>
      <div class="game-body">
        <div class="game-title" title="${escapeHtml(game.title)}">${escapeHtml(game.title)}</div>
        <div class="game-meta">
          <span>${escapeHtml(String(game.region || "N/A").split("(")[0].trim())}</span>
          <span class="mono">${escapeHtml(formatPlaytime(game.playtimeSeconds))}</span>
        </div>
      </div>
    `;

    card.querySelector(".game-remove-btn")?.addEventListener("click", async (e) => {
      e.stopPropagation();
      if (tauriInvoke) {
        const boot = await invokeBackend("remove_game_from_library", { gameId: game.id });
        if (boot) {
          applyBootstrap(boot);
          showToast(`${game.title} retiré de la bibliothèque.`);
        }
      } else {
        state.catalog = state.catalog.filter((item) => item.id !== game.id);
        if (state.activeGameId === game.id) {
          state.activeGameId = state.catalog[0]?.id || "";
        }
        updateAllViews();
      }
    });

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
      await launchSelectedGame();
    });
    grid.appendChild(card);
  }
}

function renderInspector() {
  const g = getSelectedGame();
  const playLabelEl = document.querySelector("#btn-insp-play span:first-child");
  const titleIdRow = document.getElementById("insp-titleid-row");
  const titleIdVal = document.getElementById("insp-titleid");
  const pathRow = document.getElementById("insp-path-row");
  const pathVal = document.getElementById("insp-path");
  const switchEngineRow = document.getElementById("insp-switch-engine-row");
  const switchEngineSel = document.getElementById("insp-switch-engine");
  const sramActionsRow = document.getElementById("sram-actions-row");

  if (g) {
    const consoleKey = String(g.console || "").toUpperCase();
    const isSwitch = isSwitchGame(g);
    const targetEngine = resolveTargetEngineForGame(g);

    document.getElementById("insp-title").textContent = g.title;
    document.getElementById("insp-console").textContent =
      consoleKey === "SWITCH2" ? "SWITCH 2" : consoleKey;
    document.getElementById("insp-core").textContent = isSwitch
      ? formatSwitchEngineLabel(g.switchEngine, consoleKey)
      : g.coreId;
    document.getElementById("insp-region").textContent = String(g.region || "—").split("(")[0].trim();
    document.getElementById("insp-crc").textContent = g.fastFingerprint
      ? `${g.crc32Hex} (Échantillon)`
      : g.crc32Hex;
    const crcTop = document.getElementById("insp-crc-top");
    if (crcTop) crcTop.textContent = `CRC · ${g.crc32Hex}`;
    document.getElementById("insp-sram").textContent = g.sramSummary;
    document.getElementById("insp-playtime").textContent = formatPlaytime(g.playtimeSeconds);
    document.getElementById("active-emu-game").textContent = `${consoleKey} · ${g.title}`;
    document.getElementById("tel-core").textContent = isSwitch
      ? `${targetEngine} (${consoleKey})`
      : `${g.coreId} (${consoleKey})`;

    if (titleIdRow && titleIdVal) {
      if (g.titleId) {
        titleIdRow.style.display = "flex";
        titleIdVal.textContent = g.titleId;
      } else {
        titleIdRow.style.display = "none";
      }
    }
    if (pathRow && pathVal) {
      if (g.romPath) {
        const displayPath = g.archiveEntry ? `${g.romPath} [${g.archiveEntry}]` : g.romPath;
        pathRow.style.display = "flex";
        pathVal.textContent = displayPath;
        pathVal.title = displayPath;
      } else {
        pathRow.style.display = "none";
      }
    }
    if (switchEngineRow && switchEngineSel) {
      if (isSwitch) {
        switchEngineRow.style.display = "flex";
        const autoLabel =
          consoleKey === "SWITCH2" ? "Auto (Fritchii · Switch 2)" : "Auto (Torzu · Switch 1)";
        const autoOpt = switchEngineSel.querySelector('option[value="auto"]');
        if (autoOpt) autoOpt.textContent = autoLabel;
        switchEngineSel.value = g.switchEngine || "auto";
      } else {
        switchEngineRow.style.display = "none";
      }
    }
    if (playLabelEl) {
      playLabelEl.textContent = isSwitch
        ? targetEngine === "fritchii"
          ? "Lancer avec Fritchii (Switch 2)"
          : "Lancer avec Torzu (Switch 1)"
        : "Lancer dans le Viewport 60 Hz";
    }
    if (sramActionsRow) {
      sramActionsRow.style.display = isSwitch ? "none" : "grid";
    }
    syncViewportScreenAspect(g);
  } else {
    document.getElementById("insp-title").textContent = "Aucune ROM sélectionnée";
    document.getElementById("insp-console").textContent = "—";
    document.getElementById("insp-core").textContent = "—";
    document.getElementById("insp-region").textContent = "—";
    document.getElementById("insp-crc").textContent = "00000000";
    const crcTop = document.getElementById("insp-crc-top");
    if (crcTop) crcTop.textContent = "CRC · —";
    document.getElementById("insp-sram").textContent = "Aucune mémoire active";
    document.getElementById("insp-playtime").textContent = "0m";
    document.getElementById("active-emu-game").textContent = "Aucune ROM active";
    document.getElementById("tel-core").textContent = "En veille";
    if (titleIdRow) titleIdRow.style.display = "none";
    if (pathRow) pathRow.style.display = "none";
    if (switchEngineRow) switchEngineRow.style.display = "none";
    if (playLabelEl) playLabelEl.textContent = "Lancer dans le Viewport 60 Hz";
    if (sramActionsRow) sramActionsRow.style.display = "grid";
    syncViewportScreenAspect(null);
  }

  const sramHexEl = document.getElementById("sram-hex-dump");
  if (sramHexEl) {
    const sramRows = state.sramHexInspection?.hexRows || state.sramHexInspection?.rows || [];
    if (sramRows.length > 0) {
      sramHexEl.innerHTML = sramRows.map((r) => `<div>${escapeHtml(r)}</div>`).join("");
    } else {
      sramHexEl.innerHTML = `<div class="empty-list-hint">Aucune donnée SRAM chargée</div>`;
    }
  }
  const sramCrcBadge = document.getElementById("sram-crc-badge");
  if (sramCrcBadge) {
    sramCrcBadge.textContent =
      state.sramHexInspection?.totalBytes > 0
        ? `CRC ${state.sramHexInspection.crc32Hex}`
        : "CRC —";
  }

  const ocBadge = document.getElementById("insp-overclock-badge");
  if (ocBadge) {
    ocBadge.textContent = `${state.cpuOverclockPct}% · PGXP ${state.pgxpSubpixel ? "ON" : "OFF"}`;
  }
  const ocBtn = document.getElementById("btn-overclock-cycle");
  if (ocBtn) {
    ocBtn.textContent = `CPU : ${state.cpuOverclockPct}% ${state.cpuOverclockPct > 100 ? "Turbo" : "Natif"}`;
  }
  const pgxpBtn = document.getElementById("btn-pgxp-toggle");
  if (pgxpBtn) {
    pgxpBtn.textContent = `PGXP 3D : ${state.pgxpSubpixel ? "ON" : "OFF"}`;
    pgxpBtn.style.color = state.pgxpSubpixel ? "var(--status-ok)" : "var(--text-secondary)";
  }

  const modsEl = document.getElementById("mods-list");
  if (modsEl) {
    modsEl.innerHTML = "";
    if (state.modLayers.length === 0) {
      modsEl.innerHTML = `<div class="empty-list-hint">Aucun patch .IPS actif</div>`;
    } else {
      for (const m of state.modLayers) {
        const row = document.createElement("label");
        row.className = "mod-item";
        row.innerHTML = `
          <span style="display:flex; align-items:center; gap:8px;">
            <input type="checkbox" ${m.enabled ? "checked" : ""} />
            <span>${escapeHtml(m.label)}</span>
          </span>
          <span class="mono" style="font-size:10.5px; color:${m.enabled ? "var(--status-ok)" : "var(--text-muted)"};">
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
  }

  const cheatsEl = document.getElementById("cheats-list");
  if (cheatsEl) {
    cheatsEl.innerHTML = "";
    if (state.cheatCodes.length === 0) {
      cheatsEl.innerHTML = `<div class="empty-list-hint">Aucun code GameShark / Action Replay</div>`;
    } else {
      for (const c of state.cheatCodes) {
        const row = document.createElement("label");
        row.className = "mod-item";
        row.innerHTML = `
          <span style="display:flex; align-items:center; gap:8px;">
            <input type="checkbox" ${c.enabled ? "checked" : ""} />
            <span>${escapeHtml(String(c.label || "").replace(/^[^\w]+/, ""))}</span>
          </span>
          <span class="mono" style="font-size:10.5px; color:var(--accent-primary);">${escapeHtml(c.codeText)}</span>
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
  }

  const achEl = document.getElementById("achievements-list");
  if (achEl) {
    achEl.innerHTML = "";
    if (state.achievements.length === 0) {
      achEl.innerHTML = `<div class="empty-list-hint">Aucun succès enregistré pour cette session</div>`;
    } else {
      for (const a of state.achievements) {
        const row = document.createElement("div");
        row.className = "mod-item";
        row.innerHTML = `
          <span>${escapeHtml(a.title)}</span>
          <span class="mono" style="font-size:10.5px; color:${a.unlocked ? "var(--status-ok)" : "var(--text-muted)"};">
            ${a.unlocked ? `+${a.points} pts` : `${a.points} pts`}
          </span>
        `;
        achEl.appendChild(row);
      }
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
        <div class="slot-time mono">${slot.occupied ? escapeHtml(slot.label) : "Vide — Cliquer pour sauver"}</div>
      `;
      card.addEventListener("click", async () => {
        if (!getSelectedGame()) return;
        if (slot.occupied) {
          const info = await invokeBackend("restore_save_state", { slot: slot.slot });
          document.getElementById("save-toast").textContent =
            `Slot #${slot.slot} chargé (${info?.crc32Hex || slot.crc32Hex})`;
        } else {
          const info = await invokeBackend("create_save_state", {
            slot: slot.slot,
            label: `Frame #${state.currentFrame}`
          });
          if (info) {
            const boot = await invokeBackend("get_station_bootstrap");
            if (boot) applyBootstrap(boot);
          } else {
            slot.occupied = true;
            slot.label = `Frame #${state.currentFrame}`;
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
    if (state.macros.length === 0) {
      macrosList.innerHTML = `<div class="empty-list-hint">Aucune macro TAS enregistrée</div>`;
    } else {
      for (const mac of state.macros) {
        const isRunning = state.activeMacroId === mac.id;
        const row = document.createElement("div");
        row.className = "mod-item";
        row.innerHTML = `
          <span>${escapeHtml(mac.name)}</span>
          <button class="btn mono" style="padding:3px 8px; font-size:11px;">
            ${isRunning ? "Stop" : "TAS"}
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

  const tasList = document.getElementById("tas-timeline-list");
  if (tasList) {
    tasList.innerHTML = "";
    const segments = (state.tasTimeline || []).slice(0, 4);
    if (segments.length === 0) {
      tasList.innerHTML = `<div class="empty-list-hint">Aucun segment TAS</div>`;
    } else {
      for (const seg of segments) {
        const row = document.createElement("div");
        row.className = "mod-item";
        row.style.fontSize = "10.5px";
        row.innerHTML = `
          <span>F${seg.startFrame}..${seg.endFrame} (${seg.durationFrames}f) · <strong style="color:var(--accent-secondary)">${escapeHtml(seg.buttonsLabel)}</strong></span>
          <button class="btn mono" style="padding:1px 6px; font-size:9.5px;">+A</button>
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
  }

  const wpList = document.getElementById("watchpoints-list");
  if (wpList) {
    wpList.innerHTML = "";
    const watchpoints = state.memoryWatchpoints || [];
    if (watchpoints.length === 0) {
      wpList.innerHTML = `<div class="empty-list-hint">Aucun watchpoint mémoire actif</div>`;
    } else {
      for (const wp of watchpoints) {
        const row = document.createElement("div");
        row.className = "mod-item";
        row.style.fontSize = "10.5px";
        const hexVal = `0x${(wp.currentValue ?? 0).toString(16).padStart(2, "0").toUpperCase()}`;
        row.innerHTML = `
          <span><strong style="color:var(--accent-primary)">${escapeHtml(wp.addrHex)}</strong> ${escapeHtml(wp.label)} : <strong style="color:var(--status-ok)">${hexVal}</strong></span>
          <button class="btn mono" style="padding:1px 6px; font-size:9.5px; color:${wp.frozen ? "var(--accent-secondary)" : "var(--text-muted)"};">
            ${wp.frozen ? "Gelé" : "Pin"}
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
}

function buildSwitchEngineBlock(install, target, displayName) {
  const installed = Boolean(install?.installed);
  const shaderText = formatFileSize(install?.shaderCacheBytes || 0);
  const block = document.createElement("div");
  block.className = "switch-vault-row";
  block.innerHTML = `
    <div class="switch-vault-head">
      <div style="font-weight:600; display:flex; align-items:center; gap:8px; flex-wrap:wrap;">
        <span>${escapeHtml(displayName)}</span>
        <span class="pill-badge ${installed ? "pill-green" : "pill-terracotta"} mono">
          ${installed ? `Prêt · ${escapeHtml(install.versionHint || "installé")}` : "À importer"}
        </span>
      </div>
      <span class="mono" style="font-size:10.5px; color:var(--text-muted);">Shaders : ${escapeHtml(shaderText)}</span>
    </div>
    <div class="mono path-ellipsis" style="font-size:10.5px; color:${installed ? "var(--status-ok)" : "var(--text-muted)"};" title="${escapeHtml(install?.executablePath || install?.engineDir || "")}">
      ${escapeHtml(installed && install.executablePath ? install.executablePath : install?.notes || "")}
    </div>
    <div class="vault-actions">
      <button class="btn btn-primary mono btn-vault-file" type="button" style="padding:4px 9px; font-size:11px;">
        ${installed ? "Remplacer (.zip / .7z / .exe)..." : "Importer Archive / .exe..."}
      </button>
      <button class="btn mono btn-vault-folder" type="button" style="padding:4px 9px; font-size:11px;">
        Importer Dossier...
      </button>
      <button class="btn btn-ghost mono btn-vault-clear-cache" type="button" style="padding:4px 9px; font-size:11px;">
        Vider Cache Shaders
      </button>
    </div>
  `;
  block.querySelector(".btn-vault-file")?.addEventListener("click", (ev) => {
    handleImportSwitchEngine(target, "file", ev.currentTarget);
  });
  block.querySelector(".btn-vault-folder")?.addEventListener("click", (ev) => {
    handleImportSwitchEngine(target, "folder", ev.currentTarget);
  });
  block.querySelector(".btn-vault-clear-cache")?.addEventListener("click", (ev) => {
    handleClearSwitchShaders(target, ev.currentTarget);
  });
  return block;
}

function renderNetplayAndCores() {
  const portBadge = document.getElementById("udp-port-badge");
  if (portBadge) {
    portBadge.textContent = `UDP ZML2 : 127.0.0.1:${state.udpLocalPort || 54920}`;
  }
  const stunBadge = document.getElementById("stun-endpoint-badge");
  if (stunBadge) {
    stunBadge.textContent = state.stunCandidate
      ? `STUN Hole-Punch : ${state.stunCandidate.publicEndpoint} (${state.stunCandidate.natType})`
      : "STUN Hole-Punch : En attente";
  }
  const specBtn = document.getElementById("btn-toggle-spectator");
  if (specBtn) {
    const active = state.tournamentBracket?.spectatorModeActive || false;
    const count =
      state.tournamentBracket?.spectatorCount ??
      state.tournamentBracket?.matches?.[0]?.spectatorsCount ??
      0;
    specBtn.textContent = `Spectateur (${count}) : ${active ? "ACTIF" : "OFF"}`;
  }

  const chatList = document.getElementById("netplay-chat-list");
  if (chatList) {
    chatList.innerHTML = "";
    const msgs = state.netplayChat || [];
    if (msgs.length === 0) {
      chatList.innerHTML = `<div class="empty-list-hint">Aucun message dans le salon</div>`;
    } else {
      for (const msg of msgs) {
        const sharedCrc = msg.sharedStateCrc32Hex || msg.saveStateCrcHex;
        const sharedSlot = msg.sharedStateSlot || 1;
        const row = document.createElement("div");
        row.className = "mono";
        row.style.cssText =
          "display:flex; align-items:center; justify-content:space-between; font-size:11px; padding:4px 0; border-bottom:1px solid var(--border-subtle);";
        row.innerHTML = `
          <span><strong style="color:var(--accent-secondary)">[${escapeHtml(msg.sender)}]</strong> ${escapeHtml(msg.text)}</span>
          ${
            sharedCrc
              ? `<button class="btn mono" style="padding:1px 6px; font-size:10px; border-color:var(--status-ok); color:var(--status-ok);">Charger State #${sharedSlot} (${escapeHtml(sharedCrc)})</button>`
              : ""
          }
        `;
        if (sharedCrc) {
          row.querySelector("button")?.addEventListener("click", async () => {
            await invokeBackend("restore_save_state", { slot: sharedSlot });
            launchSelectedGame();
          });
        }
        chatList.appendChild(row);
      }
    }
  }

  const netGrid = document.getElementById("netplay-grid");
  if (netGrid) {
    netGrid.innerHTML = "";

    if (state.tournamentBracket?.matches?.length > 0) {
      const t = state.tournamentBracket;
      const specTotal = t.spectatorCount ?? t.matches?.[0]?.spectatorsCount ?? 0;
      const tourneyCard = document.createElement("div");
      tourneyCard.className = "panel-card";
      tourneyCard.id = "tournament-bracket-card";
      tourneyCard.innerHTML = `
        <div class="section-title">
          <span>${escapeHtml(t.title)}</span>
          <span class="pill-badge pill-gold mono">
            ${t.champion ? `Champion : ${escapeHtml(t.champion)}` : `${specTotal} Spectateurs`}
          </span>
        </div>
        <div class="bracket-matches"></div>
      `;
      const matchesWrap = tourneyCard.querySelector(".bracket-matches");
      for (const m of t.matches || []) {
        const matchId = m.id ?? m.matchId;
        const mRow = document.createElement("div");
        mRow.className = "mod-item";
        mRow.innerHTML = `
          <div>
            <div class="mono" style="font-size:10.5px; color:var(--accent-primary)">${escapeHtml(m.roundLabel)}</div>
            <div style="font-weight:600">${escapeHtml(m.playerA)} <span style="color:var(--text-muted)">vs</span> ${escapeHtml(m.playerB)}</div>
            <div class="mono" style="font-size:10.5px; color:${m.winner ? "var(--status-ok)" : "var(--text-muted)"}">
              ${m.winner ? `Vainqueur : ${escapeHtml(m.winner)}` : `Score Live : ${m.scoreA} - ${m.scoreB}`}
            </div>
          </div>
          <div style="display:flex; gap:4px;">
            <button class="btn mono btn-win-a" style="padding:3px 7px; font-size:10px;">${escapeHtml(m.playerA)}</button>
            <button class="btn mono btn-win-b" style="padding:3px 7px; font-size:10px;">${escapeHtml(m.playerB)}</button>
          </div>
        `;
        mRow.querySelector(".btn-win-a")?.addEventListener("click", async () => {
          const boot = await invokeBackend("advance_tournament_match", {
            matchId,
            scoreA: 3,
            scoreB: 1
          });
          if (boot) applyBootstrap(boot);
        });
        mRow.querySelector(".btn-win-b")?.addEventListener("click", async () => {
          const boot = await invokeBackend("advance_tournament_match", {
            matchId,
            scoreA: 1,
            scoreB: 3
          });
          if (boot) applyBootstrap(boot);
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
          <span>${escapeHtml(r.title)}</span>
          <span class="pill-badge pill-green mono">${r.pingMs} ms · Rollback ${r.rollbackFrames}f</span>
        </div>
        <div class="kv-row"><span class="kv-label">Jeu</span><span class="kv-val">${escapeHtml(r.gameTitle)} (${escapeHtml(r.console)})</span></div>
        <div class="kv-row"><span class="kv-label">Hôte &amp; Joueurs</span><span class="kv-val mono">${escapeHtml(r.hostName)} (${r.playersCurrent}/${r.playersMax})</span></div>
        <div class="kv-row"><span class="kv-label">Invitation Deep-Link</span><span class="kv-val mono" style="color:var(--accent-secondary)">${escapeHtml(r.deepLinkUri)}</span></div>
        <div style="margin-top:10px; display:flex; justify-content:flex-end;">
          <button class="btn btn-primary mono" style="font-size:11px;">Synchroniser Salon &amp; Link 2P</button>
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
        launchSelectedGame();
      });
      netGrid.appendChild(card);
    }

    for (const ghost of state.ghostLeaderboard || []) {
      const card = document.createElement("div");
      card.className = "panel-card";
      card.innerHTML = `
        <div class="section-title">
          <span>Record Fantôme TAS · ${escapeHtml(ghost.playerName)}</span>
          <span class="pill-badge pill-cobalt mono">${ghost.score} pts · CRC ${escapeHtml(ghost.traceCrc32Hex)}</span>
        </div>
        <div class="kv-row"><span class="kv-label">Jeu &amp; Console</span><span class="kv-val">${escapeHtml(ghost.gameTitle)} (${escapeHtml(ghost.console)})</span></div>
        <div class="kv-row"><span class="kv-label">Trace Déterministe</span><span class="kv-val mono" style="color:var(--status-ok)">Vérifié (${ghost.framesCount} frames)</span></div>
        <div style="margin-top:10px; display:flex; justify-content:flex-end;">
          <button class="btn mono" style="font-size:11px; border-color:var(--accent-primary); color:var(--accent-primary);">Défier Fantôme TAS</button>
        </div>
      `;
      card.querySelector("button").addEventListener("click", async () => {
        const boot = await invokeBackend("challenge_ghost_run", { ghostId: ghost.id });
        if (boot) applyBootstrap(boot);
        document.getElementById("macro-status").textContent = "DÉFI FANTÔME TAS";
        launchSelectedGame();
      });
      netGrid.appendChild(card);
    }

    if (netGrid.children.length === 0) {
      const emptyCard = document.createElement("div");
      emptyCard.className = "panel-card";
      emptyCard.innerHTML = `
        <div class="section-title"><span>Salons P2P &amp; Fantômes TAS</span></div>
        <div class="empty-list-hint">Aucun salon P2P ou fantôme TAS actif. Créez un salon ou rejoignez une URI zaimule://room/.</div>
      `;
      netGrid.appendChild(emptyCard);
    }
  }

  const coresGrid = document.getElementById("cores-grid");
  if (coresGrid) {
    coresGrid.innerHTML = "";

    const vault = state.switchVault || {};
    const keys = vault.keys || {};
    const prodOk = Boolean(keys.prodKeysFound);
    const titleOk = Boolean(keys.titleKeysFound);

    // 1. Coffre Switch (Option A)
    const vaultCard = document.createElement("div");
    vaultCard.className = "panel-card";
    vaultCard.innerHTML = `
      <div class="section-title">
        <span>Coffre Switch (Option A : Torzu &amp; Fritchii)</span>
        <button class="btn mono" id="btn-refresh-vault" type="button" style="padding:2px 8px; font-size:10px;">Actualiser</button>
      </div>
      <div class="switch-vault-row">
        <div class="switch-vault-head">
          <div style="font-weight:600;">Clés Nintendo Switch (prod.keys / title.keys)</div>
          <span class="pill-badge ${prodOk ? "pill-green" : "pill-terracotta"} mono">
            ${prodOk ? (titleOk ? "prod + title OK" : "prod.keys OK") : "Clés requises"}
          </span>
        </div>
        <div class="mono path-ellipsis" style="font-size:10.5px; color:${prodOk ? "var(--status-ok)" : "var(--text-muted)"};" title="${escapeHtml(keys.prodKeysPath || keys.keysDir || "")}">
          ${
            prodOk
              ? `prod.keys : ${escapeHtml(keys.prodKeysPath)}`
              : "Importez manuellement vos fichiers prod.keys / title.keys (synchronisés à chaque lancement)."
          }
        </div>
        <div class="vault-actions">
          <button class="btn btn-primary mono" id="btn-import-switch-keys" type="button" style="padding:4px 9px; font-size:11px;">
            Importer Clés (.keys)...
          </button>
        </div>
      </div>
      <div id="vault-engines-wrap"></div>
      ${
        vault.vaultRoot
          ? `<div class="mono path-ellipsis" style="margin-top:8px; font-size:10px; color:var(--text-muted);" title="${escapeHtml(vault.vaultRoot)}">Coffre : ${escapeHtml(vault.vaultRoot)}</div>`
          : ""
      }
    `;
    vaultCard.querySelector("#btn-refresh-vault")?.addEventListener("click", async () => {
      const boot = await invokeBackend("refresh_switch_vault");
      if (boot) {
        applyBootstrap(boot);
        showToast("État du coffre Switch actualisé.", "ok");
      }
    });
    vaultCard.querySelector("#btn-import-switch-keys")?.addEventListener("click", (ev) => {
      handleImportSwitchKeys(ev.currentTarget);
    });
    const enginesWrap = vaultCard.querySelector("#vault-engines-wrap");
    if (enginesWrap) {
      enginesWrap.appendChild(
        buildSwitchEngineBlock(vault.torzu, "torzu", "Torzu (Nintendo Switch 1)")
      );
      enginesWrap.appendChild(
        buildSwitchEngineBlock(vault.fritchii, "fritchii", "Fritchii / Lichii (Switch 2)")
      );
    }

    // 2. Cœurs Natifs Rust Purs
    const nativeCard = document.createElement("div");
    nativeCard.className = "panel-card";
    nativeCard.innerHTML = `<div class="section-title"><span>Cœurs Natifs Rust Purs (Viewport 60 Hz)</span></div>`;
    for (const c of state.nativeCores || []) {
      const row = document.createElement("div");
      row.className = "mod-item";
      row.innerHTML = `
        <div style="min-width:0; flex:1;">
          <div style="font-weight:600; display:flex; align-items:center; gap:6px; flex-wrap:wrap;">
            <span>${escapeHtml(c.name)}</span>
            <span class="pill-badge mono" style="font-size:9.5px;">${escapeHtml(c.consoles)}</span>
          </div>
          <div class="mono" style="font-size:10.5px; color:var(--accent-primary);">${escapeHtml(c.engine)}</div>
          <div style="font-size:11px; color:var(--text-secondary);">${escapeHtml(c.audioSummary)}</div>
          ${
            c.limitations
              ? `<div style="font-size:10.5px; color:var(--accent-secondary); margin-top:2px;">${escapeHtml(c.limitations)}</div>`
              : ""
          }
        </div>
        <span class="pill-badge pill-green mono">ACTIF</span>
      `;
      nativeCard.appendChild(row);
    }

    // 3. Importation & Dossiers ROMs (manuel uniquement)
    const foldersCard = document.createElement("div");
    foldersCard.className = "panel-card";
    foldersCard.innerHTML = `
      <div class="section-title">
        <span>Importation &amp; Dossiers ROMs (Manuel)</span>
        <span class="mono" style="font-size:10px; color:var(--text-muted);">${state.romDirectories.length} dossier(s)</span>
      </div>
      <div class="vault-actions" style="margin-bottom:10px;">
        <button class="btn btn-primary" id="btn-cores-import-files" type="button">Importer fichiers...</button>
        <button class="btn" id="btn-cores-scan-folder" type="button">Scanner un dossier...</button>
      </div>
      ${
        state.romDirectories.length > 0
          ? `<div style="padding-top:8px; border-top:1px solid var(--border-subtle);">
               <div class="mono" style="font-size:10px; color:var(--text-muted); margin-bottom:4px;">DOSSIERS SCANNÉS</div>
               ${state.romDirectories
                 .map(
                   (dir) =>
                     `<div class="mono path-ellipsis" style="font-size:11px; color:var(--text-secondary); padding:2px 0;" title="${escapeHtml(dir)}">${escapeHtml(dir)}</div>`
                 )
                 .join("")}
             </div>`
          : `<div class="empty-list-hint">Aucun scan automatique au démarrage. Ajoutez vos fichiers ou dossiers manuellement, ou par glisser-déposer.</div>`
      }
      ${
        state.lastScanReport
          ? `<div class="mono" style="margin-top:10px; font-size:10.5px; color:var(--text-secondary);">
               Dernier import (${escapeHtml(state.lastScanReport.scannedRoot)}) : ${state.lastScanReport.importedEntries?.length || 0} importé(s) · ${state.lastScanReport.skippedFiles || 0} ignoré(s)
             </div>`
          : ""
      }
    `;
    foldersCard.querySelector("#btn-cores-import-files")?.addEventListener("click", () => {
      handleImportRomsTrigger();
    });
    foldersCard.querySelector("#btn-cores-scan-folder")?.addEventListener("click", () => {
      handleScanFolderTrigger();
    });

    // 4. Patchs .IPS Non-Destructifs
    const communityCard = document.createElement("div");
    communityCard.className = "panel-card";
    communityCard.innerHTML = `<div class="section-title"><span>Patchs .IPS Non-Destructifs</span></div>`;
    if ((state.communityMods || []).length === 0) {
      const hint = document.createElement("div");
      hint.className = "empty-list-hint";
      hint.textContent =
        "Aucun patch .IPS chargé. Utilisez « Patch .IPS » dans la barre supérieure pour appliquer un patch local sans modifier la ROM d'origine.";
      communityCard.appendChild(hint);
    } else {
      for (const mod of state.communityMods) {
        const row = document.createElement("div");
        row.className = "mod-item";
        row.innerHTML = `
          <div>
            <div style="font-weight:600">${escapeHtml(mod.title)}</div>
            <div style="font-size:11px; color:var(--text-muted)">${escapeHtml(mod.description)}</div>
            <div class="mono" style="font-size:10.5px; color:var(--accent-secondary)">Par ${escapeHtml(mod.author)} · Cible : ${escapeHtml(mod.targetConsole)}</div>
          </div>
          <button class="btn mono" style="padding:4px 10px; font-size:11px;">
            ${mod.installed ? "Patch .IPS Actif" : "Installer .IPS"}
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
              label: mod.title,
              enabled: true
            });
            updateAllViews();
          }
        });
        communityCard.appendChild(row);
      }
    }

    // 5. Registre Cryptographique des BIOS
    const biosCard = document.createElement("div");
    biosCard.className = "panel-card";
    biosCard.innerHTML = `
      <div class="section-title"><span>Registre Cryptographique des BIOS HLE</span></div>
      ${state.biosEntries
        .map(
          (b) => `
          <div class="mod-item">
            <div>
              <div class="mono" style="font-weight:600">${escapeHtml(b.fileName)} (${escapeHtml(b.console)})</div>
              <div style="font-size:11px; color:var(--text-muted)">${escapeHtml(b.notes)}</div>
            </div>
            <span class="pill-badge pill-green mono">CRC ${escapeHtml(b.expectedCrc32Hex)}</span>
          </div>
        `
        )
        .join("")}
    `;

    coresGrid.appendChild(vaultCard);
    coresGrid.appendChild(nativeCard);
    coresGrid.appendChild(foldersCard);
    coresGrid.appendChild(communityCard);
    coresGrid.appendChild(biosCard);
  }
}

function applyBootstrap(boot) {
  state.activeGameId = boot.activeGameId ?? "";
  state.catalog = Array.isArray(boot.catalog) ? boot.catalog : state.catalog;
  state.modLayers = Array.isArray(boot.modLayers) ? boot.modLayers : state.modLayers;
  state.cheatCodes = Array.isArray(boot.cheatCodes) ? boot.cheatCodes : state.cheatCodes;
  state.memoryWatchpoints = Array.isArray(boot.memoryWatchpoints)
    ? boot.memoryWatchpoints
    : state.memoryWatchpoints;
  state.achievements = Array.isArray(boot.achievements) ? boot.achievements : state.achievements;
  state.saveSlots = Array.isArray(boot.saveSlots) ? boot.saveSlots : state.saveSlots;
  state.keymapperWidgets = Array.isArray(boot.keymapperWidgets)
    ? boot.keymapperWidgets
    : state.keymapperWidgets;
  state.macros = Array.isArray(boot.macros) ? boot.macros : state.macros;
  state.tasTimeline = Array.isArray(boot.tasTimeline) ? boot.tasTimeline : state.tasTimeline;
  state.netplayRooms = Array.isArray(boot.netplayRooms) ? boot.netplayRooms : state.netplayRooms;
  state.udpLocalPort = boot.udpLocalPort || state.udpLocalPort;
  state.stunCandidate = boot.stunCandidate || state.stunCandidate;
  state.netplayChat = Array.isArray(boot.netplayChat) ? boot.netplayChat : state.netplayChat;
  state.communityMods = Array.isArray(boot.communityMods)
    ? boot.communityMods
    : state.communityMods;
  state.ghostLeaderboard = Array.isArray(boot.ghostLeaderboard)
    ? boot.ghostLeaderboard
    : state.ghostLeaderboard;
  const eff = boot.effectiveConfig;
  state.cpuOverclockPct = eff?.cpuOverclockPct ?? boot.cpuOverclockPct ?? state.cpuOverclockPct;
  state.pgxpSubpixel =
    eff?.pgxpSubpixel !== undefined
      ? eff.pgxpSubpixel
      : boot.pgxpSubpixel !== undefined
        ? boot.pgxpSubpixel
        : state.pgxpSubpixel;
  state.sramHexInspection = boot.sramHexInspection || state.sramHexInspection;
  state.tournamentBracket = boot.tournamentBracket || state.tournamentBracket;
  state.nativeCores = Array.isArray(boot.nativeCores) ? boot.nativeCores : state.nativeCores;
  state.biosEntries = Array.isArray(boot.biosEntries) ? boot.biosEntries : state.biosEntries;
  state.linkTelemetry = boot.linkTelemetry || { enabled: false, packetsTransferred: 0 };
  state.romDirectories = Array.isArray(boot.romDirectories)
    ? boot.romDirectories
    : state.romDirectories;
  state.switchVault = boot.switchVault || state.switchVault;
  state.lastScanReport = boot.lastScanReport || state.lastScanReport;
  state.warning = boot.warning || null;
  if (state.warning && state.warning !== lastSurfacedWarning) {
    lastSurfacedWarning = state.warning;
    showToast(state.warning, "error", 7000);
  }
  updateAllViews();
}

function computeCrc32Hex(bytes) {
  let crc = 0xffffffff;
  for (let i = 0; i < bytes.length; i++) {
    crc ^= bytes[i];
    for (let b = 0; b < 8; b++) {
      crc = (crc >>> 1) ^ (crc & 1 ? 0xedb88320 : 0);
    }
  }
  return ((crc ^ 0xffffffff) >>> 0).toString(16).toUpperCase().padStart(8, "0");
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

function renderStandbyCanvas() {
  const canvas = document.getElementById("canvas-p1");
  if (canvas) {
    const ctx = canvas.getContext("2d");
    const w = canvas.width || 160;
    const h = canvas.height || 144;
    ctx.fillStyle = "#110F0E";
    ctx.fillRect(0, 0, w, h);
    ctx.strokeStyle = "#282422";
    ctx.lineWidth = 1;
    ctx.strokeRect(8.5, 8.5, w - 17, h - 17);
  }
  const osc = document.getElementById("audio-oscilloscope-canvas");
  if (osc) {
    const ctx = osc.getContext("2d");
    ctx.fillStyle = "#110F0E";
    ctx.fillRect(0, 0, osc.width, osc.height);
    ctx.strokeStyle = "#332E2B";
    ctx.beginPath();
    ctx.moveTo(0, osc.height / 2);
    ctx.lineTo(osc.width, osc.height / 2);
    ctx.stroke();
  }
}

function paintRgbaFrame(canvasId, frame) {
  const canvas = document.getElementById(canvasId);
  if (!canvas || !frame || !frame.rgba) return;
  const processed =
    canvasId.startsWith("canvas-p") && state.upscalerMode !== "OFF"
      ? upscaleFrameIfEnabled(frame, state.upscalerMode)
      : frame;

  const w = processed.width;
  const h = processed.height;
  if (!w || !h || processed.rgba.length !== w * h * 4) return;

  if (canvas.width !== w || canvas.height !== h) {
    canvas.width = w;
    canvas.height = h;
  }
  const ctx = canvas.getContext("2d");
  let cached = canvasImageCache.get(canvasId);
  if (!cached || cached.width !== w || cached.height !== h) {
    cached = {
      width: w,
      height: h,
      imgData: ctx.createImageData(w, h)
    };
    canvasImageCache.set(canvasId, cached);
  }
  cached.imgData.data.set(processed.rgba);
  ctx.putImageData(cached.imgData, 0, 0);
}

function renderAudioOscilloscope(freqHz, frameNumber, pcmSamples = null, consoleKind = "") {
  const osc = document.getElementById("audio-oscilloscope-canvas");
  if (!osc) return;
  const ctx = osc.getContext("2d");
  const w = osc.width;
  const h = osc.height;
  ctx.fillStyle = "#110F0E";
  ctx.fillRect(0, 0, w, h);

  ctx.strokeStyle = "#332E2B";
  ctx.beginPath();
  ctx.moveTo(0, h / 2);
  ctx.lineTo(w, h / 2);
  ctx.stroke();

  let plotSamples;
  const hasRealPcm = pcmSamples && pcmSamples.length >= 2;
  if (hasRealPcm) {
    const pairCount = pcmSamples.length >> 1;
    const step = Math.max(1, Math.floor(pairCount / 48));
    plotSamples = [];
    for (let i = 0; i < pairCount && plotSamples.length < 48; i += step) {
      plotSamples.push((pcmSamples[i * 2] + pcmSamples[i * 2 + 1]) * 0.5);
    }
  } else {
    plotSamples = synthesizeMultiChannelSamples(freqHz || 0, frameNumber, 48);
  }

  ctx.strokeStyle = freqHz > 0 || hasRealPcm ? "#7D9B76" : "#3B6EF5";
  ctx.lineWidth = 1.5;
  ctx.beginPath();
  for (let i = 0; i < plotSamples.length; i++) {
    const x = (i / Math.max(1, plotSamples.length - 1)) * w;
    const clamped = Math.max(-1, Math.min(1, plotSamples[i] * 1.4));
    const y = h / 2 - clamped * (h * 0.42);
    if (i === 0) ctx.moveTo(x, y);
    else ctx.lineTo(x, y);
  }
  ctx.stroke();
}

function detectConsoleFromExtension(lowerName) {
  if (lowerName.endsWith(".nx") || lowerName.endsWith(".nup")) {
    return { console: "SWITCH2", coreId: "fritchii-external" };
  }
  if (
    lowerName.endsWith(".nsp") ||
    lowerName.endsWith(".xci") ||
    lowerName.endsWith(".nsz") ||
    lowerName.endsWith(".xcz") ||
    lowerName.endsWith(".nro") ||
    lowerName.endsWith(".nso")
  ) {
    return { console: "SWITCH", coreId: "torzu-external" };
  }
  if (lowerName.endsWith(".gba")) return { console: "GBA", coreId: "zaimule-rust-gba" };
  if (lowerName.endsWith(".nds") || lowerName.endsWith(".srl")) {
    return { console: "NDS", coreId: "zaimule-rust-nds" };
  }
  if (lowerName.endsWith(".gb") || lowerName.endsWith(".dmg")) {
    return { console: "GB", coreId: "zaimule-rust-gb" };
  }
  if (lowerName.endsWith(".gbc")) return { console: "GBC", coreId: "zaimule-rust-gb" };
  return null;
}

const MAX_WEB_ROM_READ_BYTES = 128 * 1024 * 1024;

async function handleImportedFile(file, options = {}) {
  if (!file) return;
  const lower = file.name.toLowerCase();

  if (lower.endsWith(".keys")) {
    showToast(
      "Importez prod.keys / title.keys depuis l'onglet « Cœurs & Switch » (ou glissez le fichier dans l'application desktop).",
      "info",
      6000
    );
    switchTab("view-cores");
    return;
  }

  if (file.size > MAX_WEB_ROM_READ_BYTES) {
    if (tauriInvoke) {
      showToast(
        `${file.name} (${formatFileSize(file.size)}) dépasse la limite mémoire Web. Utilisez « Importer ROM » ou glissez-déposez directement le fichier dans la fenêtre.`,
        "error",
        7000
      );
    } else {
      const detected = detectConsoleFromExtension(lower);
      if (!detected) {
        showToast(
          `Format non pris en charge : ${file.name}. Formats : .gb, .gbc, .gba, .nds, .nsp, .xci, .nx, .nup, .zip, .7z.`,
          "error"
        );
        return;
      }
      const isSwitch = detected.console === "SWITCH" || detected.console === "SWITCH2";
      const newId = `rom-${Date.now()}-${Math.floor(Math.random() * 1000)}`;
      state.catalog.unshift({
        id: newId,
        title: file.name.replace(/\.[^.]+$/, ""),
        console: detected.console,
        engineKind: isSwitch ? "externalLauncher" : "nativeRust",
        switchEngine: "auto",
        coreId: detected.coreId,
        region: "Import Local",
        crc32Hex: "00000000",
        sizeBytes: file.size,
        fastFingerprint: true,
        sramSummary: isSwitch
          ? detected.console === "SWITCH2"
            ? "Sauvegarde Fritchii (Switch 2)"
            : "Sauvegarde Torzu (Switch 1)"
          : "32 Ko SRAM Atomique (.tmp → .srm)",
        playtimeSeconds: 0,
        favorite: true,
        linkCableReady: !isSwitch && detected.console !== "NDS"
      });
      state.activeGameId = newId;
      updateAllViews();
      showToast(`${file.name} référencé dans l'aperçu Web.`, "info");
    }
    return;
  }

  const buf = await file.arrayBuffer();
  const bytes = Array.from(new Uint8Array(buf));

  if (lower.endsWith(".zaimule-pack") || lower.endsWith(".json")) {
    const boot = await invokeBackend("import_portable_pack_archive", { bytes });
    if (boot) {
      applyBootstrap(boot);
    }
    showToast(`Archive ${file.name} synchronisée`, "ok");
    return;
  }

  if (lower.endsWith(".ips")) {
    const boot = await invokeBackend("apply_ips_patch_file", {
      patchName: file.name,
      patchBytes: bytes
    });
    if (boot) {
      applyBootstrap(boot);
    } else if (!tauriInvoke) {
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
  } else if (!tauriInvoke) {
    const detected = detectConsoleFromExtension(lower);
    if (!detected) {
      showToast(
        `Format non pris en charge : ${file.name}. Formats : .gb, .gbc, .gba, .nds, .nsp, .xci, .nx, .nup, .zip, .7z.`,
        "error"
      );
      return;
    }
    const newId = `rom-${Date.now()}-${Math.floor(Math.random() * 1000)}`;
    const isSwitch = detected.console === "SWITCH" || detected.console === "SWITCH2";
    const crc32Hex = computeCrc32Hex(bytes);
    state.catalog.unshift({
      id: newId,
      title: file.name.replace(/\.[^.]+$/, ""),
      console: detected.console,
      engineKind: isSwitch ? "externalLauncher" : "nativeRust",
      switchEngine: "auto",
      coreId: detected.coreId,
      region: "Import Local",
      crc32Hex,
      sizeBytes: bytes.length,
      sramSummary: isSwitch
        ? detected.console === "SWITCH2"
          ? "Sauvegarde Fritchii (Switch 2)"
          : "Sauvegarde Torzu (Switch 1)"
        : "32 Ko SRAM Atomique (.tmp → .srm)",
      playtimeSeconds: 0,
      favorite: true,
      linkCableReady: !isSwitch && detected.console !== "NDS"
    });
    state.activeGameId = newId;
    updateAllViews();
  } else {
    return;
  }
  if (!options.skipAutoLaunch) {
    await launchSelectedGame();
  }
}

function renderDebuggerPayload(dbg) {
  const listEl = document.getElementById("disasm-list");
  if (!dbg) {
    if (listEl) {
      listEl.innerHTML = `<div class="empty-list-hint">En attente d'exécution CPU</div>`;
    }
    return;
  }
  if (dbg.vram) {
    paintRgbaFrame("vram-atlas-canvas", {
      width: dbg.vram.atlasWidth || 128,
      height: dbg.vram.atlasHeight || 64,
      rgba: dbg.vram.tileAtlasRgba
    });
  }
  if (listEl && Array.isArray(dbg.disassembly)) {
    listEl.innerHTML = "";
    if (dbg.disassembly.length === 0) {
      listEl.innerHTML = `<div class="empty-list-hint">Aucune instruction désassemblée</div>`;
      return;
    }
    for (const line of dbg.disassembly) {
      const row = document.createElement("div");
      row.className = `disasm-row${line.isPc ? " is-pc" : ""}${line.hasBreakpoint ? " has-bp" : ""}`;
      row.innerHTML = `
        <span>${line.hasBreakpoint ? "● " : line.isPc ? "▶ " : "  "}${escapeHtml(line.addrHex)}</span>
        <span style="color:var(--text-primary)">${escapeHtml(line.mnemonic)}</span>
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
  if (!state.isEmulationRunning || !getSelectedGame()) {
    renderDebuggerPayload(null);
    return;
  }
  const dbg = await invokeBackend("get_vram_and_disassembly", {}, { silent: true });
  if (dbg) {
    renderDebuggerPayload(dbg);
  }
}

async function tickEmulationLoop() {
  if (stepInFlight) {
    requestAnimationFrame(tickEmulationLoop);
    return;
  }
  stepInFlight = true;
  try {
    const buttonsP1 = computeBitmaskFromPressedCodes(pressedKeys) | pollGamepadBitmask();
    await handleBigPictureNavigation(buttonsP1);

    const activeGame = getSelectedGame();
    const isSwitch = isSwitchGame(activeGame);

    if (!state.isEmulationRunning || !activeGame || isSwitch) {
      if (isSwitch && state.isEmulationRunning) {
        state.isEmulationRunning = false;
        hideEmuBanner();
      }
      return;
    }

    if (state.debuggerPaused && !state.stepOnceRequested) {
      return;
    }
    state.stepOnceRequested = false;

    if (state.recordingMacro) {
      state.recordedFrames.push(buttonsP1);
    }

    const rawPkt = await invokeBackend(
      "step_viewport_binary",
      {
        buttonsP1,
        buttonsP2: 0,
        turboActive: true,
        speedMult: state.speedMult
      },
      { silent: true }
    );

    if (rawPkt) {
      const pkt = decodeViewportBinaryPacket(rawPkt);
      const primary = pkt.views[0];
      if (primary) {
        state.currentFrame = pkt.frameNumber || state.currentFrame + 1;
        paintRgbaFrame("canvas-p1", primary);
        if (pkt.views[1]) {
          paintRgbaFrame("canvas-p2", pkt.views[1]);
        }
        if (pkt.views[2]) {
          paintRgbaFrame("canvas-p3", pkt.views[2]);
        } else if (state.quadLinkMode) {
          paintRgbaFrame("canvas-p3", primary);
        }
        if (pkt.views[3]) {
          paintRgbaFrame("canvas-p4", pkt.views[3]);
        } else if (state.quadLinkMode) {
          paintRgbaFrame("canvas-p4", primary);
        }

        // Draw waveform BEFORE transferring pkt.pcmSamples buffer ownership to AudioWorklet
        renderAudioOscilloscope(
          pkt.audioFreqHz || 0,
          pkt.frameNumber,
          pkt.pcmSamples,
          pkt.meta?.console || activeGame.console
        );
        apuAudio.pushPcmSamples(pkt.pcmSamples, pkt.sampleRateHz);

        if (pkt.audioFreqHz >= 1200) {
          triggerGamepadRumble(getConnectedGamepad(), 55, 0.35, 0.65);
        }

        const meta = pkt.meta || {};
        if (meta.newlyUnlockedAchievement) {
          triggerGamepadRumble(getConnectedGamepad(), 140, 0.6, 0.95);
          showToast(`Succès débloqué : ${meta.newlyUnlockedAchievement}`, "ok");
          const boot = await invokeBackend("get_station_bootstrap", {}, { silent: true });
          if (boot) applyBootstrap(boot);
          else renderInspector();
        }

        const cpu = meta.cpu || {};
        const score = cpu.score ?? 0;
        document.getElementById("emu-score-pill").textContent = `Score : ${score}`;
        document.getElementById("p1-reg-readout").textContent = `${cpu.arch || "—"} · PC:${cpu.pcHex || "—"}`;
        document.getElementById("rewind-readout").textContent = `Frame #${pkt.frameNumber}`;
        document.getElementById("dbg-pc-sp").textContent = `${cpu.pcHex || "—"} / ${cpu.spHex || "—"}`;
        document.getElementById("dbg-af-bc").textContent = `${cpu.regAHex || "—"} / ${cpu.regBcHex || "—"}`;
        document.getElementById("dbg-de-hl").textContent = `${cpu.regDeHex || "—"} / ${cpu.regHlHex || "—"}`;
        document.getElementById("dbg-sio").textContent = `${cpu.serialSbHex || "0x00"} / ${pkt.audioFreqHz || 0} Hz`;
        document.getElementById("tel-rewind").textContent = `${pkt.rewindSnapshotsCount} clichés (${Math.round(pkt.rewindMemoryBytes / 1024)} Ko)`;

        const link = meta.linkTelemetry;
        if (link?.enabled) {
          const modePrefix = link.quadMode ? "4P Tournoi" : "2P Sync";
          document.getElementById("tel-link").textContent =
            `${modePrefix} (${link.packetsTransferred} pkts · ${link.lastP1ToP2Hex})`;
        }
        if ((pkt.frameNumber & 15) === 1 || state.debuggerPaused) {
          await refreshDebuggerDrawer();
        }
      }
    }
  } catch (err) {
    console.warn("[zaimule] Erreur boucle viewport:", err);
  } finally {
    stepInFlight = false;
    requestAnimationFrame(tickEmulationLoop);
  }
}

async function setupDragAndDrop() {
  const appShell = document.querySelector(".app-shell");
  const setDragActive = (active) => {
    appShell?.classList.toggle("drag-active", Boolean(active));
  };

  const handleDroppedPaths = async (rawPaths) => {
    const paths = Array.isArray(rawPaths) ? rawPaths.filter(Boolean) : [];
    setDragActive(false);
    if (paths.length === 0) return;
    showToast(`Importation de ${paths.length} élément(s)...`, "info", 4000);
    const boot = await invokeBackend("import_rom_paths", { paths });
    if (boot) {
      applyBootstrap(boot);
      const added = boot.lastScanReport?.importedEntries?.length || 0;
      if (added > 0) {
        showToast(`${added} jeu(x) importé(s) dans la bibliothèque.`, "ok");
      } else {
        showToast("Élément(s) traité(s).", "ok");
      }
    }
  };

  window.addEventListener("dragover", (e) => {
    e.preventDefault();
  });

  const currentWebview =
    window.__TAURI__?.webview?.getCurrentWebview?.() ||
    window.__TAURI__?.webviewWindow?.getCurrentWebviewWindow?.() ||
    window.__TAURI__?.window?.getCurrentWindow?.();

  if (currentWebview?.onDragDropEvent) {
    window.addEventListener("drop", (e) => {
      e.preventDefault();
      setDragActive(false);
    });
    try {
      await currentWebview.onDragDropEvent(async (event) => {
        const type = event?.payload?.type;
        if (type === "enter" || type === "over") {
          setDragActive(true);
        } else if (type === "leave" || type === "cancel") {
          setDragActive(false);
        } else if (type === "drop") {
          await handleDroppedPaths(event.payload.paths);
        }
      });
      return;
    } catch (err) {
      console.warn("[zaimule] onDragDropEvent indisponible :", err);
    }
  }

  if (window.__TAURI__?.event?.listen) {
    window.addEventListener("drop", (e) => {
      e.preventDefault();
      setDragActive(false);
    });
    await window.__TAURI__.event.listen("tauri://drag-enter", () => setDragActive(true));
    await window.__TAURI__.event.listen("tauri://drag-leave", () => setDragActive(false));
    await window.__TAURI__.event.listen("tauri://drag-drop", async (event) => {
      const paths = Array.isArray(event?.payload) ? event.payload : event?.payload?.paths;
      await handleDroppedPaths(paths);
    });
    return;
  }

  let dragDepth = 0;
  window.addEventListener("dragenter", (e) => {
    e.preventDefault();
    dragDepth += 1;
    setDragActive(true);
  });
  window.addEventListener("dragleave", (e) => {
    e.preventDefault();
    dragDepth = Math.max(0, dragDepth - 1);
    if (dragDepth === 0) setDragActive(false);
  });
  window.addEventListener("drop", async (e) => {
    e.preventDefault();
    dragDepth = 0;
    setDragActive(false);
    const files = Array.from(e.dataTransfer?.files || []);
    for (let i = 0; i < files.length; i++) {
      await handleImportedFile(files[i], {
        skipAutoLaunch: files.length > 1 && i < files.length - 1
      });
    }
  });
}

function bindEvents() {
  document.querySelectorAll(".nav-tab").forEach((btn) => {
    btn.addEventListener("click", () => switchTab(btn.dataset.target));
  });

  document.querySelectorAll(".inspector-tab-btn").forEach((btn) => {
    btn.addEventListener("click", () => {
      const targetPane = btn.dataset.inspTab;
      document.querySelectorAll(".inspector-tab-btn").forEach((b) => {
        b.classList.toggle("active", b === btn);
      });
      document.querySelectorAll(".inspector-tab-pane").forEach((p) => {
        p.classList.toggle("active", p.id === targetPane);
      });
    });
  });

  const toggleInspectorDrawer = () => {
    const drawer = document.getElementById("inspector-drawer");
    drawer?.classList.toggle("collapsed");
  };
  document.getElementById("btn-toggle-inspector")?.addEventListener("click", toggleInspectorDrawer);
  document.getElementById("prompt-options")?.addEventListener("click", toggleInspectorDrawer);

  document.getElementById("prompt-launch")?.addEventListener("click", () => {
    launchSelectedGame();
  });

  document.getElementById("prompt-savestate")?.addEventListener("click", () => {
    document.getElementById("btn-quick-save")?.click();
  });

  const cycleConsoleFilter = (dir = 1) => {
    const btns = Array.from(document.querySelectorAll(".console-btn"));
    if (btns.length === 0) return;
    const curIdx = Math.max(0, btns.findIndex((b) => b.classList.contains("active")));
    const nextBtn = btns[(curIdx + dir + btns.length) % btns.length];
    nextBtn?.click();
  };
  document.getElementById("prompt-console-cycle")?.addEventListener("click", () => cycleConsoleFilter(1));

  document.getElementById("btn-big-picture")?.addEventListener("click", (e) => {
    state.bigPictureMode = !state.bigPictureMode;
    document.querySelector(".app-shell")?.classList.toggle("big-picture", state.bigPictureMode);
    e.currentTarget.textContent = `Big Picture : ${state.bigPictureMode ? "ACTIF" : "OFF"}`;
    triggerGamepadRumble(getConnectedGamepad(), 70, 0.4, 0.7);
  });

  const colorLabels = ["#0 Fond", "#1 Basalte", "#2 Argile", "#3 Pigment"];
  document.getElementById("btn-vram-color")?.addEventListener("click", (e) => {
    state.vramPaintColorIdx = (state.vramPaintColorIdx + 3) % 4;
    e.currentTarget.textContent = colorLabels[state.vramPaintColorIdx];
  });

  document.getElementById("vram-atlas-canvas")?.addEventListener("click", async (e) => {
    if (!getSelectedGame()) return;
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
      toast.textContent = `Pixel VRAM (${atlasX}, ${atlasY}) peint en couleur #${state.vramPaintColorIdx}`;
    }
  });

  const canvasP1 = document.getElementById("canvas-p1");
  const handleStylusEvent = async (ev, pressed) => {
    if (!canvasP1 || !getSelectedGame()) return;
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

  document.getElementById("insp-switch-engine")?.addEventListener("change", async (e) => {
    const g = getSelectedGame();
    if (!g || !isSwitchGame(g)) return;
    const mode = e.target.value;
    const boot = await invokeBackend("set_game_switch_engine", {
      gameId: g.id,
      mode
    });
    if (boot) {
      applyBootstrap(boot);
      showToast(
        `Moteur Switch pour ${g.title} : ${formatSwitchEngineLabel(mode, g.console)}`,
        "ok"
      );
    } else if (!tauriInvoke) {
      g.switchEngine = mode;
      updateAllViews();
    }
  });

  document.getElementById("btn-send-chat")?.addEventListener("click", async () => {
    const inp = document.getElementById("netplay-chat-input");
    const text = inp?.value?.trim();
    if (!text) return;
    const boot = await invokeBackend("post_netplay_chat_message", {
      sender: "Hôte",
      text,
      shareSlot: null
    });
    if (boot) {
      applyBootstrap(boot);
    } else {
      state.netplayChat.unshift({
        id: `chat-${Date.now()}`,
        sender: "Hôte",
        text,
        saveStateCrcHex: null
      });
      renderNetplayAndCores();
    }
    inp.value = "";
  });

  document.getElementById("btn-share-state-chat")?.addEventListener("click", async () => {
    const g = getSelectedGame();
    if (!g) return;
    const text = `Cliché Save State Slot #1 (${g.title}) partagé`;
    const boot = await invokeBackend("post_netplay_chat_message", {
      sender: "Hôte",
      text,
      shareSlot: 1
    });
    if (boot) {
      applyBootstrap(boot);
    } else {
      state.netplayChat.unshift({
        id: `chat-state-${Date.now()}`,
        sender: "Hôte",
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

  document.getElementById("btn-import-rom")?.addEventListener("click", () => handleImportRomsTrigger());
  document.getElementById("btn-scan-folder")?.addEventListener("click", () => handleScanFolderTrigger());
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
    if (!getSelectedGame()) return;
    const updated = await invokeBackend("write_sram_byte_live", {
      offset: 4,
      value: 0xff
    });
    if (updated?.sramHexInspection) {
      applyBootstrap(updated);
    } else if (updated) {
      state.sramHexInspection = updated;
      renderInspector();
    }
    const toast = document.getElementById("save-toast");
    if (toast) toast.textContent = "Octet SRAM 0x0004 = 0xFF écrit & CRC32 recalculé";
  });

  document.getElementById("btn-sram-transfer")?.addEventListener("click", async () => {
    if (!getSelectedGame()) return;
    const boot = await invokeBackend("transfer_sram_p1_to_p2");
    if (boot) applyBootstrap(boot);
    const toast = document.getElementById("save-toast");
    if (toast) toast.textContent = "Batterie SRAM / Carte Mémoire synchronisée J1 → J2";
  });

  document.getElementById("btn-toggle-spectator")?.addEventListener("click", async () => {
    const boot = await invokeBackend("toggle_tournament_spectator");
    if (boot) {
      applyBootstrap(boot);
    }
  });

  document.getElementById("btn-submit-ghost")?.addEventListener("click", async () => {
    const g = getSelectedGame();
    if (!g) return;
    const boot = await invokeBackend("submit_ghost_leaderboard_run", {
      playerName: "Joueur 1"
    });
    if (boot) {
      applyBootstrap(boot);
    }
  });

  document.getElementById("btn-export-pack")?.addEventListener("click", async () => {
    const packBytes =
      (await invokeBackend("export_portable_pack_archive")) ||
      Array.from(
        new TextEncoder().encode(
          JSON.stringify({
            magic: "ZAIMULE_PACK_V1",
            version: "1.3.0",
            activeGameId: state.activeGameId,
            cheatCodes: state.cheatCodes,
            macros: state.macros
          })
        )
      );
    const prefix = state.activeGameId || "zaimule";
    downloadBinaryFile(packBytes, `${prefix}-cloud.zaimule-pack`, "application/json");
    const toast = document.getElementById("save-toast");
    if (toast) toast.textContent = `Pack .zaimule-pack exporté (${packBytes.length} o)`;
  });

  document.getElementById("btn-export-ips")?.addEventListener("click", async () => {
    if (!getSelectedGame()) return;
    const ipsBytes =
      (await invokeBackend("export_ips_diff")) || [
        0x50, 0x41, 0x54, 0x43, 0x48, 0x00, 0x01, 0x34, 0x00, 0x04, 0x5a, 0x4d, 0x44, 0x58, 0x45,
        0x4f, 0x46
      ];
    downloadBinaryFile(ipsBytes, `${state.activeGameId}-dx.ips`, "application/octet-stream");
    state.modLayers.unshift({
      id: `ips-diff-${Date.now()}`,
      label: `Diff ${state.activeGameId}-dx.ips (${ipsBytes.length} o)`,
      enabled: true
    });
    renderInspector();
  });

  romInput?.addEventListener("change", async (e) => {
    const files = Array.from(e.target.files || []);
    for (let i = 0; i < files.length; i++) {
      await handleImportedFile(files[i], {
        skipAutoLaunch: files.length > 1 && i < files.length - 1
      });
    }
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
    const rawAddr = inp?.value?.trim();
    if (!rawAddr) return;
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
        currentValue: 0x00,
        previousValue: 0x00,
        frozen: true,
        frozenValue: 0x00
      });
      renderKeymapperAndMacros();
    }
  });

  document.getElementById("btn-scan-ram")?.addEventListener("click", async () => {
    const inp = document.getElementById("ram-scan-input");
    const targetValue = Number(inp?.value || 0) & 0xff;
    const hits = (await invokeBackend("scan_ram_byte", { targetValue })) || [];
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
    if (!g) return;
    const boot = await invokeBackend("create_netplay_room", {
      title: `Salon Live · ${g.title}`,
      hostName: "Hôte",
      peerAddr: null
    });
    if (boot) {
      applyBootstrap(boot);
    } else {
      state.netplayRooms.unshift({
        roomCode: `ZM-${g.crc32Hex.slice(0, 4)}`,
        title: `Salon Live · ${g.title}`,
        hostName: "Hôte",
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
    launchSelectedGame();
  });

  document.getElementById("btn-launch-selected")?.addEventListener("click", () => {
    launchSelectedGame();
  });
  document.getElementById("btn-insp-play")?.addEventListener("click", () => {
    launchSelectedGame();
  });

  document.getElementById("btn-audio-toggle")?.addEventListener("click", (e) => {
    const on = apuAudio.toggleEnabled();
    if (on && state.isEmulationRunning) {
      apuAudio.ensureContext();
    }
    e.currentTarget.textContent = `Audio APU : ${on ? "ON" : "MUET"}`;
  });

  document.getElementById("btn-record-video")?.addEventListener("click", (e) => {
    if (!getSelectedGame()) return;
    const canvas = document.getElementById("canvas-p1");
    if (!viewportRecorder.isRecording) {
      viewportRecorder.start(canvas, state.activeGameId);
      e.currentTarget.textContent = "Stop .WebM";
      document.getElementById("save-toast").textContent = "Capture vidéo 60 FPS en cours...";
    } else {
      const res = viewportRecorder.stop();
      e.currentTarget.textContent = "Clip .WebM";
      document.getElementById("save-toast").textContent =
        `Clip enregistré : ${res.fileName} (${res.durationText})`;
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
        ? "Upscaler : Scale2x EPX"
        : state.upscalerMode === "XBRZ"
          ? "Upscaler : xBRZ 2x"
          : state.upscalerMode === "BLOOM"
            ? "Upscaler : CRT Bloom"
            : "Upscaler : Natif 1x";
    e.currentTarget.textContent = label;
  });

  const shaders = ["mode-crt", "mode-trinitron", "mode-dmg", "mode-lcd", "mode-clean"];
  const shaderLabels = [
    "Shader : CRT Scanlines",
    "Shader : Trinitron Courbé",
    "Shader : Game Boy DMG Vert",
    "Shader : Matrice LCD",
    "Shader : Pixel-Perfect"
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
      btn.textContent = `Keymapper : ${state.keymapperVisible ? "ON" : "OFF"}`;
    }
  };
  document.getElementById("btn-toggle-keymapper")?.addEventListener("click", toggleKm);
  document.getElementById("btn-dock-keymapper")?.addEventListener("click", toggleKm);

  document.getElementById("btn-record-macro")?.addEventListener("click", async (e) => {
    if (!state.recordingMacro) {
      state.recordingMacro = true;
      state.recordedFrames = [];
      e.currentTarget.textContent = "Arrêter & Sauver Macro";
      document.getElementById("macro-status").textContent = "ENREGISTREMENT...";
    } else {
      state.recordingMacro = false;
      e.currentTarget.textContent = "Enregistrer Séquence Live";
      const frames = state.recordedFrames.length > 0 ? state.recordedFrames : [BTN_A];
      const name = `Combo Live (${frames.length}f)`;
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
      if (quadBtn) quadBtn.textContent = "Tournoi 4P : OFF";
    }
    document.getElementById("bezel-p2").style.display = state.dualLinkMode ? "flex" : "none";
    document.getElementById("screens-wrap").classList.toggle("dual-mode", state.dualLinkMode && !state.quadLinkMode);
    e.currentTarget.textContent = `Link 2P : ${state.dualLinkMode ? "ACTIF" : "OFF"}`;
    document.getElementById("link-status-pill").textContent = state.dualLinkMode
      ? "Câble Link 2P Synchronisé"
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
    e.currentTarget.textContent = `Tournoi 4P : ${state.quadLinkMode ? "ACTIF" : "OFF"}`;
    document.getElementById("link-status-pill").textContent = state.quadLinkMode
      ? "Tournoi 4P Multi-SIO"
      : state.dualLinkMode
        ? "Câble Link 2P Synchronisé"
        : "Mode 1P Solo";
    await invokeBackend("set_quad_link_enabled", { enabled: state.quadLinkMode });
  });

  document.getElementById("btn-dbg-pause")?.addEventListener("click", (e) => {
    state.debuggerPaused = !state.debuggerPaused;
    e.currentTarget.textContent = state.debuggerPaused ? "Reprendre" : "Pause CPU";
  });

  document.getElementById("btn-dbg-step")?.addEventListener("click", () => {
    state.debuggerPaused = true;
    state.stepOnceRequested = true;
    const pauseBtn = document.getElementById("btn-dbg-pause");
    if (pauseBtn) pauseBtn.textContent = "Reprendre";
  });

  document.getElementById("btn-speed-cycle")?.addEventListener("click", (e) => {
    state.speedMult =
      state.speedMult === 1 ? 2 : state.speedMult === 2 ? 4 : state.speedMult === 4 ? 8 : 1;
    e.currentTarget.textContent = `Vitesse : ${state.speedMult}x`;
    if (state.isEmulationRunning) {
      document.getElementById("tel-fps").textContent = `${(60 * state.speedMult).toFixed(0)} FPS`;
      document.getElementById("tel-ft").textContent = `${(16.6 / state.speedMult).toFixed(1)} ms`;
    }
  });

  document.getElementById("btn-quick-save")?.addEventListener("click", async () => {
    if (!getSelectedGame()) return;
    const info = await invokeBackend("create_save_state", { slot: 1, label: "QuickSave F5" });
    document.getElementById("save-toast").textContent = info
      ? `Sauvé Slot #1 (Frame #${info.frame} · CRC ${info.crc32Hex})`
      : `Sauvé Slot #1 (Frame #${state.currentFrame})`;
  });

  document.getElementById("btn-quick-load")?.addEventListener("click", async () => {
    if (!getSelectedGame()) return;
    const info = await invokeBackend("restore_save_state", { slot: 1 });
    document.getElementById("save-toast").textContent = info
      ? `Chargé Slot #1 (Frame #${info.frame})`
      : `Chargé Slot #1`;
  });

  document.getElementById("btn-rewind-step")?.addEventListener("click", async () => {
    if (!getSelectedGame()) return;
    await invokeBackend("rewind_frames", { stepsBack: 2 });
  });

  window.addEventListener("keydown", async (e) => {
    const activeTag = document.activeElement?.tagName;
    if (activeTag === "INPUT" || activeTag === "TEXTAREA" || activeTag === "SELECT") {
      if (e.key === "Escape") document.activeElement.blur();
      return;
    }
    if (e.ctrlKey || e.metaKey || e.altKey) {
      return;
    }
    if (state.isEmulationRunning && apuAudio.enabled) {
      apuAudio.ensureContext();
    }
    pressedKeys.add(e.code);

    const activePane = document.querySelector(".view-pane.active")?.id || "view-library";
    if (e.key === "/") {
      e.preventDefault();
      document.getElementById("search-input")?.focus();
      return;
    }
    if (e.code === "KeyY") {
      e.preventDefault();
      toggleInspectorDrawer();
      return;
    }
    if (activePane === "view-library") {
      if (e.code === "Enter") {
        if (activeTag === "BUTTON") return;
        e.preventDefault();
        await launchSelectedGame();
        return;
      }
      if (e.code === "BracketLeft") {
        e.preventDefault();
        cycleConsoleFilter(-1);
        return;
      }
      if (e.code === "BracketRight") {
        e.preventDefault();
        cycleConsoleFilter(1);
        return;
      }
      if ((e.code === "ArrowRight" || e.code === "ArrowLeft") && state.catalog.length > 0) {
        e.preventDefault();
        const step = e.code === "ArrowRight" ? 1 : -1;
        const idx = state.catalog.findIndex((g) => g.id === state.activeGameId);
        const nextGame = state.catalog[(Math.max(0, idx) + step + state.catalog.length) % state.catalog.length];
        if (nextGame) {
          state.activeGameId = nextGame.id;
          const boot = await invokeBackend("select_game", { gameId: nextGame.id });
          if (boot) applyBootstrap(boot);
          else updateAllViews();
        }
        return;
      }
    }

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
  await setupDragAndDrop();

  if (window.__TAURI__?.event?.listen) {
    try {
      await window.__TAURI__.event.listen("switch-session-ended", async (event) => {
        const p = event?.payload;
        const boot = await invokeBackend("get_station_bootstrap", {}, { silent: true });
        if (boot) applyBootstrap(boot);
        if (p?.elapsedSeconds !== undefined) {
          const engineLabel = p.target === "fritchii" ? "Fritchii" : "Torzu";
          const durText =
            p.elapsedSeconds < 60 ? `${p.elapsedSeconds} s` : formatPlaytime(p.elapsedSeconds);
          showToast(
            `Session ${engineLabel} terminée · Temps enregistré : ${durText}.`,
            "ok",
            6000
          );
        }
      });
    } catch (err) {
      console.warn("[zaimule] Écouteur switch-session-ended indisponible :", err);
    }
  }

  renderStandbyCanvas();
  updateAllViews();
  const boot = await invokeBackend("get_station_bootstrap");
  if (boot) {
    applyBootstrap(boot);
  }
  await refreshDebuggerDrawer();
  requestAnimationFrame(tickEmulationLoop);
}

init();
