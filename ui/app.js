import {
  filterCatalogEntries,
  computeConsoleCounts,
  formatPlaytime,
  formatFileSize
} from "./library.js";
import { computeBitmaskFromPressedCodes } from "./keymapper.js";

const tauriInvoke = window.__TAURI__?.core?.invoke;
const appWindow = window.__TAURI__?.window?.getCurrentWindow?.();

let state = {
  activeGameId: "gb-pocket-arena",
  catalog: [],
  modLayers: [],
  saveSlots: [],
  keymapperWidgets: [],
  macros: [],
  netplayRooms: [],
  libretroCores: [],
  biosEntries: [],
  linkTelemetry: { enabled: false, packetsTransferred: 0 },
  consoleFilter: "ALL",
  searchQuery: "",
  keymapperVisible: false,
  dualLinkMode: false,
  speedMult: 1,
  activeMacroId: null
};

const pressedKeys = new Set();

async function invokeBackend(cmd, args = {}) {
  if (tauriInvoke) {
    return await tauriInvoke(cmd, args);
  }
  return null;
}

function getSelectedGame() {
  return (
    state.catalog.find((g) => g.id === state.activeGameId) ||
    state.catalog[0] || {
      id: "gb-pocket-arena",
      title: "Pocket Monster Link Arena (DMG/GBC)",
      console: "GBC",
      engineKind: "nativeRust",
      coreId: "zaimule-rust-gb",
      region: "EUR · FR",
      crc32Hex: "DCE0682B",
      sramSummary: "32 Ko SRAM Atomique + Câble Link SIO",
      playtimeSeconds: 15420,
      coverAccent: "#1e3a8a",
      coverSymbol: "🐉"
    }
  );
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
        const boot = await invokeBackend("toggle_mod_layer", {
          modId: m.id,
          enabled: e.target.checked
        });
        if (boot) applyBootstrap(boot);
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
        <div class="slot-time mono">${slot.occupied ? slot.label : "Vide"}</div>
      `;
      card.addEventListener("click", async () => {
        if (slot.occupied) {
          await invokeBackend("restore_save_state", { slot: slot.slot });
          document.getElementById("save-toast").textContent = `✓ Slot #${slot.slot} chargé (${slot.crc32Hex})`;
        } else {
          const info = await invokeBackend("create_save_state", {
            slot: slot.slot,
            label: `Frame Snapshot`
          });
          if (info) {
            const boot = await invokeBackend("get_station_bootstrap");
            if (boot) applyBootstrap(boot);
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
      badge.textContent = w.label;
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
  state.catalog = boot.catalog || [];
  state.modLayers = boot.modLayers || [];
  state.saveSlots = boot.saveSlots || [];
  state.keymapperWidgets = boot.keymapperWidgets || [];
  state.macros = boot.macros || [];
  state.netplayRooms = boot.netplayRooms || [];
  state.libretroCores = boot.libretroCores || [];
  state.biosEntries = boot.biosEntries || [];
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

async function tickEmulationLoop() {
  const buttonsP1 = computeBitmaskFromPressedCodes(pressedKeys);
  const resp = await invokeBackend("step_viewport", {
    buttonsP1,
    buttonsP2: 0,
    turboActive: true
  });

  if (resp && resp.primary) {
    paintRgbaFrame("canvas-p1", resp.primary);
    if (resp.secondary) {
      paintRgbaFrame("canvas-p2", resp.secondary);
    }
    const cpu = resp.primary.cpu;
    document.getElementById("p1-reg-readout").textContent = `${cpu.arch} · PC:${cpu.pcHex}`;
    document.getElementById("rewind-readout").textContent = `Frame #${resp.primary.frameNumber}`;
    document.getElementById("dbg-pc-sp").textContent = `${cpu.pcHex} / ${cpu.spHex}`;
    document.getElementById("dbg-af-bc").textContent = `${cpu.regAHex} / ${cpu.regBcHex}`;
    document.getElementById("dbg-de-hl").textContent = `${cpu.regDeHex} / ${cpu.regHlHex}`;
    document.getElementById("dbg-sio").textContent = `${cpu.serialSbHex} / ${cpu.serialScHex}`;
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

  document.getElementById("btn-launch-selected")?.addEventListener("click", () => {
    switchTab("view-emulator");
  });
  document.getElementById("btn-insp-play")?.addEventListener("click", () => {
    switchTab("view-emulator");
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
      btn.textContent = `🎯 Keymapper BlueStacks : ${state.keymapperVisible ? "ON" : "OFF"}`;
    }
  };
  document.getElementById("btn-toggle-keymapper")?.addEventListener("click", toggleKm);
  document.getElementById("btn-dock-keymapper")?.addEventListener("click", toggleKm);

  document.getElementById("btn-toggle-dual")?.addEventListener("click", async (e) => {
    state.dualLinkMode = !state.dualLinkMode;
    document.getElementById("bezel-p2").style.display = state.dualLinkMode ? "flex" : "none";
    document.getElementById("screens-wrap").classList.toggle("dual-mode", state.dualLinkMode);
    e.currentTarget.textContent = `🔗 Multi-Instance 2P Link : ${state.dualLinkMode ? "ACTIF" : "OFF"}`;
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
    if (info) {
      document.getElementById("save-toast").textContent =
        `✓ Sauvé Slot #1 (Frame #${info.frame} · CRC ${info.crc32Hex})`;
    }
  });

  document.getElementById("btn-quick-load")?.addEventListener("click", async () => {
    const info = await invokeBackend("restore_save_state", { slot: 1 });
    if (info) {
      document.getElementById("save-toast").textContent =
        `✓ Chargé Slot #1 (Frame #${info.frame})`;
    }
  });

  document.getElementById("btn-rewind-step")?.addEventListener("click", async () => {
    await invokeBackend("rewind_frames", { stepsBack: 2 });
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
      await invokeBackend("rewind_frames", { stepsBack: 1 });
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
  const boot = await invokeBackend("get_station_bootstrap");
  if (boot) {
    applyBootstrap(boot);
  }
  requestAnimationFrame(tickEmulationLoop);
}

init();
