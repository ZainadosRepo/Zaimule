import test from "node:test";
import assert from "node:assert/strict";
import { FramePacer, targetFpsForConsole } from "../../ui/pacing.js";

test("targetFpsForConsole returns hardware-accurate refresh rates", () => {
  assert.equal(targetFpsForConsole("GB"), 59.7275);
  assert.equal(targetFpsForConsole("GBC"), 59.7275);
  assert.equal(targetFpsForConsole("GBA"), 59.7275);
  assert.equal(targetFpsForConsole("NDS"), 59.8261);
  assert.equal(targetFpsForConsole("UNKNOWN"), 60.0);
});

test("FramePacer locks step rate to ~59.73 Hz across 60Hz, 120Hz, 144Hz, and 240Hz monitors", () => {
  const targetFps = targetFpsForConsole("GBA");
  const durationMs = 10_000;

  for (const displayHz of [60, 120, 144, 240]) {
    const pacer = new FramePacer();
    const tickMs = 1000 / displayHz;
    let stepped = 0;
    let lastResult = null;

    for (let t = 0; t <= durationMs; t += tickMs) {
      const res = pacer.advance(t, targetFps);
      if (res.shouldStep) {
        stepped += 1;
        lastResult = res;
      }
    }

    const expectedSteps = (durationMs / 1000) * targetFps;
    assert.ok(
      Math.abs(stepped - expectedSteps) <= expectedSteps * 0.02,
      `display ${displayHz}Hz stepped ${stepped} times, expected ~${expectedSteps.toFixed(1)}`
    );
    assert.ok(
      Math.abs(lastResult.measuredFps - targetFps) <= 1.5,
      `display ${displayHz}Hz measured ${lastResult.measuredFps.toFixed(2)} FPS`
    );
  }
});

test("FramePacer clamps tab-backgrounding spikes and prevents catch-up bursts", () => {
  const pacer = new FramePacer();
  const targetFps = targetFpsForConsole("NDS");

  assert.equal(pacer.advance(0, targetFps).shouldStep, true);
  assert.equal(pacer.advance(16.75, targetFps).shouldStep, true);

  // Simulate a 5-second background tab pause followed by normal 120Hz ticks
  const afterPause = pacer.advance(5016.75, targetFps);
  assert.equal(afterPause.shouldStep, true);

  // Next 120Hz tick (+8.33 ms) should NOT burst-step immediately
  const nextTick = pacer.advance(5025.08, targetFps);
  assert.equal(nextTick.shouldStep, false);
});

test("FramePacer reset clears accumulated state and handles backward timestamps safely", () => {
  const pacer = new FramePacer();
  assert.equal(pacer.advance(100, 60).shouldStep, true);
  assert.equal(pacer.advance(90, 60).shouldStep, false);
  pacer.reset();
  assert.equal(pacer.advance(500, 60).shouldStep, true);
});
