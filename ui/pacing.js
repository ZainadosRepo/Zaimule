/**
 * Pure ES module for display-refresh-independent emulation frame pacing.
 * Ensures cores run at their true hardware refresh rate (~59.73 Hz GB/GBC/GBA,
 * ~59.83 Hz NDS) even on 120 Hz, 144 Hz, or 240 Hz monitors, and measures real
 * FPS / frame times while preventing catch-up spikes after tab suspension.
 */

export const CONSOLE_REFRESH_HZ = {
  GB: 59.7275,
  GBC: 59.7275,
  GBA: 59.7275,
  NDS: 59.8261
};

export function targetFpsForConsole(consoleKind) {
  const key = String(consoleKind || "").toUpperCase();
  return CONSOLE_REFRESH_HZ[key] || 60.0;
}

export class FramePacer {
  constructor(options = {}) {
    this.maxDeltaMs = Math.max(20, Number(options.maxDeltaMs) || 100);
    this.windowSize = Math.max(4, Number(options.windowSize) || 30);
    this.reset();
  }

  reset(nowMs = null) {
    this.lastTimestampMs = typeof nowMs === "number" && Number.isFinite(nowMs) ? nowMs : null;
    this.accumulatorMs = 0;
    this.stepTimestamps = [];
    this.measuredFps = 0;
    this.frameTimeMs = 0;
  }

  /**
   * Advances the pacing accumulator to `nowMs` (in milliseconds, e.g. from rAF)
   * against `targetFps` and reports whether the emulation core should step on this tick.
   */
  advance(nowMs, targetFps = 60.0) {
    const fps = Math.max(15, Math.min(240, Number(targetFps) || 60.0));
    const framePeriodMs = 1000 / fps;
    const ts = Number(nowMs);

    if (!Number.isFinite(ts)) {
      return {
        shouldStep: true,
        measuredFps: this.measuredFps || fps,
        frameTimeMs: this.frameTimeMs || framePeriodMs
      };
    }

    if (this.lastTimestampMs === null) {
      this.lastTimestampMs = ts;
      this.accumulatorMs = 0;
      this._recordStep(ts, fps, framePeriodMs);
      return {
        shouldStep: true,
        measuredFps: this.measuredFps,
        frameTimeMs: this.frameTimeMs
      };
    }

    const rawDelta = ts - this.lastTimestampMs;
    this.lastTimestampMs = ts;

    if (rawDelta < 0) {
      // Clock moved backward; resync cleanly without stepping.
      this.accumulatorMs = 0;
      return {
        shouldStep: false,
        measuredFps: this.measuredFps || fps,
        frameTimeMs: this.frameTimeMs || framePeriodMs
      };
    }

    const spiked = rawDelta > this.maxDeltaMs;
    const clampedDelta = spiked ? framePeriodMs : rawDelta;
    this.accumulatorMs += clampedDelta;

    if (this.accumulatorMs + 0.0001 < framePeriodMs) {
      return {
        shouldStep: false,
        measuredFps: this.measuredFps || fps,
        frameTimeMs: this.frameTimeMs || framePeriodMs
      };
    }

    this.accumulatorMs -= framePeriodMs;
    // Prevent catch-up burst / spiral of death after a long stall or tab switch.
    if (spiked || this.accumulatorMs > framePeriodMs * 1.5) {
      this.accumulatorMs = Math.min(this.accumulatorMs, framePeriodMs * 0.5);
    }

    this._recordStep(ts, fps, framePeriodMs);
    return {
      shouldStep: true,
      measuredFps: this.measuredFps,
      frameTimeMs: this.frameTimeMs
    };
  }

  _recordStep(ts, fallbackFps, fallbackFrameTimeMs) {
    this.stepTimestamps.push(ts);
    if (this.stepTimestamps.length > this.windowSize) {
      this.stepTimestamps.shift();
    }
    if (this.stepTimestamps.length >= 2) {
      const spanMs = this.stepTimestamps[this.stepTimestamps.length - 1] - this.stepTimestamps[0];
      const intervals = this.stepTimestamps.length - 1;
      if (spanMs > 0) {
        this.frameTimeMs = spanMs / intervals;
        this.measuredFps = (intervals * 1000) / spanMs;
        return;
      }
    }
    this.measuredFps = fallbackFps;
    this.frameTimeMs = fallbackFrameTimeMs;
  }
}
