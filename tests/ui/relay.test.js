import test from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const rootDir = path.resolve(__dirname, "../..");

test("Multijoueur E2EE UI elements, security attributes, and Tauri commands stay wired 1:1", () => {
  const html = fs.readFileSync(path.join(rootDir, "ui/index.html"), "utf8");
  const appJs = fs.readFileSync(path.join(rootDir, "ui/app.js"), "utf8");
  const libRs = fs.readFileSync(path.join(rootDir, "src-tauri/src/lib.rs"), "utf8");

  // Onglet Paramètres -> Multijoueur E2EE présent
  assert.match(html, /data-settings-tab="settings-pane-multiplayer"/);
  assert.match(html, /id="settings-pane-multiplayer"/);

  // La clé d'accès privée du relais doit rester masquée (type="password")
  assert.match(
    html,
    /<input[^>]*type="password"[^>]*id="relay-key-input"|<input[^>]*id="relay-key-input"[^>]*type="password"/
  );

  // Chaque ID relay référencé dans app.js existe dans index.html
  const relayIds = [
    "relay-status-pill",
    "relay-header-badge",
    "relay-url-input",
    "relay-key-status",
    "relay-key-input",
    "btn-relay-clear-key",
    "relay-player-input",
    "btn-relay-save-config",
    "relay-disconnected-panel",
    "relay-selected-game-hint",
    "relay-create-password",
    "relay-max-players",
    "btn-relay-create-room",
    "relay-join-invite-input",
    "relay-join-password",
    "btn-relay-join-room",
    "relay-connected-panel",
    "relay-active-code",
    "relay-cipher-badge",
    "relay-rtt-badge",
    "relay-packets-badge",
    "relay-active-game",
    "btn-relay-copy-invite",
    "btn-relay-launch-game",
    "btn-relay-leave-room",
    "relay-invite-uri-readout",
    "relay-peers-count",
    "relay-peers-list",
    "relay-chat-list",
    "relay-chat-input",
    "btn-relay-send-chat",
    "relay-switch-tunnel-box",
    "relay-switch-tunnel-badge",
    "relay-switch-tunnel-packets",
    "relay-switch-host-port-wrap",
    "relay-switch-host-port",
    "relay-switch-tunnel-endpoint",
    "btn-relay-copy-switch-endpoint",
    "btn-relay-toggle-switch-tunnel",
    "relay-switch-tunnel-hint"
  ];

  for (const id of relayIds) {
    assert.ok(html.includes(`id="${id}"`), `index.html missing id="${id}"`);
    assert.ok(appJs.includes(`"${id}"`), `app.js missing reference to "${id}"`);
  }

  // Chaque commande Tauri relais appelée par app.js est bien exposée dans lib.rs
  const commands = [
    "configure_relay_server",
    "relay_create_room",
    "relay_join_room",
    "relay_leave_room",
    "relay_send_chat",
    "get_relay_status",
    "relay_start_switch_tunnel",
    "relay_stop_switch_tunnel",
    "get_switch_tunnel_status"
  ];
  for (const cmd of commands) {
    assert.ok(appJs.includes(`"${cmd}"`), `app.js missing invokeBackend("${cmd}")`);
    assert.ok(libRs.includes(`fn ${cmd}(`), `lib.rs missing #[tauri::command] fn ${cmd}`);
  }
});
