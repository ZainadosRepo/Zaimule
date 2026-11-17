import test from "node:test";
import assert from "node:assert/strict";
import {
  BTN_A,
  BTN_B,
  BTN_LEFT,
  BTN_RIGHT,
  BTN_START,
  BTN_UP,
  computeBitmaskFromPressedCodes,
  computeBitmaskFromGamepad,
  shouldFireTurboOnFrame,
  clampWidgetPosition,
  parseDeepLinkUri
} from "../../ui/keymapper.js";
import { sanitizeApuFrequency } from "../../ui/audio.js";

test("computeBitmaskFromPressedCodes maps keyboard codes to joypad bitmask", () => {
  const pressed = new Set(["ArrowRight", "KeyJ", "KeyK"]);
  const mask = computeBitmaskFromPressedCodes(pressed);
  assert.equal(mask, BTN_RIGHT | BTN_A | BTN_B);

  const zqsd = new Set(["KeyZ"]);
  assert.equal(computeBitmaskFromPressedCodes(zqsd), BTN_UP);
});

test("computeBitmaskFromGamepad maps standard W3C buttons and analog stick axes", () => {
  const fakePad = {
    buttons: Array.from({ length: 16 }, (_, idx) => ({
      pressed: idx === 0 || idx === 9
    })),
    axes: [-0.8, 0.0]
  };
  const mask = computeBitmaskFromGamepad(fakePad);
  assert.equal(mask, BTN_A | BTN_START | BTN_LEFT);
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
