/**
 * AudioWorkletProcessor streaming interleaved stereo Float32 PCM frames from Zaimule's Rust cores
 * with even-aligned ring-buffer overflow handling and smooth underrun attenuation.
 * Served locally from 'self' to comply cleanly with Tauri's Content-Security-Policy.
 */

const RING_FLOATS = 16384; // 8192 stereo pairs (~170 ms @ 48 kHz)
const MAX_LATENCY_FLOATS = 9216; // 4608 stereo pairs (~96 ms @ 48 kHz)
const TARGET_LATENCY_FLOATS = 4096; // 2048 stereo pairs (~42 ms @ 48 kHz)
const TRIM_FADE_FRAMES = 32;

class ZaimulePcmWorklet extends AudioWorkletProcessor {
  constructor() {
    super();
    this.ring = new Float32Array(RING_FLOATS);
    this.readPos = 0;
    this.writePos = 0;
    this.available = 0;
    this.lastL = 0;
    this.lastR = 0;
    this.trimFadeRem = 0;
    this.fadeStartL = 0;
    this.fadeStartR = 0;

    this.port.onmessage = (event) => {
      const msg = event?.data;
      if (!msg) return;

      if (msg.type === "reset") {
        this.readPos = 0;
        this.writePos = 0;
        this.available = 0;
        this.lastL = 0;
        this.lastR = 0;
        this.trimFadeRem = 0;
        this.fadeStartL = 0;
        this.fadeStartR = 0;
        return;
      }

      if (msg.type === "pcm" && msg.samples) {
        const incoming =
          msg.samples instanceof Float32Array ? msg.samples : new Float32Array(msg.samples);
        this.enqueueStereo(incoming);
      }
    };
  }

  enqueueStereo(samples) {
    // Preserve strict stereo pair parity
    let len = samples.length & ~1;
    if (len <= 0) return;

    let srcOffset = 0;
    if (len > RING_FLOATS) {
      srcOffset = (len - RING_FLOATS) & ~1;
      len = RING_FLOATS;
    }

    const free = RING_FLOATS - this.available;
    if (len > free) {
      let drop = len - free;
      if ((drop & 1) !== 0) drop += 1;
      drop = Math.min(this.available, drop);
      this.readPos = (this.readPos + drop) % RING_FLOATS;
      this.available -= drop;
    }

    for (let i = 0; i < len; i += 1) {
      this.ring[this.writePos] = samples[srcOffset + i];
      this.writePos = (this.writePos + 1) % RING_FLOATS;
    }
    this.available += len;

    // Trim long-session clock drift back to ~42 ms with a 32-frame crossfade
    if (this.available > MAX_LATENCY_FLOATS) {
      const drop = (this.available - TARGET_LATENCY_FLOATS) & ~1;
      if (drop > 0) {
        this.readPos = (this.readPos + drop) % RING_FLOATS;
        this.available -= drop;
        this.fadeStartL = this.lastL;
        this.fadeStartR = this.lastR;
        this.trimFadeRem = TRIM_FADE_FRAMES;
      }
    }
  }

  process(_inputs, outputs) {
    const outChannels = outputs[0];
    if (!outChannels || outChannels.length === 0) {
      return true;
    }
    const outL = outChannels[0];
    const outR = outChannels[1] || outL;
    const frames = outL.length;

    const availPairs = this.available >> 1;
    const toRead = Math.min(frames, availPairs);

    for (let i = 0; i < toRead; i += 1) {
      let l = this.ring[this.readPos];
      let r = this.ring[(this.readPos + 1) % RING_FLOATS];
      this.readPos = (this.readPos + 2) % RING_FLOATS;
      if (this.trimFadeRem > 0) {
        const alpha = (TRIM_FADE_FRAMES - this.trimFadeRem + 1) / TRIM_FADE_FRAMES;
        l = this.fadeStartL + (l - this.fadeStartL) * alpha;
        r = this.fadeStartR + (r - this.fadeStartR) * alpha;
        this.trimFadeRem -= 1;
      }
      this.lastL = l;
      this.lastR = r;
      outL[i] = l;
      if (outR !== outL) {
        outR[i] = r;
      } else {
        outL[i] = (l + r) * 0.5;
      }
    }
    this.available -= toRead * 2;

    // Smooth underrun fade-out prevents clicks between paused/resumed frames
    for (let i = toRead; i < frames; i += 1) {
      this.lastL *= 0.96;
      this.lastR *= 0.96;
      if (Math.abs(this.lastL) < 1e-5) this.lastL = 0;
      if (Math.abs(this.lastR) < 1e-5) this.lastR = 0;
      outL[i] = outR !== outL ? this.lastL : (this.lastL + this.lastR) * 0.5;
      if (outR !== outL) {
        outR[i] = this.lastR;
      }
    }

    return true;
  }
}

registerProcessor("zaimule-pcm-worklet", ZaimulePcmWorklet);
