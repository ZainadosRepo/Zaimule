import {
  filterCatalogEntries,
  computeConsoleCounts,
  sortCatalogEntries,
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
  BTN_START,
  computeBitmaskFromPressedCodes,
  buildKeyBindingsFromWidgets,
  applyPressedTurboWidgets,
  mapCanvasToNdsStylus,
  computeBitmaskFromGamepad,
  clampWidgetPosition,
  triggerGamepadRumble,
  computeBigPictureNavAction
} from "./keymapper.js";
import { FramePacer, targetFpsForConsole } from "./pacing.js";
import { ApuAudioEngine, synthesizeMultiChannelSamples } from "./audio.js";
import { ViewportRecorder } from "./recorder.js";
import {
  NDS_LAYOUT_MODES,
  upscaleFrameIfEnabled,
  arrangeNdsDualScreenFrame
} from "./upscaler.js";
import { decodeViewportBinaryPacket } from "./viewport_ipc.js";
import { WebGlViewportRenderer } from "./gl_renderer.js";

const tauriInvoke = window.__TAURI__?.core?.invoke;
const appWindow = window.__TAURI__?.window?.getCurrentWindow?.();
const apuAudio = new ApuAudioEngine();
const viewportRecorder = new ViewportRecorder();
const framePacer = new FramePacer();
const canvasImageCache = new Map();
const glRendererCache = new Map();

const SHADER_MODES = ["mode-clean", "mode-lcd", "mode-crt", "mode-trinitron", "mode-dmg"];
const SHADER_LABELS = {
  "mode-clean": "Cristal HD",
  "mode-lcd": "Grille LCD Subpixel",
  "mode-crt": "CRT Scanlines Pro",
  "mode-trinitron": "Sony Trinitron PVM",
  "mode-dmg": "Game Boy DMG Olive"
};
const UPSCALER_LABELS = {
  XBRZ: "xBRZ HD GPU",
  SHARP: "Sharp-Bilinear",
  SCALE2X: "Scale2x EPX",
  BLOOM: "CRT Bloom",
  OFF: "Natif 1x"
};
const NDS_LAYOUT_LABELS = {
  vertical: "DS : Vertical",
  hybrid: "DS : Hybride",
  horizontal: "DS : Côte à côte",
  "vertical-swapped": "DS : Inversé",
  "horizontal-swapped": "DS : Côte à côte inv."
};

let toastTimer = null;
let lastSurfacedWarning = "";
let stepInFlight = false;
let rafLoopScheduled = false;
let selectGameInFlightId = "";
let selectGameInFlightPromise = null;
let lastTelemetryDomUpdateMs = 0;
let activePaneId = "view-library";
let cachedKeyBindings = null;
let cachedKeyWidgetsRef = null;
let lastGamepadLabel = "";
let lastOscilloscopeDrawMs = 0;
const oscPlotBuffer = new Float32Array(48);
let oscCanvasVisible = false;
let vramCanvasVisible = false;
let lastDebuggerRefreshMs = 0;
let debuggerRefreshInFlight = false;
let pendingNdsStylus = null;
const domCache = Object.create(null);

function setText(id, value) {
  const el = document.getElementById(id);
  if (el) el.textContent = value;
}

function getCachedEl(id) {
  let el = domCache[id];
  if (el && el.isConnected) return el;
  el = document.getElementById(id);
  if (el) domCache[id] = el;
  return el;
}

function getCachedKeyBindings() {
  if (cachedKeyBindings && cachedKeyWidgetsRef === state.keymapperWidgets) {
    return cachedKeyBindings;
  }
  cachedKeyWidgetsRef = state.keymapperWidgets;
  cachedKeyBindings = buildKeyBindingsFromWidgets(state.keymapperWidgets);
  return cachedKeyBindings;
}

function invalidateKeyBindingsCache() {
  cachedKeyBindings = null;
  cachedKeyWidgetsRef = null;
}

function getGlRenderer(canvasId) {
  let renderer = glRendererCache.get(canvasId);
  if (renderer && renderer.canvas && renderer.canvas.isConnected) {
    return renderer.isSupported() ? renderer : null;
  }
  const canvas = document.getElementById(canvasId);
  if (!canvas) return null;
  renderer = new WebGlViewportRenderer(canvas);
  glRendererCache.set(canvasId, renderer);
  const screenContainer = canvas.closest(".screen-container");
  if (screenContainer) {
    screenContainer.classList.toggle("gl-active", renderer.isSupported());
  }
  return renderer.isSupported() ? renderer : null;
}

function redrawAllGlViewports() {
  const g = getSelectedGame();
  const isNds = Boolean(g && String(g.console || "").toUpperCase() === "NDS");
  const opts = {
    upscalerMode: state.upscalerMode,
    shaderCss: state.activeShaderCss,
    isNds,
    ndsLayout: state.ndsLayout,
    integerScale: Boolean(state.viewportIntegerScale)
  };
  for (const id of ["canvas-p1", "canvas-p2", "canvas-p3", "canvas-p4"]) {
    const renderer = glRendererCache.get(id);
    if (renderer && renderer.isSupported()) {
      renderer.redraw(opts);
    }
  }
}

function shouldKeepEmulationLoopAlive() {
  if (typeof document !== "undefined" && document.hidden) return false;
  if (state.bigPictureMode) return true;
  if (!state.isEmulationRunning) return false;
  if (activePaneId !== "view-emulator") return false;
  if (state.debuggerPaused && !state.stepOnceRequested) return false;
  return true;
}

function ensureEmulationLoopRunning() {
  if (rafLoopScheduled || !shouldKeepEmulationLoopAlive()) return;
  rafLoopScheduled = true;
  requestAnimationFrame(tickEmulationLoop);
}

function getCanvasEntry(canvasId, opaque = false) {
  let entry = canvasImageCache.get(canvasId);
  if (entry && entry.canvas && entry.canvas.isConnected) {
    return entry;
  }
  const canvas = document.getElementById(canvasId);
  if (!canvas) return null;
  const ctx =
    (opaque ? canvas.getContext("2d", { alpha: false }) : null) ||
    canvas.getContext("2d");
  if (!ctx) return null;
  entry = { canvas, ctx, width: 0, height: 0, imgData: null };
  canvasImageCache.set(canvasId, entry);
  return entry;
}

function switchSettingsSubTab(paneId) {
  const target = paneId || "settings-pane-general";
  document.querySelectorAll(".settings-nav-btn").forEach((btn) => {
    btn.classList.toggle("active", btn.dataset.settingsTab === target);
  });
  document.querySelectorAll(".settings-pane").forEach((pane) => {
    pane.classList.toggle("active", pane.id === target);
  });
}

function openSettingsTab(paneId = "settings-pane-general") {
  switchTab("view-settings");
  switchSettingsSubTab(paneId);
}

function setAudioVolumePct(rawPct, persist = true) {
  const pct = Math.max(0, Math.min(100, Math.round(Number(rawPct) || 0)));
  state.audioVolume = pct;
  apuAudio.setVolume(pct / 100);
  for (const sliderId of ["audio-volume-slider", "settings-volume-slider"]) {
    const s = document.getElementById(sliderId);
    if (s) s.value = String(pct);
  }
  setText("audio-volume-label", `${pct}%`);
  setText("settings-volume-label", `${pct}%`);
  if (persist) {
    try {
      window.localStorage?.setItem("zaimule.audioVolume", String(pct));
    } catch {
      // Ignore storage write errors
    }
  }
}

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
    },
    firmware: {
      firmwareDir: "",
      installed: false,
      ncaCount: 0,
      totalBytes: 0,
      versionHint: null
    }
  },
  saveSlots: Array.from({ length: 9 }, (_, i) => ({
    slot: i + 1,
    occupied: false,
    frame: 0,
    crc32Hex: "00000000",
    label: `Slot #${i + 1} Vide`
  })),
  keymapperWidgets: [
    {
      id: "km-dpad",
      label: "✚ D-PAD [ZQSD / Flèches]",
      kind: "dpad",
      keyBinding: "ZQSD",
      buttonMask: BTN_UP | BTN_DOWN | BTN_LEFT | BTN_RIGHT,
      xPct: 14,
      yPct: 78,
      turboHz: null
    },
    {
      id: "km-btn-a",
      label: "🅰 Bouton A [J / Espace]",
      kind: "actionButton",
      keyBinding: "KeyJ",
      buttonMask: BTN_A,
      xPct: 82,
      yPct: 74,
      turboHz: null
    },
    {
      id: "km-btn-b",
      label: "🅱 Bouton B [K / Shift]",
      kind: "actionButton",
      keyBinding: "KeyK",
      buttonMask: BTN_B,
      xPct: 68,
      yPct: 82,
      turboHz: null
    },
    {
      id: "km-turbo-a",
      label: "⚡ TURBO A 20Hz [U]",
      kind: "turboButton",
      keyBinding: "KeyU",
      buttonMask: BTN_A,
      xPct: 78,
      yPct: 16,
      turboHz: 20
    },
    {
      id: "km-touch-nds",
      label: "🖱 Tactile NDS / Start [Entrée]",
      kind: "touchTapZone",
      keyBinding: "Enter",
      buttonMask: BTN_START,
      xPct: 20,
      yPct: 16,
      turboHz: null
    }
  ],
  macros: [],
  tasTimeline: [],
  netplayRooms: [],
  udpLocalPort: 54920,
  udpPeerAddr: null,
  udpPacketsSent: 0,
  udpPacketsReceived: 0,
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
      console: "NDS",
      fileName: "bios7.bin",
      expectedCrc32Hex: "1280F0D5",
      status: "freeBiosBuiltIn",
      notes: "BIOS ARM7 Nintendo DS (16 Ko) — optionnel, clé KEY1 + appels BIOS officiels."
    },
    {
      console: "NDS",
      fileName: "bios9.bin",
      expectedCrc32Hex: "2AB23573",
      status: "freeBiosBuiltIn",
      notes: "BIOS ARM9 Nintendo DS (4 Ko) — optionnel, appels BIOS ARM9 natifs."
    },
    {
      console: "NDS",
      fileName: "firmware.bin",
      expectedCrc32Hex: "N/A",
      status: "freeBiosBuiltIn",
      notes: "Firmware SPI Nintendo DS (256 Ko) — optionnel, profil utilisateur et calibration tactile."
    },
    {
      console: "GBA",
      fileName: "gba_bios.bin",
      expectedCrc32Hex: "81977335",
      status: "freeBiosBuiltIn",
      notes: "BIOS GBA HLE Rust intégré (termboy-gba 0.5.0, SWI + DMA complets)."
    },
    {
      console: "GB",
      fileName: "dmg_boot.bin",
      expectedCrc32Hex: "59C8598E",
      status: "freeBiosBuiltIn",
      notes: "Séquence de démarrage DMG/CGB Rust intégrée (termboy-gb 0.5.0)."
    }
  ],
  linkTelemetry: { enabled: false, quadMode: false, packetsTransferred: 0 },
  consoleFilter: "ALL",
  sortBy: "recent",
  searchQuery: "",
  audioVolume: (() => {
    try {
      const raw = window.localStorage?.getItem("zaimule.audioVolume");
      if (raw !== null && raw !== "") {
        const saved = Number(raw);
        if (Number.isFinite(saved)) return Math.max(0, Math.min(100, Math.round(saved)));
      }
    } catch {
      // Ignore storage access errors in sandboxed webviews
    }
    return 85;
  })(),
  keymapperVisible: false,
  dualLinkMode: false,
  quadLinkMode: false,
  debuggerPaused: false,
  stepOnceRequested: false,
  bigPictureMode: false,
  isFullscreen: false,
  prevNavMask: 0,
  vramPaintColorIdx: 3,
  stylusPressed: false,
  activeShaderCss: "mode-clean",
  upscalerMode: "XBRZ",
  speedMult: 1,
  ndsLayout: "vertical",
  nds3dScale: 1,
  viewportIntegerScale: false,
  viewportStretchMode:
    (typeof localStorage !== "undefined" && localStorage.getItem("zaimule.viewportStretchMode")) ||
    "native",
  rewindEnabled: true,
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

