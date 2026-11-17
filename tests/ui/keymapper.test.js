import test from "node:test";
import assert from "node:assert/strict";
import {
  BTN_A,
  BTN_B,
  BTN_L,
  BTN_LEFT,
  BTN_R,
  BTN_RIGHT,
  BTN_START,
  BTN_UP,
  computeBitmaskFromPressedCodes,
  computeBitmaskFromGamepad,
  shouldFireTurboOnFrame,
  clampWidgetPosition,
  parseDeepLinkUri,
  triggerGamepadRumble,
  computeBigPictureNavAction
} from "../../ui/keymapper.js";
import { sanitizeApuFrequency, synthesizeMultiChannelSamples } from "../../ui/audio.js";

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
