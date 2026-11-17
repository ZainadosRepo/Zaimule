import test from "node:test";
import assert from "node:assert/strict";
import {
  BTN_A,
  BTN_B,
  BTN_RIGHT,
  BTN_UP,
  computeBitmaskFromPressedCodes,
  shouldFireTurboOnFrame,
  clampWidgetPosition,
  parseDeepLinkUri
} from "../../ui/keymapper.js";

test("computeBitmaskFromPressedCodes maps keyboard codes to joypad bitmask", () => {
  const pressed = new Set(["ArrowRight", "KeyJ", "KeyK"]);
  const mask = computeBitmaskFromPressedCodes(pressed);
  assert.equal(mask, BTN_RIGHT | BTN_A | BTN_B);

  const zqsd = new Set(["KeyZ"]);
  assert.equal(computeBitmaskFromPressedCodes(zqsd), BTN_UP);
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