async function handleImportSwitchFirmware(mode = "archive", buttonEl = null) {
  if (!tauriInvoke) {
    showToast("L'importation du firmware Switch requiert l'application desktop Zaimule.", "error");
    return;
  }
  const prevText = buttonEl ? buttonEl.textContent : "";
  if (buttonEl) {
    buttonEl.disabled = true;
    buttonEl.textContent = "Importation...";
  }
  showToast("Importation du firmware Switch (.nca) dans le coffre isolé...", "info", 8000);
  const boot = await invokeBackend("pick_and_import_switch_firmware", { mode });
  if (buttonEl) {
    buttonEl.disabled = false;
    buttonEl.textContent = prevText;
  }
  if (boot) {
    applyBootstrap(boot);
    const fw = boot.switchVault?.firmware;
    if (fw?.installed) {
      showToast(
        `Firmware Switch installé (${fw.ncaCount} .nca · ${formatFileSize(fw.totalBytes)}).`,
        "ok",
        6000
      );
    }
  }
}

function describeBiosStatus(entry) {
  const st = entry?.status;
  const expected = entry?.expectedCrc32Hex || "—";
  if (st === "verifiedOfficial") {
    return {
      pillClass: "pill-green",
      label: `OFFICIEL · CRC ${entry.actualCrc32Hex || expected}`
    };
  }
  if (st && typeof st === "object" && st.customLoaded) {
    const crc =
      st.customLoaded.actualCrc32 ||
      st.customLoaded.actual_crc32 ||
      entry.actualCrc32Hex ||
      expected;
    return {
      pillClass: "pill-green",
      label: `IMPORTÉ · CRC ${crc}`
    };
  }
  if (st && typeof st === "object" && st.checksumMismatch) {
    const crc =
      st.checksumMismatch.actualCrc32 ||
      st.checksumMismatch.actual_crc32 ||
      entry.actualCrc32Hex ||
      "?";
    return {
      pillClass: "pill-terracotta",
      label: `CRC ≠ (${crc} / ${expected})`
    };
  }
  if (st && typeof st === "object" && st.sizeMismatch) {
    const sz = st.sizeMismatch.actualBytes ?? st.sizeMismatch.actual_bytes ?? 0;
    return {
      pillClass: "pill-terracotta",
      label: `TAILLE ≠ (${sz} o)`
    };
  }
  return {
    pillClass: "pill-cobalt",
    label: expected === "N/A" ? "SYNTHÉTIQUE INTÉGRÉ" : `HLE INTÉGRÉ · ${expected}`
  };
}

function listInstalledBiosMap(entries) {
  const map = new Map();
  for (const b of entries || []) {
    if (!b) continue;
    const isInstalled =
      Boolean(b.installedPath) ||
      b.status === "verifiedOfficial" ||
      (b.status && typeof b.status === "object" && Boolean(b.status.customLoaded));
    if (isInstalled) {
      map.set(b.fileName, b.actualCrc32Hex || "installed");
    }
  }
  return map;
}

