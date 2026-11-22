import test from "node:test";
import assert from "node:assert/strict";
import {
  BTN_A,
  BTN_B,
  BTN_L,
  BTN_LEFT,
  BTN_R,
  BTN_RIGHT,
  BTN_SELECT,
  BTN_START,
  BTN_UP,
  BTN_X,
  BTN_Y,
  computeBitmaskFromPressedCodes,
  buildKeyBindingsFromWidgets,
  applyPressedTurboWidgets,
  mapCanvasToNdsStylus,
  computeBitmaskFromGamepad,
  shouldFireTurboOnFrame,
  clampWidgetPosition,
  parseDeepLinkUri,
  triggerGamepadRumble,
  computeBigPictureNavAction
} from "../../ui/keymapper.js";
import { sanitizeApuFrequency, synthesizeMultiChannelSamples } from "../../ui/audio.js";

test("computeBitmaskFromPressedCodes maps keyboard codes including L/R and separates Backspace from Select", () => {
  const pressed = new Set(["ArrowRight", "KeyJ", "KeyK", "KeyE", "KeyR"]);
  const mask = computeBitmaskFromPressedCodes(pressed);
  assert.equal(mask, BTN_RIGHT | BTN_A | BTN_B | BTN_L | BTN_R);

  const zqsd = new Set(["KeyZ", "KeyL", "KeyP"]);
  assert.equal(computeBitmaskFromPressedCodes(zqsd), BTN_UP | BTN_L | BTN_R);

  // Backspace is reserved for Rewind and must not assert BTN_SELECT
  assert.equal(computeBitmaskFromPressedCodes(new Set(["Backspace"])), 0);
  assert.equal(computeBitmaskFromPressedCodes(new Set(["ShiftRight"])), BTN_SELECT);
  assert.equal(computeBitmaskFromPressedCodes(new Set(["KeyN"])), BTN_SELECT);
});

test("computeBitmaskFromGamepad maps A/B/X/Y distinctly, analog triggers, and stick axes", () => {
  const fakePad = {
    buttons: Array.from({ length: 16 }, (_, idx) => ({
      pressed: idx === 0 || idx === 2 || idx === 3 || idx === 9,
      value: idx === 6 ? 0.6 : idx === 7 ? 0.1 : 0
    })),
    axes: [-0.8, 0.0]
  };
  const mask = computeBitmaskFromGamepad(fakePad);
  assert.equal(mask, BTN_A | BTN_X | BTN_Y | BTN_L | BTN_START | BTN_LEFT);
  assert.equal(mask & BTN_B, 0);
  assert.equal(mask & BTN_R, 0);
});

test("applyPressedTurboWidgets never fires unpressed buttons and pulses only when turbo key is held", () => {
  const widgets = [
    { id: "km-dpad", kind: "dpad", keyBinding: "ZQSD", buttonMask: BTN_UP | BTN_LEFT, turboHz: null },
    { id: "km-btn-a", kind: "actionButton", keyBinding: "KeyJ", buttonMask: BTN_A, turboHz: null },
    { id: "km-turbo-a", kind: "turboButton", keyBinding: "KeyU", buttonMask: BTN_A, turboHz: 20 }
  ];

  // No keys held -> turbo MUST NOT fire on any frame
  const empty = new Set();
  for (let frame = 0; frame < 10; frame++) {
    assert.equal(applyPressedTurboWidgets(0, empty, widgets, frame, 60), 0);
  }

  // Holding regular A (KeyJ) without holding KeyU -> steady BTN_A on every frame
  const regularA = new Set(["KeyJ"]);
  const bindings = buildKeyBindingsFromWidgets(widgets);
  const rawRegular = computeBitmaskFromPressedCodes(regularA, bindings);
  assert.equal(rawRegular, BTN_A);
  for (let frame = 0; frame < 10; frame++) {
    assert.equal(applyPressedTurboWidgets(rawRegular, regularA, widgets, frame, 60), BTN_A);
  }

  // Holding turbo key KeyU -> pulses BTN_A on 20Hz duty cycle (period=3, half=1 => frames 0, 3, 6...)
  const turboHeld = new Set(["KeyU"]);
  const rawTurbo = computeBitmaskFromPressedCodes(turboHeld, bindings);
  assert.equal(rawTurbo, 0); // turbo key not mapped as steady hold
  assert.equal(applyPressedTurboWidgets(rawTurbo, turboHeld, widgets, 0, 60), BTN_A);
  assert.equal(applyPressedTurboWidgets(rawTurbo, turboHeld, widgets, 1, 60), 0);
  assert.equal(applyPressedTurboWidgets(rawTurbo, turboHeld, widgets, 2, 60), 0);
  assert.equal(applyPressedTurboWidgets(rawTurbo, turboHeld, widgets, 3, 60), BTN_A);
});

