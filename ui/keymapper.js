/**
 * Pure ES module for BlueStacks-style Visual Keymapper Overlay,
 * adjustable Turbo/Autofire duty-cycle, keyboard-to-joypad bitmask mapping,
 * and W3C Gamepad API standard controller polling.
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
export const BTN_X = 1 << 10;
export const BTN_Y = 1 << 11;

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
  KeyI: BTN_X,
  KeyO: BTN_Y,
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

export function computeBitmaskFromGamepad(gamepad, deadzone = 0.35) {
  if (!gamepad || !gamepad.buttons) return 0;
  let mask = 0;
  const pressed = (idx) => Boolean(gamepad.buttons[idx]?.pressed);

  if (pressed(0) || pressed(2)) mask |= BTN_A;
  if (pressed(1) || pressed(3)) mask |= BTN_B;
  if (pressed(4) || pressed(6)) mask |= BTN_L;
  if (pressed(5) || pressed(7)) mask |= BTN_R;
  if (pressed(8)) mask |= BTN_SELECT;
  if (pressed(9)) mask |= BTN_START;
  if (pressed(12)) mask |= BTN_UP;
  if (pressed(13)) mask |= BTN_DOWN;
  if (pressed(14)) mask |= BTN_LEFT;
  if (pressed(15)) mask |= BTN_RIGHT;

  const axes = gamepad.axes || [];
  if (Number(axes[0]) < -deadzone) mask |= BTN_LEFT;
  if (Number(axes[0]) > deadzone) mask |= BTN_RIGHT;
  if (Number(axes[1]) < -deadzone) mask |= BTN_UP;
  if (Number(axes[1]) > deadzone) mask |= BTN_DOWN;

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

/**
 * Triggers dual-rumble haptic feedback on a W3C Gamepad if supported by the controller.
 */
export function triggerGamepadRumble(
  gamepad,
  durationMs = 60,
  weakMagnitude = 0.35,
  strongMagnitude = 0.65
) {
  const actuator = gamepad?.vibrationActuator;
  if (!actuator || typeof actuator.playEffect !== "function") {
    return false;
  }
  try {
    actuator.playEffect("dual-rumble", {
      startDelay: 0,
      duration: Math.max(10, Math.min(1000, Number(durationMs) || 60)),
      weakMagnitude: Math.max(0, Math.min(1, Number(weakMagnitude) || 0.35)),
      strongMagnitude: Math.max(0, Math.min(1, Number(strongMagnitude) || 0.65))
    });
    return true;
  } catch {
    return false;
  }
}

/**
 * Computes rising-edge Big Picture UI navigation actions from previous and current joypad bitmasks.
 */
export function computeBigPictureNavAction(prevMask, nextMask) {
  const rising = ~prevMask & nextMask;
  if (rising & BTN_LEFT) return "PREV_GAME";
  if (rising & BTN_RIGHT) return "NEXT_GAME";
  if (rising & BTN_UP) return "PREV_ROW";
  if (rising & BTN_DOWN) return "NEXT_ROW";
  if (rising & BTN_L) return "PREV_TAB";
  if (rising & BTN_R) return "NEXT_TAB";
  if (rising & (BTN_A | BTN_START)) return "LAUNCH_GAME";
  if (rising & BTN_B) return "BACK_LIBRARY";
  return null;
}
