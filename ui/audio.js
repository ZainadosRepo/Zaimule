/**
 * Real-time PCM AudioWorklet streaming & WebAudio fallback engine for Zaimule,
 * plus 4-channel APU/oscilloscope waveform helpers.
 */

export function sanitizeApuFrequency(freqHz) {
  const n = Number(freqHz) || 0;
  if (n < 60 || n > 4200) return 0;
  return Math.round(n);
}

/**
 * Generates `sampleCount` normalized (`-1.0..=1.0`) waveform samples combining:
 * - Ch1: Square Lead / YM2612 FM Carrier (`freqHz`)
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
    const fmMod = Math.sin(phase * 2.0) * 0.35;
    const ch1Square = Math.sin(phase * (baseFreq / 220) + fmMod) >= 0 ? 0.45 : -0.45;
    const ch2Fifth = Math.sin(phase * ((baseFreq * 1.5) / 220)) >= 0.25 ? 0.22 : -0.22;
    const triRaw = ((i / count + f * 0.03) * 2) % 2;
    const ch3Triangle = (triRaw < 1 ? triRaw * 2 - 1 : 3 - triRaw * 2) * 0.25;
    const ch4Noise = freq > 0 ? (((i * 17 + f * 31) & 15) / 7.5 - 1) * 0.08 : 0;
    const mixed = (ch1Square + ch2Fifth + ch3Triangle + ch4Noise) * ampScale;
    samples[i] = Math.max(-1, Math.min(1, Number(mixed.toFixed(4))));
  }
  return samples;
}

/**
 * Linearly resamples interleaved stereo Float32 PCM (`[L0, R0, L1, R1, ...]`)
 * when the host AudioContext runs at a sample rate other than `srcRate`.
 */
export function resampleInterleavedStereo(samples, srcRate, dstRate) {
  const srcFrames = samples.length >> 1;
  if (srcFrames === 0 || !srcRate || !dstRate || Math.abs(srcRate - dstRate) < 1) {
    return samples instanceof Float32Array ? samples : new Float32Array(samples);
  }
  const ratio = srcRate / dstRate;
  const dstFrames = Math.max(1, Math.round(srcFrames / ratio));
  const out = new Float32Array(dstFrames * 2);

  for (let i = 0; i < dstFrames; i += 1) {
    const srcPos = i * ratio;
    const idx0 = Math.min(srcFrames - 1, Math.floor(srcPos));
    const idx1 = Math.min(srcFrames - 1, idx0 + 1);
    const frac = srcPos - idx0;

    const l0 = samples[idx0 * 2];
    const r0 = samples[idx0 * 2 + 1];
    const l1 = samples[idx1 * 2];
    const r1 = samples[idx1 * 2 + 1];

    out[i * 2] = l0 + (l1 - l0) * frac;
    out[i * 2 + 1] = r0 + (r1 - r0) * frac;
  }
  return out;
}

