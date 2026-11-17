/**
 * Pure & WebAudio module for Zaimule APU Chiptune synthesis (Square / Pulse wave).
 * Synthesizes hardware frequency tones emitted by the Game Boy / NES / CHIP-8 cores.
 */

export function sanitizeApuFrequency(freqHz) {
  const n = Number(freqHz) || 0;
  if (n < 60 || n > 4200) return 0;
  return Math.round(n);
}

export class ApuAudioEngine {
  constructor() {
    this.ctx = null;
    this.enabled = true;
    this.lastPlayedFreq = 0;
    this.lastToneMs = 0;
  }

  ensureContext() {
    if (!this.enabled || typeof window === "undefined") return null;
    const AudioCtx = window.AudioContext || window.webkitAudioContext;
    if (!AudioCtx) return null;
    if (!this.ctx) {
      this.ctx = new AudioCtx();
    }
    if (this.ctx.state === "suspended") {
      this.ctx.resume().catch(() => {});
    }
    return this.ctx;
  }

  toggleEnabled() {
    this.enabled = !this.enabled;
    return this.enabled;
  }

  triggerFrameTone(rawFreqHz) {
    if (!this.enabled) return false;
    const freq = sanitizeApuFrequency(rawFreqHz);
    if (freq === 0) {
      this.lastPlayedFreq = 0;
      return false;
    }

    const nowMs = Date.now();
    // Avoid re-triggering identical sustained tones faster than 45ms
    if (freq === this.lastPlayedFreq && nowMs - this.lastToneMs < 45) {
      return false;
    }

    const ctx = this.ensureContext();
    if (!ctx) return false;

    this.lastPlayedFreq = freq;
    this.lastToneMs = nowMs;

    const osc = ctx.createOscillator();
    const gain = ctx.createGain();
    osc.type = "square";
    osc.frequency.setValueAtTime(freq, ctx.currentTime);

    gain.gain.setValueAtTime(0.045, ctx.currentTime);
    gain.gain.exponentialRampToValueAtTime(0.001, ctx.currentTime + 0.065);

    osc.connect(gain);
    gain.connect(ctx.destination);
    osc.start(ctx.currentTime);
    osc.stop(ctx.currentTime + 0.07);
    return true;
  }
}
