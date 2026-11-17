/**
 * Pure ES module for BlueStacks-style Visual Keymapper Overlay,
 * adjustable Turbo/Autofire duty-cycle, and keyboard-to-joypad bitmask mapping.
 */

export const BTN_UP = 1 << 0;
export const BTN_DOWN = 1 << 1;
export const BTN_LEFT = 1 << 2;
export const BTN_RIGHT = 1 << 3;
export const BTN_A = 1 << 4;
export const BTN_B = 1 << 5;
export const BTN_START = 1 << 6;
export const BTN_SELECT = 1 << 7;
export const BTN_L = 1 << 8;
export const BTN_R = 1 << 9;

export const DEFAULT_KEY_BINDINGS = {
  ArrowUp: BTN_UP,
  KeyW: BTN_UP,
  KeyZ: BTN_UP,
  ArrowDown: BTN_DOWN,
  KeyS: BTN_DOWN,
  ArrowLeft: BTN_LEFT,
  KeyA: BTN_LEFT,
  KeyQ: BTN_LEFT,
  ArrowRight: BTN_RIGHT,
  KeyD: BTN_RIGHT,
  KeyJ: BTN_A,
  Space: BTN_A,
  KeyK: BTN_B,
  ShiftLeft: BTN_B,
  Enter: BTN_START,
  Backspace: BTN_SELECT
};

export function computeBitmaskFromPressedCodes(pressedSet, bindings = DEFAULT_KEY_BINDINGS) {
  let mask = 0;
  if (!pressedSet) return mask;
  for (const code of pressedSet) {
    if (bindings[code]) {
      mask |= bindings[code];
    }
  }
  return mask;
}

export function shouldFireTurboOnFrame(frame, turboHz = 20, fps = 60) {
  const hz = Math.max(1, Math.min(30, Number(turboHz) || 20));
  const rate = Math.max(30, Number(fps) || 60);
  const period = Math.max(2, Math.floor(rate / hz));
  const half = Math.max(1, Math.floor(period / 2));
  return (Math.max(0, Number(frame) || 0) % period) < half;
}

export function clampWidgetPosition(xPct, yPct) {
  const x = Math.max(4, Math.min(92, Number(xPct) || 50));
  const y = Math.max(6, Math.min(90, Number(yPct) || 50));
  return { xPct: Math.round(x * 10) / 10, yPct: Math.round(y * 10) / 10 };
}

export function parseDeepLinkUri(uri) {
  if (!uri || typeof uri !== "string") return null;
  const trimmed = uri.trim();
  if (!trimmed.startsWith("zaimule://room/")) return null;
  const withoutScheme = trimmed.slice("zaimule://room/".length);
  const [roomPart, queryPart = ""] = withoutScheme.split("?");
  const roomCode = roomPart.trim().toUpperCase();
  if (!roomCode) return null;

  const params = new URLSearchParams(queryPart);
  return {
    roomCode,
    crc: (params.get("crc") || "DCE0682B").toUpperCase(),
    core: params.get("core") || "zaimule-rust-gb"
  };
}