export class ApuAudioEngine {
  constructor() {
    this.ctx = null;
    this.gainNode = null;
    this.workletNode = null;
    this.workletReady = false;
    this.workletFailed = false;
    this.workletInitPromise = null;
    this.nextScheduleTime = 0;
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
      try {
        this.ctx = new AudioCtx({ sampleRate: 48000 });
      } catch {
        this.ctx = new AudioCtx();
      }
      this.gainNode = this.ctx.createGain();
      this.gainNode.gain.value = this.enabled ? 0.85 : 0.0;
      this.gainNode.connect(this.ctx.destination);
      this._initWorklet();
    }
    if (this.ctx.state === "suspended") {
      this.ctx.resume().catch(() => {});
    }
    return this.ctx;
  }

  _initWorklet() {
    if (!this.ctx || !this.ctx.audioWorklet) {
      this.workletFailed = true;
      return;
    }
    if (this.workletInitPromise || this.workletFailed) return;

    this.workletInitPromise = (async () => {
      try {
        await this.ctx.audioWorklet.addModule("./audio_worklet.js");
        const node = new AudioWorkletNode(this.ctx, "zaimule-pcm-worklet", {
          numberOfInputs: 0,
          numberOfOutputs: 1,
          outputChannelCount: [2]
        });
        node.connect(this.gainNode);
        this.workletNode = node;
        this.workletReady = true;
      } catch (err) {
        console.warn("[zaimule-audio] AudioWorklet indisponible, repli BufferSource:", err);
        this.workletFailed = true;
      }
    })();
  }

  reset() {
    if (this.workletReady && this.workletNode) {
      this.workletNode.port.postMessage({ type: "reset" });
    }
    this.nextScheduleTime = 0;
    this.lastPlayedFreq = 0;
  }

  toggleEnabled() {
    this.enabled = !this.enabled;
    if (this.gainNode && this.ctx) {
      this.gainNode.gain.setTargetAtTime(
        this.enabled ? 0.85 : 0.0,
        this.ctx.currentTime,
        0.01
      );
    }
    if (!this.enabled) {
      this.reset();
    }
    return this.enabled;
  }

  pushPcmSamples(pcmSamples, sampleRateHz = 48000) {
    if (!this.enabled || !pcmSamples || pcmSamples.length < 2) return false;
    const ctx = this.ensureContext();
    if (!ctx) return false;

    const rate = Number(sampleRateHz) || 48000;
    const prepared = resampleInterleavedStereo(pcmSamples, rate, ctx.sampleRate);

    if (this.workletReady && this.workletNode) {
      // Ensure standalone transferable ArrayBuffer sized exactly to the slice
      const owned =
        prepared.byteOffset === 0 && prepared.byteLength === prepared.buffer.byteLength
          ? prepared
          : new Float32Array(prepared);
      this.workletNode.port.postMessage({ type: "pcm", samples: owned }, [owned.buffer]);
      return true;
    }

    if (!this.workletFailed) {
      // Worklet is finishing initial load cleanly
      return false;
    }

    // Fallback scheduled WebAudio buffer streaming when AudioWorklet is blocked
    const frames = prepared.length >> 1;
    if (frames === 0) return false;
    const audioBuf = ctx.createBuffer(2, frames, ctx.sampleRate);
    const chL = audioBuf.getChannelData(0);
    const chR = audioBuf.getChannelData(1);
    for (let i = 0; i < frames; i += 1) {
      chL[i] = prepared[i * 2];
      chR[i] = prepared[i * 2 + 1];
    }
    const src = ctx.createBufferSource();
    src.buffer = audioBuf;
    src.connect(this.gainNode);

    const now = ctx.currentTime;
    if (this.nextScheduleTime < now || this.nextScheduleTime > now + 0.25) {
      this.nextScheduleTime = now + 0.02;
    }
    src.start(this.nextScheduleTime);
    this.nextScheduleTime += audioBuf.duration;
    return true;
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

    const osc1 = ctx.createOscillator();
    const gain1 = ctx.createGain();
    osc1.type = "square";
    osc1.frequency.setValueAtTime(freq, ctx.currentTime);
    gain1.gain.setValueAtTime(0.04, ctx.currentTime);
    gain1.gain.exponentialRampToValueAtTime(0.001, ctx.currentTime + 0.065);
    osc1.connect(gain1);
    gain1.connect(this.gainNode || ctx.destination);
    osc1.start(ctx.currentTime);
    osc1.stop(ctx.currentTime + 0.07);

    if (this.spcHarmonyEnabled) {
      const osc2 = ctx.createOscillator();
      const gain2 = ctx.createGain();
      osc2.type = "triangle";
      osc2.frequency.setValueAtTime(Math.max(65, freq * 0.5), ctx.currentTime);
      gain2.gain.setValueAtTime(0.03, ctx.currentTime);
      gain2.gain.exponentialRampToValueAtTime(0.001, ctx.currentTime + 0.085);
      osc2.connect(gain2);
      gain2.connect(this.gainNode || ctx.destination);
      osc2.start(ctx.currentTime);
      osc2.stop(ctx.currentTime + 0.09);
    }
    return true;
  }
}
