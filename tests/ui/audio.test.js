import test from "node:test";
import assert from "node:assert/strict";
import {
  sanitizeApuFrequency,
  synthesizeMultiChannelSamples,
  resampleInterleavedStereo,
  ApuAudioEngine,
} from "../../ui/audio.js";

test("sanitizeApuFrequency filters out-of-range and non-finite frequencies", () => {
  assert.equal(sanitizeApuFrequency(0), 0);
  assert.equal(sanitizeApuFrequency(59), 0);
  assert.equal(sanitizeApuFrequency(4201), 0);
  assert.equal(sanitizeApuFrequency(Number.NaN), 0);
  assert.equal(sanitizeApuFrequency(Infinity), 0);
  assert.equal(sanitizeApuFrequency(440), 440);
  assert.equal(sanitizeApuFrequency(60), 60);
  assert.equal(sanitizeApuFrequency(4200), 4200);
});

test("synthesizeMultiChannelSamples scales standby vs active frequencies and stays normalized", () => {
  const standby = synthesizeMultiChannelSamples(0, 1, 32);
  assert.equal(standby.length, 32);
  assert.ok(standby.every((v) => v >= -1 && v <= 1));
  const maxStandby = Math.max(...standby.map(Math.abs));

  const active = synthesizeMultiChannelSamples(440, 2, 64);
  assert.equal(active.length, 64);
  assert.ok(active.every((v) => v >= -1 && v <= 1));
  const maxActive = Math.max(...active.map(Math.abs));
  assert.ok(maxActive > maxStandby, "active tone should have higher amplitude than standby");
});

test("resampleInterleavedStereo returns copy when rates match and preserves stereo parity on conversion", () => {
  const input = new Float32Array([0.2, -0.2, 0.4, -0.4, 0.6, -0.6, 0.8, -0.8]);
  const same = resampleInterleavedStereo(input, 48000, 48000);
  assert.deepEqual(Array.from(same), Array.from(input));

  const upsampled = resampleInterleavedStereo(input, 32768, 48000);
  assert.equal(upsampled.length % 2, 0);
  assert.ok(upsampled.length > input.length);
  for (let i = 0; i < upsampled.length; i += 2) {
    assert.ok(upsampled[i] >= 0, "left channel stays non-negative");
    assert.ok(upsampled[i + 1] <= 0, "right channel stays non-positive");
  }

  const empty = resampleInterleavedStereo(new Float32Array(0), 48000, 44100);
  assert.equal(empty.length, 0);
});

test("ApuAudioEngine clamps volume, toggles enabled cleanly, and stays safe in headless Node", () => {
  const engine = new ApuAudioEngine();
  assert.equal(engine.enabled, true);
  assert.equal(engine.volume, 0.85);

  assert.equal(engine.setVolume(2.5), 1);
  assert.equal(engine.setVolume(-0.4), 0);
  assert.equal(engine.setVolume(Number.NaN), 0.85);
  assert.equal(engine.setVolume(0.75), 0.75);

  assert.equal(engine.toggleEnabled(), false);
  assert.equal(engine.enabled, false);
  assert.equal(engine.pushPcmSamples(new Float32Array([0.1, 0.1]), 48000), false);
  assert.equal(engine.triggerFrameTone(440), false);

  assert.equal(engine.toggleEnabled(), true);
  assert.equal(engine.enabled, true);
  // Headless Node has no window/AudioContext -> returns false without throwing.
  assert.equal(engine.pushPcmSamples(new Float32Array([0.1, 0.1]), 48000), false);
  engine.reset();
});

test("ApuAudioEngine routes PCM through AudioWorklet when ready and falls back to BufferSource when failed", () => {
  const engine = new ApuAudioEngine();
  const posted = [];
  const startedSources = [];
  engine.ctx = {
    sampleRate: 48000,
    currentTime: 1.0,
    state: "running",
    createBuffer(channels, frames, rate) {
      const data = Array.from({ length: channels }, () => new Float32Array(frames));
      return {
        channels,
        frames,
        rate,
        duration: frames / rate,
        getChannelData: (c) => data[c],
      };
    },
    createBufferSource() {
      return {
        buffer: null,
        startedAt: null,
        connected: false,
        connect() {
          this.connected = true;
        },
        start(t) {
          this.startedAt = t;
          startedSources.push(this);
        },
      };
    },
  };
  engine.ensureContext = () => (engine.enabled ? engine.ctx : null);
  engine.gainNode = {
    gain: {
      value: 0.85,
      setTargetAtTime(v) {
        this.value = v;
      },
    },
  };
  engine.workletReady = true;
  engine.workletNode = {
    port: {
      postMessage(msg) {
        posted.push(msg);
      },
    },
  };

  // Worklet path
  const okWorklet = engine.pushPcmSamples(new Float32Array([0.25, -0.25, 0.5, -0.5]), 48000);
  assert.equal(okWorklet, true);
  assert.equal(posted.length, 1);
  assert.equal(posted[0].type, "pcm");
  assert.equal(posted[0].samples.length, 4);

  // Reset flushes worklet ring buffer
  engine.reset();
  assert.equal(posted.length, 2);
  assert.equal(posted[1].type, "reset");

  // Fallback BufferSource path when worklet failed
  engine.workletReady = false;
  engine.workletFailed = true;
  const okFallback = engine.pushPcmSamples(new Float32Array([0.25, -0.25, 0.5, -0.5]), 48000);
  assert.equal(okFallback, true);
  assert.equal(startedSources.length, 1);
  assert.equal(startedSources[0].connected, true);
  assert.ok(startedSources[0].startedAt >= 1.0);
});