async function handleImportBios(buttonEl = null) {
  if (!tauriInvoke) {
    showToast("L'importation de BIOS / Firmware requiert l'application desktop Zaimule.", "error");
    return;
  }
  const prevText = buttonEl ? buttonEl.textContent : "";
  if (buttonEl) {
    buttonEl.disabled = true;
    buttonEl.textContent = "Importation...";
  }
  const res = await invokeBackend("pick_and_import_bios");
  if (buttonEl) {
    buttonEl.disabled = false;
    buttonEl.textContent = prevText;
  }
  if (res?.bootstrap) {
    applyBootstrap(res.bootstrap);
    const imported = res.report?.imported || [];
    if (imported.length > 0) {
      const verifiedCount = (res.report?.verified || []).length;
      const verifiedNote = verifiedCount > 0 ? ` (${verifiedCount} vérifié(s) CRC32)` : "";
      showToast(
        `BIOS / Firmware importés dans system/ : ${imported.join(", ")}${verifiedNote}. Relancez un jeu NDS pour activer.`,
        "ok",
        6500
      );
    } else if (res.report?.warnings?.length > 0) {
      showToast(res.report.warnings.join(" · "), "error", 7000);
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

function getVisibleCatalog() {
  const opts = {
    consoleFilter:
      state.consoleFilter === "RUST_ONLY" ||
      state.consoleFilter === "LINK_READY" ||
      state.consoleFilter === "FAVORITES"
        ? "ALL"
        : state.consoleFilter,
    onlyNativeRust: state.consoleFilter === "RUST_ONLY",
    onlyLinkReady: state.consoleFilter === "LINK_READY",
    onlyFavorites: state.consoleFilter === "FAVORITES",
    query: state.searchQuery
  };
  return sortCatalogEntries(filterCatalogEntries(state.catalog, opts), state.sortBy);
}

function getLibraryGridColumnCount() {
  const grid = document.getElementById("games-grid");
  if (!grid || typeof window.getComputedStyle !== "function") return 3;
  const cols = window.getComputedStyle(grid).gridTemplateColumns;
  if (!cols || cols === "none") return 3;
  const count = cols.trim().split(/\s+/).filter(Boolean).length;
  return Math.max(1, count);
}

function scrollSelectedCardIntoView() {
  const card = document.querySelector("#games-grid .game-card.selected");
  if (card && typeof card.scrollIntoView === "function") {
    card.scrollIntoView({ block: "nearest", inline: "nearest" });
  }
}

function setEmulationPaused(paused) {
  state.debuggerPaused = Boolean(paused);
  if (state.debuggerPaused) {
    framePacer.reset();
    apuAudio.reset();
    if (state.isEmulationRunning) {
      scheduleDebuggerRefresh(performance.now(), true);
    }
  } else {
    framePacer.reset();
    ensureEmulationLoopRunning();
  }
  const topBtn = document.getElementById("btn-emu-pause");
  if (topBtn) {
    topBtn.textContent = state.debuggerPaused ? "Reprendre" : "Pause";
    topBtn.classList.toggle("btn-paused", state.debuggerPaused);
  }
  const dbgBtn = document.getElementById("btn-dbg-pause");
  if (dbgBtn) {
    dbgBtn.textContent = state.debuggerPaused ? "Reprendre" : "Pause CPU";
  }
}

async function setViewportFullscreen(enabled) {
  const next = Boolean(enabled);
  state.isFullscreen = next;
  document.querySelector(".app-shell")?.classList.toggle("viewport-fullscreen", next);
  const btn = document.getElementById("btn-fullscreen-toggle");
  if (btn) {
    btn.textContent = next ? "Quitter" : "Plein écran";
  }
  try {
    if (appWindow?.setFullscreen) {
      await appWindow.setFullscreen(next);
    } else if (next && document.documentElement?.requestFullscreen) {
      if (!document.fullscreenElement) {
        await document.documentElement.requestFullscreen();
      }
    } else if (!next && document.fullscreenElement && document.exitFullscreen) {
      await document.exitFullscreen();
    }
  } catch {
    // Keep CSS fullscreen mode even if OS/WebView fullscreen API declines
  }
  requestAnimationFrame(() => redrawAllGlViewports());
}

function captureScreenshotPng() {
  const game = getSelectedGame();
  if (!game || isSwitchGame(game)) {
    showToast("Lancez d'abord un jeu natif (GB/GBC/GBA/NDS) pour capturer l'écran.", "info");
    return;
  }
  const canvas = document.getElementById("canvas-p1");
  if (!canvas || !canvas.width || !canvas.height) return;
  const slug = String(game.title || game.id || "zaimule")
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "")
    .slice(0, 40) || "zaimule";
  const fileName = `${slug}-f${state.currentFrame}.png`;

  const triggerLink = (href, revokeAfter = false) => {
    const a = document.createElement("a");
    a.href = href;
    a.download = fileName;
    document.body.appendChild(a);
    a.click();
    a.remove();
    if (revokeAfter) {
      setTimeout(() => URL.revokeObjectURL(href), 4000);
    }
    showToast(`Capture PNG enregistrée : ${fileName}`, "ok");
  };

  if (typeof canvas.toBlob === "function") {
    canvas.toBlob((blob) => {
      if (!blob) return;
      triggerLink(URL.createObjectURL(blob), true);
    }, "image/png");
  } else if (typeof canvas.toDataURL === "function") {
    triggerLink(canvas.toDataURL("image/png"), false);
  }
}

function syncViewportScreenAspect(game) {
  const consoleKey = String(game?.console || "GBC").toUpperCase();
  const isNds = consoleKey === "NDS";
  const isGba = consoleKey === "GBA";
  const isGb = !isNds && !isGba;
  const isSwitch = isSwitchGame(game);
  const layout = String(state.ndsLayout || "vertical").toLowerCase();
  const isHorizontal = isNds && layout.startsWith("horizontal");
  const isHybrid = isNds && layout === "hybrid";
  ["screen-1", "screen-2", "screen-3", "screen-4"].forEach((id) => {
    const el = document.getElementById(id);
    if (!el) return;
    el.classList.toggle("mode-gb", isGb);
    el.classList.toggle("mode-gba", isGba);
    el.classList.toggle("mode-nds", isNds);
    el.classList.toggle("nds-horizontal", isHorizontal);
    el.classList.toggle("nds-hybrid", isHybrid);
  });
  const layoutBtn = document.getElementById("btn-nds-layout");
  if (layoutBtn) {
    layoutBtn.style.display = isNds ? "inline-flex" : "none";
  }
  const drawerNdsRow = document.getElementById("emu-nds-layout-row");
  if (drawerNdsRow) {
    drawerNdsRow.style.display = isNds ? "flex" : "none";
  }
  const drawerNds3dRow = document.getElementById("emu-nds-3d-row");
  if (drawerNds3dRow) {
    drawerNds3dRow.style.display = isNds ? "flex" : "none";
  }
  const linkSection = document.getElementById("emu-link-section");
  if (linkSection) {
    linkSection.style.display = isNds || isSwitch ? "none" : "block";
  }
  setText(
    "emu-controls-hint",
    isNds
      ? "Flèches / ZQSD : Déplacer · J / Espace : A · K / Shift : B · E / R : L / R · Souris : Écran tactile · Retour : Rembobiner"
      : "Flèches / ZQSD : Déplacer · J / Espace : A · K / Shift : B · E / R : L / R · Entrée : Start · Retour : Rembobiner"
  );
}

function applyDisplayPreferencesToDom() {
  const shaderCss = SHADER_MODES.includes(state.activeShaderCss) ? state.activeShaderCss : "mode-clean";
  state.activeShaderCss = shaderCss;
  for (const id of ["emu-shader-select", "settings-shader-select"]) {
    const sel = document.getElementById(id);
    if (sel) sel.value = shaderCss;
  }
  ["screen-1", "screen-2", "screen-3", "screen-4"].forEach((id) => {
    const el = document.getElementById(id);
    if (!el) return;
    el.classList.remove(...SHADER_MODES);
    el.classList.add(shaderCss);
  });

  const upscalerMode = UPSCALER_LABELS[state.upscalerMode] ? state.upscalerMode : "XBRZ";
  state.upscalerMode = upscalerMode;
  for (const id of ["emu-upscaler-select", "settings-upscaler-select"]) {
    const sel = document.getElementById(id);
    if (sel) sel.value = upscalerMode;
  }

  const speed = [1, 2, 4, 8].includes(Number(state.speedMult)) ? Number(state.speedMult) : 1;
  state.speedMult = speed;
  setText("btn-speed-cycle", `${speed}x`);

  const ndsLayout = NDS_LAYOUT_LABELS[state.ndsLayout] ? state.ndsLayout : "vertical";
  state.ndsLayout = ndsLayout;
  setText("btn-nds-layout", NDS_LAYOUT_LABELS[ndsLayout]);
  for (const id of ["emu-nds-layout-select", "settings-nds-layout-select"]) {
    const sel = document.getElementById(id);
    if (sel) sel.value = ndsLayout;
  }

  const nds3dScale = Number(state.nds3dScale) >= 2 ? 2 : 1;
  state.nds3dScale = nds3dScale;
  for (const id of ["emu-nds-3d-scale-select", "settings-nds-3d-scale-select"]) {
    const sel = document.getElementById(id);
    if (sel) sel.value = String(nds3dScale);
  }

  const intScale = Boolean(state.viewportIntegerScale);
  setText(
    "btn-emu-integer-scale",
    intScale ? "Échelle entière : ON (Pixel-Exact)" : "Échelle entière : OFF (Plein cadre)"
  );
  setText(
    "btn-settings-integer-scale",
    intScale ? "Activé (Pixel-Exact)" : "Désactivé (Plein cadre)"
  );

  const rewindOn = state.rewindEnabled !== false;
  setText("btn-rewind-toggle", `Rembobinage : ${rewindOn ? "ON" : "OFF"}`);

  syncViewportScreenAspect(getSelectedGame());
  redrawAllGlViewports();
}

function releaseTransientInputs() {
  pressedKeys.clear();
  pendingNdsStylus = null;
  if (state.stylusPressed) {
    state.stylusPressed = false;
    const g = getSelectedGame();
    if (g && String(g.console || "").toUpperCase() === "NDS") {
      invokeBackend("set_nds_stylus", { x: 0, y: 0, pressed: false }, { silent: true });
    }
  }
}

function stopEmulationPlayback() {
  state.isEmulationRunning = false;
  setEmulationPaused(false);
  framePacer.reset();
  lastTelemetryDomUpdateMs = 0;
  apuAudio.reset();
  releaseTransientInputs();
  hideEmuBanner();
  renderStandbyCanvas();
  setText("tel-fps", "Prêt");
}

async function launchSelectedGame() {
  if (selectGameInFlightPromise) {
    try {
      await selectGameInFlightPromise;
    } catch {
      // Handled by caller
    }
  }
  const game = getSelectedGame();
  if (!game) {
    await handleImportRomsTrigger();
    return;
  }

  if (isSwitchGame(game)) {
    stopEmulationPlayback();
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
      openSettingsTab("settings-pane-switch");
    }
    return;
  }

  apuAudio.ensureContext();
  apuAudio.reset();
  setEmulationPaused(false);
  hideEmuBanner();

  syncViewportScreenAspect(game);
  framePacer.reset();
  lastTelemetryDomUpdateMs = 0;
  state.isEmulationRunning = true;
  const targetFps = targetFpsForConsole(game.console);
  const hasGl = Boolean(getGlRenderer("canvas-p1"));
  setText("tel-fps", `${Math.round(targetFps * state.speedMult)} FPS${hasGl ? " · GPU" : ""}`);
  switchTab("view-emulator");
  ensureEmulationLoopRunning();
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

function pollGamepadBitmask(pad = getConnectedGamepad()) {
  if (pad) {
    const label = pad.id.slice(0, 18);
    if (label !== lastGamepadLabel) {
      lastGamepadLabel = label;
      const el = getCachedEl("tel-pad");
      if (el) el.textContent = label;
    }
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

  const tabs = ["view-library", "view-emulator", "view-settings"];
  const activePane = activePaneId || "view-library";

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
    const visible = getVisibleCatalog();
    if (visible.length === 0) {
      if (action === "LAUNCH_GAME") {
        await launchSelectedGame();
      }
      return;
    }
    const idx = visible.findIndex((g) => g.id === state.activeGameId);
    if (action === "PREV_GAME" || action === "NEXT_GAME" || action === "PREV_ROW" || action === "NEXT_ROW") {
      const cols = getLibraryGridColumnCount();
      const step =
        action === "PREV_GAME" ? -1 : action === "NEXT_GAME" ? 1 : action === "PREV_ROW" ? -cols : cols;
      const baseIdx = idx >= 0 ? idx : 0;
      const nextIdx = (baseIdx + step + visible.length * 4) % visible.length;
      const nextGame = visible[nextIdx];
      if (nextGame) {
        state.activeGameId = nextGame.id;
        const boot = await invokeBackend("select_game", { gameId: nextGame.id });
        if (boot) applyBootstrap(boot);
        else updateAllViews();
        scrollSelectedCardIntoView();
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
  setTxt("count-favorites", counts.FAVORITES);
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

  const filtered = getVisibleCatalog();
  grid.innerHTML = "";

  if (filtered.length === 0) {
    const emptyEl = document.createElement("div");
    emptyEl.className = "empty-catalog-state";
    if (state.catalog.length === 0) {
      emptyEl.innerHTML = `
        <div class="empty-catalog-icon" aria-hidden="true">
          <svg viewBox="0 0 24 24"><rect x="4" y="3" width="16" height="18" rx="2"/><path d="M8 7h8M9 14h6M12 11v6"/></svg>
        </div>
        <div class="empty-catalog-title">Aucun jeu dans votre bibliothèque</div>
        <div class="empty-catalog-desc">
          Ajoutez vos jeux <strong>Game Boy / GBC</strong>, <strong>GBA</strong>, <strong>Nintendo DS</strong>
          ou <strong>Nintendo Switch</strong> (.gb, .gbc, .gba, .nds, .nsp, .xci, .nx, .zip, .7z),
          ou glissez-déposez vos fichiers directement dans cette fenêtre.
        </div>
        <div class="empty-catalog-actions">
          <button class="btn btn-primary" id="btn-empty-import-rom" type="button">Ajouter un jeu...</button>
          <button class="btn" id="btn-empty-scan-folder" type="button">Scanner un dossier...</button>
          <button class="btn btn-ghost" id="btn-empty-open-vault" type="button">Configurer Switch</button>
        </div>
      `;
      emptyEl.querySelector("#btn-empty-import-rom")?.addEventListener("click", () => {
        handleImportRomsTrigger();
      });
      emptyEl.querySelector("#btn-empty-scan-folder")?.addEventListener("click", () => {
        handleScanFolderTrigger();
      });
      emptyEl.querySelector("#btn-empty-open-vault")?.addEventListener("click", () => {
        openSettingsTab("settings-pane-switch");
      });
    } else {
      emptyEl.innerHTML = `
        <div class="empty-catalog-title">Aucun jeu ne correspond à votre recherche</div>
        <div class="empty-catalog-desc">Modifiez votre filtre ou sélectionnez « Tous les jeux ».</div>
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
    const switchBadge = switchMode
      ? `<span class="core-tag mono core-external">${resolveTargetEngineForGame(game) === "fritchii" ? "FRITCHII" : "TORZU"}</span>`
      : "";
    const regionLabel = String(game.region || "").split("(")[0].trim();

    const card = document.createElement("article");
    card.className = `game-card${game.id === state.activeGameId ? " selected" : ""}`;
    card.dataset.gameId = game.id;
    card.dataset.console = consoleKey;
    card.setAttribute("role", "listitem");
    card.tabIndex = 0;

    card.innerHTML = `
      <div class="game-jacket-stage">
        <div class="game-cover" style="--cover-bg: ${spec.bg};">
          <button class="game-fav-btn${game.favorite ? " active" : ""}" type="button" title="${game.favorite ? "Retirer des favoris" : "Ajouter aux favoris"}" aria-label="${game.favorite ? "Retirer" : "Ajouter"} ${escapeHtml(game.title)} ${game.favorite ? "des" : "aux"} favoris">${game.favorite ? "★" : "☆"}</button>
          <button class="game-remove-btn" type="button" title="Retirer de la bibliothèque" aria-label="Retirer ${escapeHtml(game.title)}">✕</button>
          ${
            hasSpine
              ? `<div class="cover-spine"><span class="cover-spine-label">${escapeHtml(spec.spine)}</span></div>`
              : ""
          }
          <div class="cover-top">
            <span class="console-tag mono">${escapeHtml(consoleKey === "SWITCH2" ? "SWITCH 2" : consoleKey)}</span>
            ${switchBadge}
          </div>
          <div class="cover-art-graphic" aria-hidden="true">${spec.svg}</div>
          <span class="cover-action-prompt">Jouer</span>
          <div class="cover-bottom mono">
            <span>${escapeHtml(regionLabel || "Jeu")}</span>
            <span>${escapeHtml(formatFileSize(game.sizeBytes))}</span>
          </div>
        </div>
      </div>
      <div class="game-body">
        <div class="game-title" title="${escapeHtml(game.title)}">${escapeHtml(game.title)}</div>
        <div class="game-meta">
          <span>${escapeHtml(consoleKey === "SWITCH2" ? "Switch 2" : consoleKey)}</span>
          <span class="mono">${escapeHtml(formatPlaytime(game.playtimeSeconds))}</span>
        </div>
      </div>
    `;

    card.querySelector(".game-fav-btn")?.addEventListener("click", async (e) => {
      e.stopPropagation();
      if (tauriInvoke) {
        const boot = await invokeBackend("toggle_game_favorite", { gameId: game.id });
        if (boot) applyBootstrap(boot);
      } else {
        game.favorite = !game.favorite;
        updateAllViews();
      }
    });

    card.querySelector(".game-remove-btn")?.addEventListener("click", async (e) => {
      e.stopPropagation();
      const removingActive = state.activeGameId === game.id;
      if (removingActive && state.isEmulationRunning) {
        stopEmulationPlayback();
      }
      if (tauriInvoke) {
        const boot = await invokeBackend("remove_game_from_library", { gameId: game.id });
        if (boot) {
          applyBootstrap(boot);
          showToast(`${game.title} retiré de la bibliothèque.`);
        }
      } else {
        state.catalog = state.catalog.filter((item) => item.id !== game.id);
        if (removingActive) {
          state.activeGameId = state.catalog[0]?.id || "";
        }
        updateAllViews();
      }
    });

    const selectCardGame = () => {
      if (state.activeGameId === game.id && selectGameInFlightId === game.id && selectGameInFlightPromise) {
        return selectGameInFlightPromise;
      }
      const alreadySelected = state.activeGameId === game.id;
      state.activeGameId = game.id;
      // Fast O(N) class toggle without destroying/recreating DOM nodes
      grid.querySelectorAll(".game-card").forEach((el) => {
        el.classList.toggle("selected", el.dataset.gameId === game.id);
      });
      renderInspector();
      if (alreadySelected && !selectGameInFlightPromise) {
        return Promise.resolve();
      }
      selectGameInFlightId = game.id;
      const p = invokeBackend("select_game", { gameId: game.id })
        .then((boot) => {
          if (boot) {
            applyBootstrap(boot, { skipCatalogGridRebuild: true });
          }
        })
        .finally(() => {
          if (selectGameInFlightPromise === p) {
            selectGameInFlightPromise = null;
            selectGameInFlightId = "";
          }
        });
      selectGameInFlightPromise = p;
      return p;
    };

    card.addEventListener("click", () => {
      selectCardGame();
    });
    card.addEventListener("dblclick", async () => {
      await selectCardGame();
      await launchSelectedGame();
    });
    grid.appendChild(card);
  }
}

function renderInspector() {
  const g = getSelectedGame();
  const playLabelEl = document.querySelector("#btn-insp-play span:first-child");
  const pathRow = document.getElementById("insp-path-row");
  const pathVal = document.getElementById("insp-path");
  const switchEngineRow = document.getElementById("insp-switch-engine-row");
  const switchEngineSel = document.getElementById("insp-switch-engine");

  if (g) {
    const consoleKey = String(g.console || "").toUpperCase();
    const isSwitch = isSwitchGame(g);
    const targetEngine = resolveTargetEngineForGame(g);

    setText("insp-title", g.title);
    setText("insp-console", consoleKey === "SWITCH2" ? "SWITCH 2" : consoleKey);
    setText("insp-region", String(g.region || "—").split("(")[0].trim());
    setText("insp-playtime", formatPlaytime(g.playtimeSeconds));
    setText("active-emu-game", g.title);

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
          ? "Jouer avec Fritchii"
          : "Jouer avec Torzu"
        : "Jouer";
    }
    syncViewportScreenAspect(g);
  } else {
    setText("insp-title", "Aucun jeu sélectionné");
    setText("insp-console", "—");
    setText("insp-region", "—");
    setText("insp-playtime", "0m");
    setText("active-emu-game", "Aucun jeu lancé");
    if (pathRow) pathRow.style.display = "none";
    if (switchEngineRow) switchEngineRow.style.display = "none";
    if (playLabelEl) playLabelEl.textContent = "Jouer";
    syncViewportScreenAspect(null);
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
        const row = document.createElement("div");
        row.className = "mod-item";
        row.innerHTML = `
          <label style="display:flex; align-items:center; gap:8px; min-width:0; flex:1; cursor:pointer;">
            <input type="checkbox" ${c.enabled ? "checked" : ""} />
            <span class="path-ellipsis">${escapeHtml(String(c.label || "").replace(/^[^\w]+/, ""))}</span>
          </label>
          <span style="display:inline-flex; align-items:center; gap:6px; flex-shrink:0;">
            <span class="mono" style="font-size:10.5px; color:var(--accent-primary);">${escapeHtml(c.codeText)}</span>
            <button class="btn mono btn-inline-delete btn-remove-cheat" type="button" title="Supprimer ce code" aria-label="Supprimer ce code">✕</button>
          </span>
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
        row.querySelector(".btn-remove-cheat")?.addEventListener("click", async () => {
          if (tauriInvoke) {
            const boot = await invokeBackend("remove_cheat_code", { cheatId: c.id });
            if (boot) applyBootstrap(boot);
          } else {
            state.cheatCodes = state.cheatCodes.filter((item) => item.id !== c.id);
            renderInspector();
          }
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
    const selected = getSelectedGame();
    const canUseStates = Boolean(selected && !isSwitchGame(selected));
    for (const slot of state.saveSlots.slice(0, 9)) {
      const card = document.createElement("div");
      card.className = "slot-card";
      const metaLabel = slot.occupied ? slot.label || "Sauvegardé" : "Libre";
      card.innerHTML = `
        <div class="slot-head">
          <div class="slot-num mono">Emplacement #${slot.slot}</div>
          <div class="slot-time" title="${escapeHtml(metaLabel)}">${escapeHtml(metaLabel)}</div>
        </div>
        <div class="slot-actions">
          <button class="slot-action-btn btn-slot-save" type="button" ${canUseStates ? "" : "disabled"}>
            ${slot.occupied ? "Écraser" : "Sauver"}
          </button>
          <button class="slot-action-btn btn-slot-load" type="button" ${canUseStates && slot.occupied ? "" : "disabled"}>
            Charger
          </button>
          <button class="slot-action-btn danger btn-slot-delete" type="button" title="Supprimer l'emplacement #${slot.slot}" aria-label="Supprimer l'emplacement #${slot.slot}" ${canUseStates && slot.occupied ? "" : "disabled"}>
            ✕
          </button>
        </div>
      `;

      card.querySelector(".btn-slot-save")?.addEventListener("click", async () => {
        if (!canUseStates) return;
        const info = await invokeBackend("create_save_state", {
          slot: slot.slot,
          label: `Sauvegarde #${slot.slot}`
        });
        if (info) {
          const boot = await invokeBackend("get_station_bootstrap");
          if (boot) applyBootstrap(boot);
          showToast(`Emplacement #${slot.slot} sauvegardé.`, "ok", 2600);
        } else if (!tauriInvoke) {
          slot.occupied = true;
          slot.label = `Sauvegarde #${slot.slot}`;
          renderInspector();
        }
      });

      card.querySelector(".btn-slot-load")?.addEventListener("click", async () => {
        if (!canUseStates || !slot.occupied) return;
        const info = await invokeBackend("restore_save_state", { slot: slot.slot });
        if (info || !tauriInvoke) {
          showToast(`Emplacement #${slot.slot} chargé.`, "ok", 2600);
        }
      });

      card.querySelector(".btn-slot-delete")?.addEventListener("click", async () => {
        if (!canUseStates || !slot.occupied) return;
        if (tauriInvoke) {
          const slots = await invokeBackend("delete_save_state", { slot: slot.slot });
          if (Array.isArray(slots)) {
            state.saveSlots = slots;
            renderInspector();
            showToast(`Emplacement #${slot.slot} supprimé.`, "info", 2600);
          }
        } else {
          slot.occupied = false;
          slot.frame = 0;
          slot.crc32Hex = "00000000";
          slot.label = `Slot #${slot.slot} Vide`;
          renderInspector();
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
        invalidateKeyBindingsCache();
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
          setText("macro-status", nextId ? "LECTURE TAS" : "PRÊT");
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
          <span class="path-ellipsis" style="min-width:0; flex:1;"><strong style="color:var(--accent-primary)">${escapeHtml(wp.addrHex)}</strong> ${escapeHtml(wp.label)} : <strong style="color:var(--status-ok)">${hexVal}</strong></span>
          <span style="display:inline-flex; align-items:center; gap:4px; flex-shrink:0;">
            <button class="btn mono btn-toggle-wp" type="button" style="padding:1px 6px; font-size:9.5px; color:${wp.frozen ? "var(--accent-secondary)" : "var(--text-muted)"};">
              ${wp.frozen ? "Gelé" : "Pin"}
            </button>
            <button class="btn mono btn-inline-delete btn-remove-wp" type="button" title="Supprimer ce watchpoint" aria-label="Supprimer ce watchpoint">✕</button>
          </span>
        `;
        row.querySelector(".btn-toggle-wp")?.addEventListener("click", async () => {
          const boot = await invokeBackend("toggle_watchpoint_freeze", { watchId: wp.id });
          if (boot) applyBootstrap(boot);
          else if (!tauriInvoke) {
            wp.frozen = !wp.frozen;
            renderKeymapperAndMacros();
          }
        });
        row.querySelector(".btn-remove-wp")?.addEventListener("click", async () => {
          if (tauriInvoke) {
            const boot = await invokeBackend("remove_memory_watchpoint", { watchId: wp.id });
            if (boot) applyBootstrap(boot);
          } else {
            state.memoryWatchpoints = state.memoryWatchpoints.filter((item) => item.id !== wp.id);
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
    portBadge.textContent = state.udpLocalPort
      ? `UDP 0.0.0.0:${state.udpLocalPort}`
      : "UDP inactif";
  }
  const packetsBadge = document.getElementById("udp-packets-badge");
  if (packetsBadge) {
    packetsBadge.textContent = state.udpPeerAddr
      ? `Pair ${state.udpPeerAddr} (${state.udpPacketsSent} ↑ / ${state.udpPacketsReceived} ↓)`
      : "Aucun pair connecté";
  }
  const disconnectBtn = document.getElementById("btn-disconnect-peer");
  if (disconnectBtn) {
    disconnectBtn.style.display = state.udpPeerAddr ? "inline-flex" : "none";
  }
  const peerInp = document.getElementById("netplay-peer-input");
  if (peerInp && document.activeElement !== peerInp && state.udpPeerAddr) {
    peerInp.value = state.udpPeerAddr;
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
              ? `<button class="btn mono" style="padding:1px 6px; font-size:10px; border-color:var(--status-ok); color:var(--status-ok);">Charger Slot #${sharedSlot}</button>`
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
    for (const r of state.netplayRooms) {
      const card = document.createElement("div");
      card.className = "panel-card";
      card.innerHTML = `
        <div class="section-title">
          <span>${escapeHtml(r.title)}</span>
          <span class="pill-badge pill-green mono">${r.pingMs} ms</span>
        </div>
        <div class="kv-row"><span class="kv-label">Jeu</span><span class="kv-val">${escapeHtml(r.gameTitle)} (${escapeHtml(r.console)})</span></div>
        <div class="kv-row"><span class="kv-label">Hôte</span><span class="kv-val mono">${escapeHtml(r.hostName)} (${r.playersCurrent}/${r.playersMax})</span></div>
        <div class="kv-row"><span class="kv-label">Lien</span><span class="kv-val mono" style="color:var(--accent-secondary)">${escapeHtml(r.deepLinkUri)}</span></div>
        <div style="margin-top:10px; display:flex; justify-content:flex-end;">
          <button class="btn btn-primary" style="font-size:11px;">Rejoindre &amp; Jouer</button>
        </div>
      `;
      card.querySelector("button").addEventListener("click", async () => {
        const peerAddr = document.getElementById("netplay-peer-input")?.value?.trim() || null;
        const boot = await invokeBackend("join_netplay_deep_link", {
          uri: r.deepLinkUri,
          peerAddr
        });
        if (boot) applyBootstrap(boot);
        if (!state.dualLinkMode) {
          document.getElementById("btn-toggle-dual")?.click();
        }
        launchSelectedGame();
      });
      netGrid.appendChild(card);
    }
    if (netGrid.children.length === 0) {
      netGrid.innerHTML = `<div class="empty-list-hint">Aucun salon actif. Créez un salon ou saisissez une adresse IP:PORT pour jouer en réseau local.</div>`;
    }
  }

  // 1. Paramètres -> Bibliothèque & Dossiers
  const libContainer = document.getElementById("settings-library-container");
  if (libContainer) {
    libContainer.innerHTML = "";
    const foldersCard = document.createElement("div");
    foldersCard.className = "panel-card";
    foldersCard.innerHTML = `
      <div class="section-title">
        <span>Dossiers de jeux</span>
        <span class="mono" style="font-size:10px; color:var(--text-muted);">${state.romDirectories.length} dossier(s)</span>
      </div>
      <div class="vault-actions" style="margin-bottom:12px;">
        <button class="btn btn-primary" id="btn-cores-import-files" type="button">Ajouter des jeux...</button>
        <button class="btn" id="btn-cores-scan-folder" type="button">Scanner un dossier...</button>
        ${
          state.romDirectories.length > 0
            ? `<button class="btn" id="btn-cores-rescan-all" type="button">Tout actualiser</button>`
            : ""
        }
      </div>
      <div id="rom-dirs-container"></div>
      ${
        state.lastScanReport
          ? `<div class="mono" style="margin-top:10px; font-size:10.5px; color:var(--text-secondary);">
               Dernier scan (${escapeHtml(state.lastScanReport.scannedRoot)}) : ${state.lastScanReport.importedEntries?.length || 0} jeu(x) ajouté(s)
             </div>`
          : ""
      }
    `;
    const dirsWrap = foldersCard.querySelector("#rom-dirs-container");
    if (dirsWrap) {
      if (state.romDirectories.length > 0) {
        const listBox = document.createElement("div");
        listBox.style.cssText = "padding-top:8px; border-top:1px solid var(--border-subtle);";
        for (const dir of state.romDirectories) {
          const row = document.createElement("div");
          row.className = "rom-dir-row";
          row.innerHTML = `
            <span class="mono path-ellipsis" style="font-size:11px; color:var(--text-secondary); min-width:0; flex:1;" title="${escapeHtml(dir)}">${escapeHtml(dir)}</span>
            <span class="rom-dir-actions">
              <button class="btn btn-rescan-dir" type="button" style="padding:2px 8px; font-size:11px;" title="Actualiser ce dossier">Actualiser</button>
              <button class="btn btn-inline-delete btn-remove-dir" type="button" title="Retirer ce dossier" aria-label="Retirer ce dossier">✕</button>
            </span>
          `;
          row.querySelector(".btn-rescan-dir")?.addEventListener("click", async (ev) => {
            const btn = ev.currentTarget;
            if (!tauriInvoke) {
              showToast("Le rescan de dossier requiert l'application desktop Zaimule.", "error");
              return;
            }
            btn.disabled = true;
            btn.textContent = "Scan...";
            const boot = await invokeBackend("scan_rom_directory", { dirPath: dir });
            if (boot) {
              applyBootstrap(boot);
              const added = boot.lastScanReport?.importedEntries?.length || 0;
              showToast(
                added > 0
                  ? `${added} jeu(x) ajouté(s) depuis ${dir}.`
                  : "Dossier à jour : aucun nouveau jeu.",
                added > 0 ? "ok" : "info"
              );
            } else {
              btn.disabled = false;
              btn.textContent = "Actualiser";
            }
          });
          row.querySelector(".btn-remove-dir")?.addEventListener("click", async () => {
            if (tauriInvoke) {
              const boot = await invokeBackend("remove_rom_directory", { dirPath: dir });
              if (boot) {
                applyBootstrap(boot);
                showToast("Dossier retiré de la liste (vos jeux déjà ajoutés restent conservés).", "info");
              }
            } else {
              state.romDirectories = state.romDirectories.filter((d) => d !== dir);
              renderNetplayAndCores();
            }
          });
          listBox.appendChild(row);
        }
        dirsWrap.appendChild(listBox);
      } else {
        dirsWrap.innerHTML = `<div class="empty-list-hint">Aucun dossier configuré. Ajoutez vos fichiers ou un dossier pour remplir la bibliothèque.</div>`;
      }
    }
    foldersCard.querySelector("#btn-cores-import-files")?.addEventListener("click", () => {
      handleImportRomsTrigger();
    });
    foldersCard.querySelector("#btn-cores-scan-folder")?.addEventListener("click", () => {
      handleScanFolderTrigger();
    });
    foldersCard.querySelector("#btn-cores-rescan-all")?.addEventListener("click", async (ev) => {
      const btn = ev.currentTarget;
      if (!tauriInvoke) {
        showToast("Le rescan des dossiers requiert l'application desktop Zaimule.", "error");
        return;
      }
      const prev = btn.textContent;
      btn.disabled = true;
      btn.textContent = "Scan...";
      const boot = await invokeBackend("rescan_all_rom_directories");
      if (boot) {
        applyBootstrap(boot);
        const added = boot.lastScanReport?.importedEntries?.length || 0;
        showToast(
          added > 0
            ? `${added} nouveau(x) jeu(x) trouvé(s).`
            : "Bibliothèque déjà à jour.",
          added > 0 ? "ok" : "info"
        );
      } else {
        btn.disabled = false;
        btn.textContent = prev;
      }
    });
    libContainer.appendChild(foldersCard);
  }

  // 2. Paramètres -> Nintendo Switch
  const switchContainer = document.getElementById("settings-switch-container");
  if (switchContainer) {
    switchContainer.innerHTML = "";
    const vault = state.switchVault || {};
    const keys = vault.keys || {};
    const fw = vault.firmware || {};
    const prodOk = Boolean(keys.prodKeysFound);
    const titleOk = Boolean(keys.titleKeysFound);
    const fwOk = Boolean(fw.installed);

    const vaultCard = document.createElement("div");
    vaultCard.className = "panel-card";
    vaultCard.innerHTML = `
      <div class="section-title">
        <span>Coffre Switch (Torzu &amp; Fritchii)</span>
        <button class="btn" id="btn-refresh-vault" type="button" style="padding:2px 8px; font-size:11px;">Actualiser</button>
      </div>
      <div class="switch-vault-row">
        <div class="switch-vault-head">
          <div style="font-weight:600;">1. Clés Nintendo Switch (prod.keys / title.keys)</div>
          <span class="pill-badge ${prodOk ? "pill-green" : "pill-terracotta"} mono">
            ${prodOk ? (titleOk ? "Clés prêtes" : "prod.keys OK") : "Requis"}
          </span>
        </div>
        <div class="mono path-ellipsis" style="font-size:10.5px; color:${prodOk ? "var(--status-ok)" : "var(--text-muted)"};" title="${escapeHtml(keys.prodKeysPath || keys.keysDir || "")}">
          ${
            prodOk
              ? escapeHtml(keys.prodKeysPath)
              : "Importez vos fichiers prod.keys / title.keys pour déchiffrer vos jeux Switch."
          }
        </div>
        <div class="vault-actions">
          <button class="btn btn-primary" id="btn-import-switch-keys" type="button" style="padding:4px 10px; font-size:11px;">
            Importer Clés (.keys)...
          </button>
        </div>
      </div>
      <div class="switch-vault-row">
        <div class="switch-vault-head">
          <div style="font-weight:600;">2. Firmware Nintendo Switch (.nca)</div>
          <span class="pill-badge ${fwOk ? "pill-green" : "pill-gold"} mono">
            ${
              fwOk
                ? `${fw.ncaCount} fichiers · ${escapeHtml(fw.versionHint || formatFileSize(fw.totalBytes || 0))}`
                : "Recommandé"
            }
          </span>
        </div>
        <div class="mono path-ellipsis" style="font-size:10.5px; color:${fwOk ? "var(--status-ok)" : "var(--text-muted)"};" title="${escapeHtml(fw.firmwareDir || "")}">
          ${
            fwOk
              ? `${escapeHtml(fw.firmwareDir)} (${escapeHtml(formatFileSize(fw.totalBytes || 0))})`
              : "Nécessaire pour les polices système et certains jeux récents (MK8D, Animal Crossing, Pokémon)."
          }
        </div>
        <div class="vault-actions">
          <button class="btn btn-primary" id="btn-import-switch-fw-archive" type="button" style="padding:4px 10px; font-size:11px;">
            ${fwOk ? "Remplacer (.zip / .7z)..." : "Importer archive (.zip / .7z)..."}
          </button>
          <button class="btn" id="btn-import-switch-fw-folder" type="button" style="padding:4px 10px; font-size:11px;">
            Importer dossier...
          </button>
        </div>
      </div>
      <div id="vault-engines-wrap"></div>
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
    vaultCard.querySelector("#btn-import-switch-fw-archive")?.addEventListener("click", (ev) => {
      handleImportSwitchFirmware("archive", ev.currentTarget);
    });
    vaultCard.querySelector("#btn-import-switch-fw-folder")?.addEventListener("click", (ev) => {
      handleImportSwitchFirmware("folder", ev.currentTarget);
    });
    const enginesWrap = vaultCard.querySelector("#vault-engines-wrap");
    if (enginesWrap) {
      enginesWrap.appendChild(
        buildSwitchEngineBlock(vault.torzu, "torzu", "3. Torzu (Nintendo Switch 1)")
      );
      enginesWrap.appendChild(
        buildSwitchEngineBlock(vault.fritchii, "fritchii", "4. Fritchii (Nintendo Switch 2)")
      );
    }
    switchContainer.appendChild(vaultCard);
  }

  // 3. Paramètres -> Avancé : BIOS optionnels
  const biosContainer = document.getElementById("settings-bios-container");
  if (biosContainer) {
    biosContainer.innerHTML = "";
    const biosCard = document.createElement("div");
    biosCard.className = "panel-card";
    biosCard.innerHTML = `
      <div class="section-title">
        <span>BIOS &amp; Firmware (Optionnel)</span>
        <button class="btn" id="btn-import-bios" type="button" style="padding:3px 10px; font-size:11px;">
          Importer BIOS (.zip / .7z / .bin)...
        </button>
      </div>
      <div style="font-size:12px; color:var(--text-secondary); margin-bottom:10px; line-height:1.45;">
        Les consoles fonctionnent déjà prêtes à l'emploi sans BIOS. Importer vos propres fichiers DS
        (<strong>bios7.bin</strong>, <strong>bios9.bin</strong>, <strong>firmware.bin</strong>) améliore la compatibilité de certains titres.
      </div>
      ${state.biosEntries
        .map((b) => {
          const badge = describeBiosStatus(b);
          const pathAttr = b.installedPath ? ` title="${escapeHtml(b.installedPath)}"` : "";
          return `
          <div class="mod-item"${pathAttr}>
            <div>
              <div class="mono" style="font-weight:600">${escapeHtml(b.fileName)} (${escapeHtml(String(b.console || "").toUpperCase())})</div>
              <div style="font-size:11px; color:var(--text-muted)">${escapeHtml(b.notes)}</div>
            </div>
            <span class="pill-badge ${badge.pillClass} mono">${escapeHtml(badge.label)}</span>
          </div>
        `;
        })
        .join("")}
    `;
    biosCard.querySelector("#btn-import-bios")?.addEventListener("click", (ev) => {
      handleImportBios(ev.currentTarget);
    });
    biosContainer.appendChild(biosCard);
  }

  // 4. Paramètres -> Avancé : Cœurs natifs intégrés
  const nativeContainer = document.getElementById("settings-native-cores-container");
  if (nativeContainer) {
    nativeContainer.innerHTML = "";
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
        </div>
        <span class="pill-badge pill-green mono">PRÊT</span>
      `;
      nativeContainer.appendChild(row);
    }
  }
}

function applyBootstrap(boot, options = {}) {
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
  invalidateKeyBindingsCache();
  state.macros = Array.isArray(boot.macros) ? boot.macros : state.macros;
  state.tasTimeline = Array.isArray(boot.tasTimeline) ? boot.tasTimeline : state.tasTimeline;
  state.netplayRooms = Array.isArray(boot.netplayRooms) ? boot.netplayRooms : state.netplayRooms;
  state.udpLocalPort =
    boot.udpLocalPort !== undefined ? Number(boot.udpLocalPort) || 0 : state.udpLocalPort;
  state.udpPeerAddr = boot.udpPeerAddr ?? null;
  state.udpPacketsSent = Number(boot.udpPacketsSent) || 0;
  state.udpPacketsReceived = Number(boot.udpPacketsReceived) || 0;
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
  if (typeof boot.activeShaderCss === "string" && boot.activeShaderCss) {
    state.activeShaderCss = boot.activeShaderCss;
  }
  if (typeof boot.upscalerMode === "string" && boot.upscalerMode) {
    state.upscalerMode = boot.upscalerMode;
  }
  if ([1, 2, 4, 8].includes(Number(boot.speedMultiplier))) {
    state.speedMult = Number(boot.speedMultiplier);
  }
  if (typeof boot.ndsLayout === "string" && boot.ndsLayout) {
    state.ndsLayout = boot.ndsLayout;
  }
  if (boot.nds3dScale !== undefined) {
    state.nds3dScale = Number(boot.nds3dScale) >= 2 ? 2 : 1;
  }
  if (boot.viewportIntegerScale !== undefined) {
    state.viewportIntegerScale = Boolean(boot.viewportIntegerScale);
  }
  if (boot.rewindEnabled !== undefined) {
    state.rewindEnabled = Boolean(boot.rewindEnabled);
  }
  state.warning = boot.warning || null;
  if (state.warning && state.warning !== lastSurfacedWarning) {
    lastSurfacedWarning = state.warning;
    showToast(state.warning, "error", 7000);
  }
  if (state.isEmulationRunning && (!getSelectedGame() || isSwitchGame(getSelectedGame()))) {
    stopEmulationPlayback();
  }
  applyDisplayPreferencesToDom();
  if (options.skipCatalogGridRebuild) {
    renderSidebarCounts();
    renderInspector();
    renderKeymapperAndMacros();
    renderNetplayAndCores();
  } else {
    updateAllViews();
  }
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
  const prevPane = activePaneId || document.querySelector(".view-pane.active")?.id;
  activePaneId = targetId;
  if (prevPane === "view-emulator" && targetId !== "view-emulator") {
    if (state.isFullscreen) {
      setViewportFullscreen(false);
    }
    framePacer.reset();
    apuAudio.reset();
    releaseTransientInputs();
  } else if (targetId === "view-emulator") {
    framePacer.reset();
    lastTelemetryDomUpdateMs = 0;
  }
  document.querySelectorAll(".nav-tab").forEach((btn) => {
    const isActive = btn.dataset.target === targetId;
    btn.classList.toggle("active", isActive);
    btn.setAttribute("aria-selected", String(isActive));
  });
  document.querySelectorAll(".view-pane").forEach((pane) => {
    pane.classList.toggle("active", pane.id === targetId);
  });
  if (targetId === "view-emulator") {
    requestAnimationFrame(() => redrawAllGlViewports());
    ensureEmulationLoopRunning();
  } else if (prevPane === "view-emulator" && tauriInvoke) {
    // Refresh playtime / save state metadata once when returning from an active emulation session
    invokeBackend("get_station_bootstrap", {}, { silent: true }).then((boot) => {
      if (boot) applyBootstrap(boot);
    });
  }
}

function renderStandbyCanvas() {
  const glP1 = getGlRenderer("canvas-p1");
  if (glP1) {
    const w = 160;
    const h = 144;
    const standby = new Uint8Array(w * h * 4);
    for (let i = 0; i < standby.length; i += 4) {
      standby[i] = 17;
      standby[i + 1] = 15;
      standby[i + 2] = 14;
      standby[i + 3] = 255;
    }
    glP1.renderFrame(
      { width: w, height: h, rgba: standby },
      {
        upscalerMode: "OFF",
        shaderCss: "mode-clean",
        isNds: false,
        ndsLayout: "vertical",
        integerScale: false
      }
    );
  } else {
    const p1 = getCanvasEntry("canvas-p1", true);
    if (p1) {
      const { canvas, ctx } = p1;
      const w = canvas.width || 160;
      const h = canvas.height || 144;
      ctx.fillStyle = "#110F0E";
      ctx.fillRect(0, 0, w, h);
      ctx.strokeStyle = "#282422";
      ctx.lineWidth = 1;
      ctx.strokeRect(8.5, 8.5, w - 17, h - 17);
    }
  }
  const oscEntry = getCanvasEntry("audio-oscilloscope-canvas", true);
  if (oscEntry) {
    const { canvas: osc, ctx } = oscEntry;
    ctx.fillStyle = "#110F0E";
    ctx.fillRect(0, 0, osc.width, osc.height);
    ctx.strokeStyle = "#332E2B";
    ctx.beginPath();
    ctx.moveTo(0, osc.height / 2);
    ctx.lineTo(osc.width, osc.height / 2);
    ctx.stroke();
  }
}

function paintRgbaFrame(canvasId, frame, activeGame = null) {
  if (!frame || !frame.rgba) return;
  const isPlayerCanvas = canvasId.startsWith("canvas-p");
  if (isPlayerCanvas) {
    const glRenderer = getGlRenderer(canvasId);
    if (glRenderer) {
      const g = activeGame || getSelectedGame();
      const isNds = Boolean(g && String(g.console || "").toUpperCase() === "NDS");
      if (
        glRenderer.renderFrame(frame, {
          upscalerMode: state.upscalerMode,
          shaderCss: state.activeShaderCss,
          isNds,
          ndsLayout: state.ndsLayout,
          integerScale: Boolean(state.viewportIntegerScale)
        })
      ) {
        return;
      }
    }
  }

  const entry = getCanvasEntry(canvasId, isPlayerCanvas);
  if (!entry) return;
  const { canvas, ctx } = entry;

  let processed =
    isPlayerCanvas && state.upscalerMode !== "OFF"
      ? upscaleFrameIfEnabled(frame, state.upscalerMode)
      : frame;

  if (isPlayerCanvas && state.ndsLayout !== "vertical") {
    const g = activeGame || getSelectedGame();
    if (g && String(g.console || "").toUpperCase() === "NDS") {
      processed = arrangeNdsDualScreenFrame(processed, state.ndsLayout);
    }
  }

  const w = processed.width;
  const h = processed.height;
  const expectedLen = w * h * 4;
  if (!w || !h || processed.rgba.length !== expectedLen) return;

  if (canvas.width !== w || canvas.height !== h) {
    canvas.width = w;
    canvas.height = h;
  }

  if (typeof ImageData === "function" && processed.rgba instanceof Uint8ClampedArray) {
    ctx.putImageData(new ImageData(processed.rgba, w, h), 0, 0);
    return;
  }

  if (!entry.imgData || entry.width !== w || entry.height !== h) {
    entry.width = w;
    entry.height = h;
    entry.imgData = ctx.createImageData(w, h);
  }
  entry.imgData.data.set(processed.rgba);
  ctx.putImageData(entry.imgData, 0, 0);
}

function renderAudioOscilloscope(freqHz, frameNumber, pcmSamples = null, nowMs = 0) {
  if (state.isFullscreen || !oscCanvasVisible) return;
  if (nowMs > 0 && nowMs - lastOscilloscopeDrawMs < 50) return;
  lastOscilloscopeDrawMs = nowMs || performance.now();

  const entry = getCanvasEntry("audio-oscilloscope-canvas", true);
  if (!entry) return;
  const { canvas: osc, ctx } = entry;
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
  let sampleCount = 0;
  const hasRealPcm = pcmSamples && pcmSamples.length >= 2;
  if (hasRealPcm) {
    const pairCount = pcmSamples.length >> 1;
    const step = Math.max(1, Math.floor(pairCount / 48));
    for (let i = 0; i < pairCount && sampleCount < 48; i += step) {
      oscPlotBuffer[sampleCount] = (pcmSamples[i * 2] + pcmSamples[i * 2 + 1]) * 0.5;
      sampleCount += 1;
    }
    plotSamples = oscPlotBuffer;
  } else {
    plotSamples = synthesizeMultiChannelSamples(freqHz || 0, frameNumber, 48);
    sampleCount = plotSamples.length;
  }

  ctx.strokeStyle = freqHz > 0 || hasRealPcm ? "#7D9B76" : "#3B6EF5";
  ctx.lineWidth = 1.5;
  ctx.beginPath();
  const invSpan = w / Math.max(1, sampleCount - 1);
  const halfH = h * 0.5;
  const ampScale = h * 0.42;
  for (let i = 0; i < sampleCount; i++) {
    const x = i * invSpan;
    let s = plotSamples[i] * 1.4;
    if (s > 1) s = 1;
    else if (s < -1) s = -1;
    const y = halfH - s * ampScale;
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
      "Importez prod.keys / title.keys depuis « Paramètres → Nintendo Switch » (ou glissez le fichier dans l'application desktop).",
      "info",
      6000
    );
    openSettingsTab("settings-pane-switch");
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

function scheduleDebuggerRefresh(nowMs, force = false) {
  if (debuggerRefreshInFlight) return;
  if (!force && (!vramCanvasVisible || state.isFullscreen || nowMs - lastDebuggerRefreshMs < 750)) {
    return;
  }
  debuggerRefreshInFlight = true;
  lastDebuggerRefreshMs = nowMs;
  refreshDebuggerDrawer().finally(() => {
    debuggerRefreshInFlight = false;
  });
}

async function tickEmulationLoop(nowMs) {
  rafLoopScheduled = false;
  if (shouldKeepEmulationLoopAlive()) {
    rafLoopScheduled = true;
    requestAnimationFrame(tickEmulationLoop);
  }
  if (stepInFlight) {
    return;
  }
  stepInFlight = true;
  try {
    const activeGame = getSelectedGame();
    const isSwitch = isSwitchGame(activeGame);
    const activePane = activePaneId || "view-library";
    const keyBindings = getCachedKeyBindings();
    const connectedPad = getConnectedGamepad();
    const rawMask =
      computeBitmaskFromPressedCodes(pressedKeys, keyBindings) | pollGamepadBitmask(connectedPad);

    if (state.bigPictureMode && !document.hidden) {
      await handleBigPictureNavigation(rawMask);
    } else {
      state.prevNavMask = rawMask;
    }

    if (!state.isEmulationRunning || !activeGame || isSwitch) {
      if (isSwitch && state.isEmulationRunning) {
        stopEmulationPlayback();
      } else {
        framePacer.reset();
      }
      return;
    }

    if (document.hidden || activePane !== "view-emulator") {
      framePacer.reset();
      return;
    }

    if (state.debuggerPaused && !state.stepOnceRequested) {
      framePacer.reset();
      return;
    }

    const targetFps = targetFpsForConsole(activeGame.console);
    const manualStep = state.stepOnceRequested;
    const pacing = manualStep
      ? { shouldStep: true, measuredFps: targetFps, frameTimeMs: 1000 / targetFps }
      : framePacer.advance(nowMs, targetFps);

    if (!pacing.shouldStep) {
      return;
    }
    state.stepOnceRequested = false;

    const buttonsP1 = applyPressedTurboWidgets(
      rawMask,
      pressedKeys,
      state.keymapperWidgets,
      state.currentFrame,
      targetFps
    );

    if (state.recordingMacro) {
      state.recordedFrames.push(buttonsP1);
    }

    const wantTelemetry =
      !lastTelemetryDomUpdateMs ||
      nowMs - lastTelemetryDomUpdateMs >= 250 ||
      state.debuggerPaused ||
      manualStep;

    const stepOptions = { includeDebugMeta: wantTelemetry };
    if (pendingNdsStylus) {
      stepOptions.stylusX = pendingNdsStylus.x;
      stepOptions.stylusY = pendingNdsStylus.y;
      stepOptions.stylusPressed = pendingNdsStylus.pressed;
      pendingNdsStylus = null;
    }

    const rawPkt = await invokeBackend(
      "step_viewport_binary",
      {
        buttonsP1,
        buttonsP2: 0,
        turboActive: false,
        speedMult: state.speedMult,
        options: stepOptions
      },
      { silent: true }
    );

    if (rawPkt) {
      const pkt = decodeViewportBinaryPacket(rawPkt);
      const primary = pkt.views[0];
      if (primary) {
        state.currentFrame = pkt.frameNumber || state.currentFrame + 1;
        paintRgbaFrame("canvas-p1", primary, activeGame);
        if (pkt.views[1]) {
          paintRgbaFrame("canvas-p2", pkt.views[1], activeGame);
        }
        if (pkt.views[2]) {
          paintRgbaFrame("canvas-p3", pkt.views[2], activeGame);
        } else if (state.quadLinkMode) {
          paintRgbaFrame("canvas-p3", primary, activeGame);
        }
        if (pkt.views[3]) {
          paintRgbaFrame("canvas-p4", pkt.views[3], activeGame);
        } else if (state.quadLinkMode) {
          paintRgbaFrame("canvas-p4", primary, activeGame);
        }

        // Draw waveform BEFORE transferring pkt.pcmSamples buffer ownership to AudioWorklet
        renderAudioOscilloscope(pkt.audioFreqHz || 0, pkt.frameNumber, pkt.pcmSamples, nowMs);
        apuAudio.pushPcmSamples(pkt.pcmSamples, pkt.sampleRateHz);

        if (pkt.audioFreqHz >= 1200 && connectedPad) {
          triggerGamepadRumble(connectedPad, 55, 0.35, 0.65);
        }

        const meta = pkt.meta || {};
        if (meta.newlyUnlockedAchievement) {
          if (connectedPad) {
            triggerGamepadRumble(connectedPad, 140, 0.6, 0.95);
          }
          showToast(`Succès débloqué : ${meta.newlyUnlockedAchievement}`, "ok");
          const boot = await invokeBackend("get_station_bootstrap", {}, { silent: true });
          if (boot) applyBootstrap(boot);
          else renderInspector();
        }

        if (wantTelemetry) {
          lastTelemetryDomUpdateMs = nowMs;
          const fpsEl = getCachedEl("tel-fps");
          if (fpsEl) {
            const hasGl = Boolean(glRendererCache.get("canvas-p1")?.isSupported());
            const hdTag =
              String(activeGame.console || "").toUpperCase() === "NDS" && state.nds3dScale >= 2
                ? " · 3D 2x"
                : "";
            fpsEl.textContent = `${Math.round(pacing.measuredFps * state.speedMult)} FPS${hasGl ? " · GPU" : ""}${hdTag}`;
          }

          const cpu = meta.cpu || {};
          const pcSpEl = getCachedEl("dbg-pc-sp");
          if (pcSpEl) pcSpEl.textContent = `${cpu.pcHex || "—"} / ${cpu.spHex || "—"}`;
          const afBcEl = getCachedEl("dbg-af-bc");
          if (afBcEl) afBcEl.textContent = `${cpu.regAHex || "—"} / ${cpu.regBcHex || "—"}`;
          const deHlEl = getCachedEl("dbg-de-hl");
          if (deHlEl) deHlEl.textContent = `${cpu.regDeHex || "—"} / ${cpu.regHlHex || "—"}`;
          const sioEl = getCachedEl("dbg-sio");
          if (sioEl) sioEl.textContent = `${cpu.serialSbHex || "0x00"} / ${pkt.audioFreqHz || 0} Hz`;
        }
        if (manualStep || state.debuggerPaused || vramCanvasVisible) {
          scheduleDebuggerRefresh(nowMs, manualStep || state.debuggerPaused);
        }
      }
    }
  } catch (err) {
    console.warn("[zaimule] Erreur boucle viewport:", err);
  } finally {
    stepInFlight = false;
    if (shouldKeepEmulationLoopAlive()) {
      ensureEmulationLoopRunning();
    }
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
    const prevBios = listInstalledBiosMap(state.biosEntries);
    showToast(`Importation de ${paths.length} élément(s)...`, "info", 4000);
    const boot = await invokeBackend("import_rom_paths", { paths });
    if (boot) {
      applyBootstrap(boot);
      const added = boot.lastScanReport?.importedEntries?.length || 0;
      const nextBios = listInstalledBiosMap(boot.biosEntries);
      const changedBios = [];
      for (const [fileName, sig] of nextBios.entries()) {
        if (prevBios.get(fileName) !== sig) {
          changedBios.push(fileName);
        }
      }
      const looksLikeBiosDrop = paths.some((p) => /bios|firmware/i.test(String(p)));
      if (added > 0 && changedBios.length > 0) {
        showToast(
          `${added} jeu(x) + BIOS (${changedBios.join(", ")}) importés.`,
          "ok",
          6500
        );
      } else if (added > 0) {
        showToast(`${added} jeu(x) importé(s) dans la bibliothèque.`, "ok");
      } else if (changedBios.length > 0) {
        showToast(
          `BIOS / Firmware importés dans system/ : ${changedBios.join(", ")}. Relancez un jeu NDS pour activer.`,
          "ok",
          6500
        );
      } else if (looksLikeBiosDrop && nextBios.size > 0) {
        showToast(
          `BIOS / Firmware vérifiés dans system/ : ${Array.from(nextBios.keys()).join(", ")}.`,
          "ok",
          6000
        );
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

  document.querySelectorAll(".settings-nav-btn").forEach((btn) => {
    btn.addEventListener("click", () => switchSettingsSubTab(btn.dataset.settingsTab));
  });

  document.querySelectorAll(".inspector-tab-btn").forEach((btn) => {
    btn.addEventListener("click", () => {
      const targetPane = btn.dataset.inspTab;
      document.querySelectorAll(".inspector-tab-btn").forEach((b) => {
        const isActive = b === btn;
        b.classList.toggle("active", isActive);
        b.setAttribute("aria-selected", String(isActive));
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
  document.getElementById("btn-close-inspector")?.addEventListener("click", () => {
    document.getElementById("inspector-drawer")?.classList.add("collapsed");
  });
  document.getElementById("prompt-options")?.addEventListener("click", toggleInspectorDrawer);

  const toggleEmuDrawer = () => {
    const drawer = document.getElementById("emu-options-drawer");
    drawer?.classList.toggle("collapsed");
  };
  document.getElementById("btn-toggle-emu-drawer")?.addEventListener("click", toggleEmuDrawer);
  document.getElementById("btn-close-emu-drawer")?.addEventListener("click", () => {
    document.getElementById("emu-options-drawer")?.classList.add("collapsed");
  });

  document.getElementById("prompt-launch")?.addEventListener("click", () => {
    launchSelectedGame();
  });

  document.getElementById("prompt-savestate")?.addEventListener("click", () => {
    document.getElementById("btn-quick-save")?.click();
  });

  document.getElementById("prompt-fullscreen")?.addEventListener("click", async () => {
    if (activePaneId !== "view-emulator" && state.isEmulationRunning) {
      switchTab("view-emulator");
    }
    await setViewportFullscreen(!state.isFullscreen);
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
    e.currentTarget.textContent = state.bigPictureMode ? "Activé" : "Désactivé";
    triggerGamepadRumble(getConnectedGamepad(), 70, 0.4, 0.7);
    redrawAllGlViewports();
    ensureEmulationLoopRunning();
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
  const isNdsActive = () => {
    const g = getSelectedGame();
    return Boolean(g && String(g.console || "").toUpperCase() === "NDS");
  };
  const dispatchStylusState = (x, y, pressed) => {
    const payload = { x, y, pressed };
    if (
      state.isEmulationRunning &&
      !state.debuggerPaused &&
      activePaneId === "view-emulator"
    ) {
      pendingNdsStylus = payload;
      return;
    }
    pendingNdsStylus = null;
    invokeBackend("set_nds_stylus", payload, { silent: true });
  };
  const handleStylusEvent = (ev, pressed) => {
    if (!canvasP1 || !isNdsActive()) return false;
    const rect = canvasP1.getBoundingClientRect();
    const normX = Math.max(0, Math.min(1, (ev.clientX - rect.left) / Math.max(1, rect.width)));
    const normY = Math.max(0, Math.min(1, (ev.clientY - rect.top) / Math.max(1, rect.height)));
    const mapped = mapCanvasToNdsStylus(normX, normY, state.ndsLayout);
    if (pressed && !mapped.onBottomScreen) {
      dispatchStylusState(mapped.x, 0, false);
      return false;
    }
    dispatchStylusState(mapped.x, mapped.y, pressed && mapped.onBottomScreen);
    return mapped.onBottomScreen;
  };

  canvasP1?.addEventListener("mousedown", (e) => {
    if (!isNdsActive()) return;
    const rect = canvasP1.getBoundingClientRect();
    const normX = Math.max(0, Math.min(1, (e.clientX - rect.left) / Math.max(1, rect.width)));
    const normY = Math.max(0, Math.min(1, (e.clientY - rect.top) / Math.max(1, rect.height)));
    const mapped = mapCanvasToNdsStylus(normX, normY, state.ndsLayout);
    if (!mapped.onBottomScreen) return;
    state.stylusPressed = true;
    dispatchStylusState(mapped.x, mapped.y, true);
  });
  canvasP1?.addEventListener("mousemove", (e) => {
    if (!state.stylusPressed) return;
    handleStylusEvent(e, true);
  });
  window.addEventListener("mouseup", (e) => {
    if (!state.stylusPressed) return;
    state.stylusPressed = false;
    handleStylusEvent(e, false);
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

  document.getElementById("library-sort-select")?.addEventListener("change", (e) => {
    state.sortBy = e.target.value || "recent";
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

  document.getElementById("btn-export-pack")?.addEventListener("click", async () => {
    const packBytes =
      (await invokeBackend("export_portable_pack_archive")) ||
      Array.from(
        new TextEncoder().encode(
          JSON.stringify({
            magic: "ZAIMULE_PACK_V1",
            version: "1.6.0",
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

  document.getElementById("btn-connect-peer")?.addEventListener("click", async () => {
    const inp = document.getElementById("netplay-peer-input");
    const peerAddr = inp?.value?.trim();
    if (!peerAddr) {
      showToast("Saisissez une adresse LAN IP:PORT (ex: 192.168.1.20:54920).", "info");
      return;
    }
    if (tauriInvoke) {
      const boot = await invokeBackend("connect_netplay_peer", { peerAddr });
      if (boot) {
        applyBootstrap(boot);
        showToast(`Pair UDP LAN connecté : ${boot.udpPeerAddr || peerAddr}`, "ok");
      }
    } else {
      state.udpPeerAddr = peerAddr;
      renderNetplayAndCores();
      showToast(`Pair UDP LAN connecté : ${peerAddr}`, "ok");
    }
  });

  document.getElementById("btn-disconnect-peer")?.addEventListener("click", async () => {
    const inp = document.getElementById("netplay-peer-input");
    if (inp) inp.value = "";
    if (tauriInvoke) {
      const boot = await invokeBackend("connect_netplay_peer", { peerAddr: null });
      if (boot) applyBootstrap(boot);
    } else {
      state.udpPeerAddr = null;
      renderNetplayAndCores();
    }
    showToast("Pair UDP déconnecté.", "info");
  });

  document.getElementById("btn-create-room")?.addEventListener("click", async () => {
    const g = getSelectedGame();
    if (!g) return;
    const peerAddr = document.getElementById("netplay-peer-input")?.value?.trim() || null;
    const boot = await invokeBackend("create_netplay_room", {
      title: `Salon Live · ${g.title}`,
      hostName: "Hôte",
      peerAddr
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
    const peerAddr = document.getElementById("netplay-peer-input")?.value?.trim() || null;
    const boot = await invokeBackend("join_netplay_deep_link", { uri, peerAddr });
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
    e.currentTarget.textContent = on ? "Son" : "Muet";
  });

  for (const sliderId of ["audio-volume-slider", "settings-volume-slider"]) {
    document.getElementById(sliderId)?.addEventListener("input", (e) => {
      setAudioVolumePct(e.target.value, true);
    });
  }

  document.getElementById("btn-record-video")?.addEventListener("click", (e) => {
    if (!getSelectedGame()) return;
    const canvas = document.getElementById("canvas-p1");
    if (!viewportRecorder.isRecording) {
      viewportRecorder.start(canvas, state.activeGameId);
      e.currentTarget.textContent = "Stop .WebM";
      setText("save-toast", "Enregistrement vidéo en cours...");
    } else {
      const res = viewportRecorder.stop();
      e.currentTarget.textContent = "Clip .WebM";
      setText("save-toast", `Clip enregistré (${res.durationText})`);
    }
  });

  document.getElementById("btn-emu-pause")?.addEventListener("click", () => {
    if (!state.isEmulationRunning) return;
    setEmulationPaused(!state.debuggerPaused);
  });

  document.getElementById("btn-emu-reset")?.addEventListener("click", async () => {
    const g = getSelectedGame();
    if (!g || isSwitchGame(g)) {
      showToast("Lancez d'abord un jeu natif (GB/GBC/GBA/NDS) pour redémarrer.", "info");
      return;
    }
    if (tauriInvoke) {
      const boot = await invokeBackend("reset_active_game");
      if (!boot) return;
      applyBootstrap(boot);
    }
    state.currentFrame = 0;
    framePacer.reset();
    apuAudio.reset();
    setEmulationPaused(false);
    showToast("Jeu redémarré (sauvegarde conservée).", "ok");
  });

  document.getElementById("btn-screenshot-png")?.addEventListener("click", () => {
    captureScreenshotPng();
  });

  const applyNdsLayout = (nextLayout) => {
    state.ndsLayout = nextLayout;
    applyDisplayPreferencesToDom();
    invokeBackend(
      "update_ui_preferences",
      {
        shaderCss: null,
        upscalerMode: null,
        speedMultiplier: null,
        ndsLayout: state.ndsLayout
      },
      { silent: true }
    );
  };

  document.getElementById("btn-nds-layout")?.addEventListener("click", () => {
    const curIdx = Math.max(0, NDS_LAYOUT_MODES.indexOf(state.ndsLayout));
    applyNdsLayout(NDS_LAYOUT_MODES[(curIdx + 1) % NDS_LAYOUT_MODES.length]);
  });

  for (const id of ["emu-nds-layout-select", "settings-nds-layout-select"]) {
    document.getElementById(id)?.addEventListener("change", (e) => {
      applyNdsLayout(e.target.value);
    });
  }

  document.getElementById("btn-fullscreen-toggle")?.addEventListener("click", async () => {
    await setViewportFullscreen(!state.isFullscreen);
  });

  document.addEventListener("fullscreenchange", () => {
    if (!document.fullscreenElement && state.isFullscreen) {
      state.isFullscreen = false;
      document.querySelector(".app-shell")?.classList.remove("viewport-fullscreen");
      setText("btn-fullscreen-toggle", "Plein écran");
    }
  });

  const applyUpscalerMode = (mode) => {
    state.upscalerMode = mode;
    applyDisplayPreferencesToDom();
    invokeBackend(
      "update_ui_preferences",
      {
        shaderCss: null,
        upscalerMode: state.upscalerMode,
        speedMultiplier: null,
        ndsLayout: null
      },
      { silent: true }
    );
  };

  for (const id of ["emu-upscaler-select", "settings-upscaler-select"]) {
    document.getElementById(id)?.addEventListener("change", (e) => {
      applyUpscalerMode(e.target.value);
    });
  }

  const applyShaderMode = (shader) => {
    state.activeShaderCss = shader;
    applyDisplayPreferencesToDom();
    invokeBackend(
      "update_ui_preferences",
      {
        shaderCss: state.activeShaderCss,
        upscalerMode: null,
        speedMultiplier: null,
        ndsLayout: null
      },
      { silent: true }
    );
  };

  for (const id of ["emu-shader-select", "settings-shader-select"]) {
    document.getElementById(id)?.addEventListener("change", (e) => {
      applyShaderMode(e.target.value);
    });
  }

  const applyNds3dScale = (rawScale) => {
    state.nds3dScale = Number(rawScale) >= 2 ? 2 : 1;
    applyDisplayPreferencesToDom();
    invokeBackend(
      "update_ui_preferences",
      {
        shaderCss: null,
        upscalerMode: null,
        speedMultiplier: null,
        ndsLayout: null,
        extras: { ndsHdScale: state.nds3dScale }
      },
      { silent: true }
    );
  };

  for (const id of ["emu-nds-3d-scale-select", "settings-nds-3d-scale-select"]) {
    document.getElementById(id)?.addEventListener("change", (e) => {
      applyNds3dScale(e.target.value);
    });
  }

  const applyStretchMode = (nextMode) => {
    const valid = ["native", "wide", "stretch"];
    const mode = valid.includes(nextMode) ? nextMode : "native";
    state.viewportStretchMode = mode;
    try {
      localStorage.setItem("zaimule.viewportStretchMode", mode);
    } catch {
      // ignore storage errors
    }
    applyDisplayPreferencesToDom();
  };

  document.getElementById("btn-stretch-cycle")?.addEventListener("click", () => {
    const order = ["native", "wide", "stretch"];
    const idx = order.indexOf(state.viewportStretchMode);
    applyStretchMode(order[(idx + 1) % order.length]);
  });

  for (const id of ["emu-stretch-select", "settings-stretch-select"]) {
    document.getElementById(id)?.addEventListener("change", (e) => {
      applyStretchMode(e.target.value);
    });
  }

  const toggleIntegerScale = () => {
    state.viewportIntegerScale = !state.viewportIntegerScale;
    applyDisplayPreferencesToDom();
    invokeBackend(
      "update_ui_preferences",
      {
        shaderCss: null,
        upscalerMode: null,
        speedMultiplier: null,
        ndsLayout: null,
        extras: { viewportIntegerScale: state.viewportIntegerScale }
      },
      { silent: true }
    );
  };

  document.getElementById("btn-emu-integer-scale")?.addEventListener("click", toggleIntegerScale);
  document.getElementById("btn-settings-integer-scale")?.addEventListener("click", toggleIntegerScale);

  document.getElementById("btn-rewind-toggle")?.addEventListener("click", () => {
    state.rewindEnabled = state.rewindEnabled === false;
    applyDisplayPreferencesToDom();
    invokeBackend(
      "update_ui_preferences",
      {
        shaderCss: null,
        upscalerMode: null,
        speedMultiplier: null,
        ndsLayout: null,
        extras: { rewindEnabled: state.rewindEnabled }
      },
      { silent: true }
    );
  });

  document.getElementById("btn-reset-optimal")?.addEventListener("click", async () => {
    state.upscalerMode = "XBRZ";
    state.activeShaderCss = "mode-clean";
    state.speedMult = 1;
    state.nds3dScale = 1;
    state.viewportIntegerScale = false;
    state.rewindEnabled = true;
    state.cpuOverclockPct = 100;
    state.pgxpSubpixel = true;
    applyDisplayPreferencesToDom();
    setAudioVolumePct(85, true);
    await invokeBackend(
      "update_ui_preferences",
      {
        shaderCss: "mode-clean",
        upscalerMode: "XBRZ",
        speedMultiplier: 1,
        ndsLayout: null,
        extras: {
          ndsHdScale: 1,
          viewportIntegerScale: false,
          rewindEnabled: true
        }
      },
      { silent: true }
    );
    const boot = await invokeBackend(
      "set_hardware_overclock_profile",
      { cpuOverclockPct: 100, pgxpSubpixel: true },
      { silent: true }
    );
    if (boot) applyBootstrap(boot);
    else renderInspector();
    showToast("Qualité optimale restaurée (xBRZ HD GPU · Cristal HD).", "ok");
  });

  const toggleKm = () => {
    state.keymapperVisible = !state.keymapperVisible;
    document.getElementById("keymapper-p1")?.classList.toggle("visible", state.keymapperVisible);
    setText("btn-toggle-keymapper", state.keymapperVisible ? "Masquer touches" : "Touches écran");
  };
  document.getElementById("btn-toggle-keymapper")?.addEventListener("click", toggleKm);

  document.getElementById("btn-record-macro")?.addEventListener("click", async (e) => {
    if (!state.recordingMacro) {
      state.recordingMacro = true;
      state.recordedFrames = [];
      e.currentTarget.textContent = "Arrêter & Sauver";
      setText("macro-status", "ENREGISTREMENT...");
    } else {
      state.recordingMacro = false;
      e.currentTarget.textContent = "Enregistrer Séquence";
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
      setText("macro-status", "LECTURE TAS");
    }
  });

  const syncLinkPill = () => {
    const pill = document.getElementById("link-status-pill");
    if (!pill) return;
    if (state.quadLinkMode) {
      pill.style.display = "inline-flex";
      pill.textContent = "Link 4P";
    } else if (state.dualLinkMode) {
      pill.style.display = "inline-flex";
      pill.textContent = "Link 2P";
    } else {
      pill.style.display = "none";
    }
  };

  document.getElementById("btn-toggle-dual")?.addEventListener("click", async (e) => {
    state.dualLinkMode = !state.dualLinkMode;
    if (!state.dualLinkMode) {
      state.quadLinkMode = false;
      const p3 = document.getElementById("bezel-p3");
      const p4 = document.getElementById("bezel-p4");
      if (p3) p3.style.display = "none";
      if (p4) p4.style.display = "none";
      document.getElementById("screens-wrap")?.classList.remove("quad-mode");
      setText("btn-toggle-quad", "Link 4P : OFF");
    }
    const p2 = document.getElementById("bezel-p2");
    if (p2) p2.style.display = state.dualLinkMode ? "flex" : "none";
    document.getElementById("screens-wrap")?.classList.toggle("dual-mode", state.dualLinkMode && !state.quadLinkMode);
    e.currentTarget.textContent = `Link 2P : ${state.dualLinkMode ? "ON" : "OFF"}`;
    syncLinkPill();
    await invokeBackend("set_dual_link_enabled", { enabled: state.dualLinkMode });
  });

  document.getElementById("btn-toggle-quad")?.addEventListener("click", async (e) => {
    state.quadLinkMode = !state.quadLinkMode;
    if (state.quadLinkMode) {
      state.dualLinkMode = true;
    }
    const wrap = document.getElementById("screens-wrap");
    wrap?.classList.remove("dual-mode");
    wrap?.classList.toggle("quad-mode", state.quadLinkMode);
    const p2 = document.getElementById("bezel-p2");
    const p3 = document.getElementById("bezel-p3");
    const p4 = document.getElementById("bezel-p4");
    if (p2) p2.style.display = state.quadLinkMode || state.dualLinkMode ? "flex" : "none";
    if (p3) p3.style.display = state.quadLinkMode ? "flex" : "none";
    if (p4) p4.style.display = state.quadLinkMode ? "flex" : "none";
    e.currentTarget.textContent = `Link 4P : ${state.quadLinkMode ? "ON" : "OFF"}`;
    setText("btn-toggle-dual", `Link 2P : ${state.dualLinkMode ? "ON" : "OFF"}`);
    syncLinkPill();
    await invokeBackend("set_quad_link_enabled", { enabled: state.quadLinkMode });
  });

  document.getElementById("btn-dbg-pause")?.addEventListener("click", () => {
    setEmulationPaused(!state.debuggerPaused);
  });

  document.getElementById("btn-dbg-step")?.addEventListener("click", () => {
    setEmulationPaused(true);
    state.stepOnceRequested = true;
    ensureEmulationLoopRunning();
  });

  document.getElementById("btn-speed-cycle")?.addEventListener("click", () => {
    state.speedMult =
      state.speedMult === 1 ? 2 : state.speedMult === 2 ? 4 : state.speedMult === 4 ? 8 : 1;
    applyDisplayPreferencesToDom();
    lastTelemetryDomUpdateMs = 0;
    invokeBackend(
      "update_ui_preferences",
      {
        shaderCss: null,
        upscalerMode: null,
        speedMultiplier: state.speedMult,
        ndsLayout: null
      },
      { silent: true }
    );
  });

  document.getElementById("btn-quick-save")?.addEventListener("click", async () => {
    if (!getSelectedGame()) return;
    const info = await invokeBackend("create_save_state", { slot: 1, label: "Sauvegarde rapide" });
    setText("save-toast", info ? "Sauvegardé (Slot #1)" : "Sauvegardé (Slot #1)");
  });

  document.getElementById("btn-quick-load")?.addEventListener("click", async () => {
    if (!getSelectedGame()) return;
    await invokeBackend("restore_save_state", { slot: 1 });
    setText("save-toast", "Chargé (Slot #1)");
  });

  document.getElementById("btn-rewind-step")?.addEventListener("click", async () => {
    if (!getSelectedGame()) return;
    await invokeBackend("rewind_frames", { stepsBack: 2 });
  });

  const EMULATOR_PREVENT_DEFAULT_CODES = new Set([
    "Space",
    "ArrowUp",
    "ArrowDown",
    "ArrowLeft",
    "ArrowRight",
    "Enter",
    "Backspace"
  ]);

  window.addEventListener("keydown", async (e) => {
    const activeTag = document.activeElement?.tagName;
    if (activeTag === "INPUT" || activeTag === "TEXTAREA" || activeTag === "SELECT") {
      if (e.key === "Escape") document.activeElement.blur();
      return;
    }
    if (e.ctrlKey || e.metaKey || e.altKey) {
      return;
    }

    const activePane = document.querySelector(".view-pane.active")?.id || "view-library";
    if (e.code === "Escape" && state.isFullscreen) {
      e.preventDefault();
      await setViewportFullscreen(false);
      return;
    }
    if ((e.code === "F9" || e.code === "Pause") && activePane === "view-emulator" && state.isEmulationRunning) {
      e.preventDefault();
      setEmulationPaused(!state.debuggerPaused);
      return;
    }
    if (e.code === "F11" && activePane === "view-emulator") {
      e.preventDefault();
      await setViewportFullscreen(!state.isFullscreen);
      return;
    }
    if (e.code === "F12" && activePane === "view-emulator") {
      e.preventDefault();
      captureScreenshotPng();
      return;
    }

    if (activePane === "view-emulator" && EMULATOR_PREVENT_DEFAULT_CODES.has(e.code)) {
      e.preventDefault();
    }

    if (state.isEmulationRunning && apuAudio.enabled && activePane === "view-emulator") {
      apuAudio.ensureContext();
    }
    pressedKeys.add(e.code);

    if (activePane !== "view-emulator") {
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
      if (
        e.code === "ArrowRight" ||
        e.code === "ArrowLeft" ||
        e.code === "ArrowDown" ||
        e.code === "ArrowUp"
      ) {
        const visible = getVisibleCatalog();
        if (visible.length === 0) return;
        e.preventDefault();
        const cols = getLibraryGridColumnCount();
        const step =
          e.code === "ArrowRight"
            ? 1
            : e.code === "ArrowLeft"
              ? -1
              : e.code === "ArrowDown"
                ? cols
                : -cols;
        const idx = visible.findIndex((g) => g.id === state.activeGameId);
        const base = idx >= 0 ? idx : 0;
        const nextIdx =
          e.code === "ArrowRight" || e.code === "ArrowLeft"
            ? (base + step + visible.length) % visible.length
            : Math.max(0, Math.min(visible.length - 1, base + step));
        const nextGame = visible[nextIdx];
        if (nextGame && (idx < 0 || nextGame.id !== state.activeGameId)) {
          state.activeGameId = nextGame.id;
          const boot = await invokeBackend("select_game", { gameId: nextGame.id });
          if (boot) applyBootstrap(boot);
          else updateAllViews();
          scrollSelectedCardIntoView();
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
    } else if (e.code === "Backspace" && activePane === "view-emulator") {
      e.preventDefault();
      document.getElementById("btn-rewind-step")?.click();
    }
  });

  window.addEventListener("keyup", (e) => {
    pressedKeys.delete(e.code);
  });

  window.addEventListener("blur", () => {
    releaseTransientInputs();
  });

  document.addEventListener("visibilitychange", () => {
    if (document.hidden) {
      releaseTransientInputs();
      framePacer.reset();
      apuAudio.reset();
    } else {
      framePacer.reset();
      lastTelemetryDomUpdateMs = 0;
      ensureEmulationLoopRunning();
    }
  });

  document.getElementById("win-min")?.addEventListener("click", () => appWindow?.minimize());
  document.getElementById("win-max")?.addEventListener("click", () => appWindow?.toggleMaximize());
  document.getElementById("win-close")?.addEventListener("click", () => appWindow?.close());
}

async function init() {
  activePaneId = document.querySelector(".view-pane.active")?.id || "view-library";
  if (typeof IntersectionObserver === "function") {
    const vramEl = document.getElementById("vram-atlas-canvas");
    const oscEl = document.getElementById("audio-oscilloscope-canvas");
    const visObserver = new IntersectionObserver(
      (entries) => {
        for (const entry of entries) {
          if (entry.target === vramEl) {
            const wasVisible = vramCanvasVisible;
            vramCanvasVisible = entry.isIntersecting;
            if (vramCanvasVisible && !wasVisible && state.isEmulationRunning) {
              scheduleDebuggerRefresh(performance.now(), true);
            }
          } else if (entry.target === oscEl) {
            oscCanvasVisible = entry.isIntersecting;
          }
        }
      },
      { threshold: 0.05 }
    );
    if (vramEl) visObserver.observe(vramEl);
    if (oscEl) visObserver.observe(oscEl);
  }
  if (typeof ResizeObserver === "function") {
    const resizeObs = new ResizeObserver(() => {
      if (activePaneId === "view-emulator") {
        redrawAllGlViewports();
      }
    });
    for (const id of ["screen-1", "screen-2", "screen-3", "screen-4"]) {
      const el = document.getElementById(id);
      if (el) resizeObs.observe(el);
    }
  }
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

  setAudioVolumePct(state.audioVolume, false);
  applyDisplayPreferencesToDom();
  renderStandbyCanvas();
  updateAllViews();
  const boot = await invokeBackend("get_station_bootstrap");
  if (boot) {
    applyBootstrap(boot);
  }
  ensureEmulationLoopRunning();
}

init();