test("buildKeyBindingsFromWidgets merges custom single-key widgets without breaking D-Pad", () => {
  const widgets = [
    { id: "km-dpad", kind: "dpad", keyBinding: "ZQSD", buttonMask: BTN_UP, turboHz: null },
    { id: "km-custom", kind: "actionButton", keyBinding: "KeyH", buttonMask: BTN_X, turboHz: null }
  ];
  const bindings = buildKeyBindingsFromWidgets(widgets);
  assert.equal(bindings.KeyH, BTN_X);
  assert.equal(bindings.ZQSD, undefined);
});

test("mapCanvasToNdsStylus rejects top screen and maps touchscreen accurately across all 4 NDS layouts", () => {
  // Default / vertical (top = 0..0.5 Y, touch = 0.5..1.0 Y)
  assert.deepEqual(mapCanvasToNdsStylus(0.5, 0.25), { onBottomScreen: false, x: 0, y: 0 });
  assert.deepEqual(mapCanvasToNdsStylus(0.5, 0.499, "vertical"), { onBottomScreen: false, x: 0, y: 0 });
  assert.deepEqual(mapCanvasToNdsStylus(0.0, 0.5, "vertical"), { onBottomScreen: true, x: 0, y: 0 });
  assert.deepEqual(mapCanvasToNdsStylus(0.5, 0.75, "vertical"), { onBottomScreen: true, x: 128, y: 96 });
  assert.deepEqual(mapCanvasToNdsStylus(1.0, 1.0, "vertical"), { onBottomScreen: true, x: 255, y: 191 });

  // Vertical-swapped (touch = 0..0.5 Y, top = 0.5..1.0 Y)
  assert.deepEqual(mapCanvasToNdsStylus(0.5, 0.5, "vertical-swapped"), { onBottomScreen: false, x: 0, y: 0 });
  assert.deepEqual(mapCanvasToNdsStylus(0.5, 0.75, "vertical-swapped"), { onBottomScreen: false, x: 0, y: 0 });
  assert.deepEqual(mapCanvasToNdsStylus(0.5, 0.25, "vertical-swapped"), { onBottomScreen: true, x: 128, y: 96 });
  assert.deepEqual(mapCanvasToNdsStylus(0.0, 0.0, "vertical-swapped"), { onBottomScreen: true, x: 0, y: 0 });

  // Horizontal (top = 0..0.5 X, touch = 0.5..1.0 X)
  assert.deepEqual(mapCanvasToNdsStylus(0.25, 0.5, "horizontal"), { onBottomScreen: false, x: 0, y: 0 });
  assert.deepEqual(mapCanvasToNdsStylus(0.5, 0.0, "horizontal"), { onBottomScreen: true, x: 0, y: 0 });
  assert.deepEqual(mapCanvasToNdsStylus(0.75, 0.5, "horizontal"), { onBottomScreen: true, x: 128, y: 96 });
  assert.deepEqual(mapCanvasToNdsStylus(1.0, 1.0, "horizontal"), { onBottomScreen: true, x: 255, y: 191 });

  // Horizontal-swapped (touch = 0..0.5 X, top = 0.5..1.0 X)
  assert.deepEqual(mapCanvasToNdsStylus(0.5, 0.5, "horizontal-swapped"), { onBottomScreen: false, x: 0, y: 0 });
  assert.deepEqual(mapCanvasToNdsStylus(0.75, 0.5, "horizontal-swapped"), { onBottomScreen: false, x: 0, y: 0 });
  assert.deepEqual(mapCanvasToNdsStylus(0.25, 0.5, "horizontal-swapped"), { onBottomScreen: true, x: 128, y: 96 });

  // Hybrid (main = 0..2/3 X, touch = 2/3..1.0 X & 0.25..0.75 Y)
  assert.deepEqual(mapCanvasToNdsStylus(0.4, 0.5, "hybrid"), { onBottomScreen: false, x: 0, y: 0 });
  assert.deepEqual(mapCanvasToNdsStylus(0.8, 0.1, "hybrid"), { onBottomScreen: false, x: 0, y: 0 });
  assert.deepEqual(mapCanvasToNdsStylus(5 / 6, 0.5, "hybrid"), { onBottomScreen: true, x: 128, y: 96 });
});

