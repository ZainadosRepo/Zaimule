/**
 * Pure & WebAudio module for Zaimule 4-Channel APU + SPC700 Chiptune synthesis
 * and real-time 60 FPS Oscilloscope waveform generation.
 */

export function sanitizeApuFrequency(freqHz) {
  const n = Number(freqHz) || 0;
  if (n < 60 || n > 4200) return 0;
  return Math.round(n);
}

/**
 * Generates `sampleCount` normalized (`-1.0..=1.0`) waveform samples combining:
 * - Ch1: Square Lead (`freqHz`)
 * - Ch2: Pulse Fifth Harmony (`1.5 * freqHz`)
 * - Ch3: Triangle / SPC700 Sub-Bass (`0.5 * freqHz`)
 * - Ch4: LFSR Pseudo-Noise transient
 */
export function synthesizeMultiChannelSamples(rawFreqHz, frameNumber = 0, sampleCount = 64) {
  const count = Math.max(8, Math.min(256, Number(sampleCount) || 64));
  const freq = sanitizeApuFrequency(rawFreqHz);
  const f = Number(frameNumber) || 0;
  const samples = new Array(count);

  const baseFreq = freq > 0 ? freq : 220;
  const ampScale = freq > 0 ? 0.88 : 0.22;

  for (let i = 0; i < count; i++) {
    const phase = (i / count) * Math.PI * 2 + f * 0.28;
    const ch1Square = Math.sin(phase * (baseFreq / 220)) >= 0 ? 0.45 : -0.45;
    const ch2Fifth = Math.sin(phase * ((baseFreq * 1.5) / 220)) >= 0.25 ? 0.22 : -0.22;
    const triRaw = ((i / count + f * 0.03) * 2) % 2;
    const ch3Triangle = (triRaw < 1 ? triRaw * 2 - 1 : 3 - triRaw * 2) * 0.25;
    const ch4Noise = freq > 0 ? (((i * 17 + f * 31) & 15) / 7.5 - 1) * 0.08 : 0;
    const mixed = (ch1Square + ch2Fifth + ch3Triangle + ch4Noise) * ampScale;
    samples[i] = Math.max(-1, Math.min(1, Number(mixed.toFixed(4))));
  }
  return samples;
}

export class ApuAudioEngine {
  constructor() {
    this.ctx = null;
    this.enabled = true;
    this.spcHarmonyEnabled = true;
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
    if (freq === this.lastPlayedFreq && nowMs - this.lastToneMs < 45) {
      return false;
    }

    const ctx = this.ensureContext();
    if (!ctx) return false;

    this.lastPlayedFreq = freq;
    this.lastToneMs = nowMs;

    // Channel 1: Pulse / Square Lead
    const osc1 = ctx.createOscillator();
    const gain1 = ctx.createGain();
    osc1.type = "square";
    osc1.frequency.setValueAtTime(freq, ctx.currentTime);
    gain1.gain.setValueAtTime(0.04, ctx.currentTime);
    gain1.gain.exponentialRampToValueAtTime(0.001, ctx.currentTime + 0.065);
    osc1.connect(gain1);
    gain1.connect(ctx.destination);
    osc1.start(ctx.currentTime);
    osc1.stop(ctx.currentTime + 0.07);

    // Channel 3: SPC700 / Triangle Sub-Bass + Fifth Harmony
    if (this.spcHarmonyEnabled) {
      const osc2 = ctx.createOscillator();
      const gain2 = ctx.createGain();
      osc2.type = "triangle";
      osc2.frequency.setValueAtTime(Math.max(65, freq * 0.5), ctx.currentTime);
      gain2.gain.setValueAtTime(0.03, ctx.currentTime);
      gain2.gain.exponentialRampToValueAtTime(0.001, ctx.currentTime + 0.085);
      osc2.connect(gain2);
      gain2.connect(ctx.destination);
      osc2.start(ctx.currentTime);
      osc2.stop(ctx.currentTime + 0.09);
    }
    return true;
  }
}