test("sanitizeApuFrequency validates audible chiptune frequencies", () => {
  assert.equal(sanitizeApuFrequency(880), 880);
  assert.equal(sanitizeApuFrequency(15), 0);
  assert.equal(sanitizeApuFrequency(99999), 0);
});

test("shouldFireTurboOnFrame alternates on 50% duty cycle", () => {
  assert.equal(shouldFireTurboOnFrame(0, 15, 60), true);
  assert.equal(shouldFireTurboOnFrame(1, 15, 60), true);
  assert.equal(shouldFireTurboOnFrame(2, 15, 60), false);
  assert.equal(shouldFireTurboOnFrame(3, 15, 60), false);
  assert.equal(shouldFireTurboOnFrame(4, 15, 60), true);
});

test("clampWidgetPosition keeps overlay widgets inside viewport bounds", () => {
  assert.deepEqual(clampWidgetPosition(-10, 120), { xPct: 4, yPct: 90 });
  assert.deepEqual(clampWidgetPosition(45.24, 62.89), { xPct: 45.2, yPct: 62.9 });
});

test("parseDeepLinkUri parses zaimule://room/ invitations", () => {
  const parsed = parseDeepLinkUri("zaimule://room/pkmn-link-01?crc=dce0682b&core=zaimule-rust-gb");
  assert.deepEqual(parsed, {
    roomCode: "PKMN-LINK-01",
    crc: "DCE0682B",
    core: "zaimule-rust-gb"
  });
  assert.equal(parseDeepLinkUri("https://example.com"), null);
});

test("triggerGamepadRumble invokes W3C dual-rumble actuator safely", () => {
  let lastEffect = null;
  const fakePad = {
    vibrationActuator: {
      playEffect(type, params) {
        lastEffect = { type, params };
      }
    }
  };
  assert.equal(triggerGamepadRumble(fakePad, 90, 0.4, 0.8), true);
  assert.equal(lastEffect.type, "dual-rumble");
  assert.equal(lastEffect.params.duration, 90);
  assert.equal(triggerGamepadRumble(null), false);
});

test("computeBigPictureNavAction detects rising edges on D-Pad, shoulders, and A", () => {
  assert.equal(computeBigPictureNavAction(0, BTN_RIGHT), "NEXT_GAME");
  assert.equal(computeBigPictureNavAction(BTN_RIGHT, BTN_RIGHT), null);
  assert.equal(computeBigPictureNavAction(0, BTN_L), "PREV_TAB");
  assert.equal(computeBigPictureNavAction(0, BTN_R), "NEXT_TAB");
  assert.equal(computeBigPictureNavAction(0, BTN_A), "LAUNCH_GAME");
});

test("synthesizeMultiChannelSamples produces normalized 4-channel APU + SPC700 waveform", () => {
  const samples = synthesizeMultiChannelSamples(880, 12, 64);
  assert.equal(samples.length, 64);
  assert.ok(samples.every((s) => s >= -1.0 && s <= 1.0));
  assert.ok(samples.some((s) => Math.abs(s) > 0.2));
});
