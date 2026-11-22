// Zaimule — Real-Time Pixel-Art Upscaler & CRT Phosphor Bloom Pipeline
// Pure ES module testable under Node.js (`node --test`) and browser WebView.

export const UPSCALER_MODES = ["OFF", "SHARP", "SCALE2X", "XBRZ", "BLOOM"];

const IS_LITTLE_ENDIAN = (() => {
  const buf = new ArrayBuffer(2);
  new DataView(buf).setUint16(0, 0x0102, true);
  return new Uint8Array(buf)[0] === 0x02;
})();

let alignedScratchBuf = null;
let alignedScratchU8 = null;
let alignedScratchU32 = null;

function ensureAlignedScratch(byteLen) {
  if (!alignedScratchBuf || alignedScratchBuf.byteLength < byteLen) {
    const cap = Math.max(byteLen, 512 * 1024);
    alignedScratchBuf = new ArrayBuffer(cap);
    alignedScratchU8 = new Uint8Array(alignedScratchBuf);
    alignedScratchU32 = new Uint32Array(alignedScratchBuf);
  }
}

function toAlignedUint32Pixels(rgba, pixelCount) {
  const byteLen = pixelCount * 4;
  if (
    (rgba instanceof Uint8ClampedArray || rgba instanceof Uint8Array) &&
    rgba.byteLength >= byteLen
  ) {
    if ((rgba.byteOffset & 3) === 0) {
      return new Uint32Array(rgba.buffer, rgba.byteOffset, pixelCount);
    }
    ensureAlignedScratch(byteLen);
    alignedScratchU8.set(
      rgba.byteLength === byteLen ? rgba : rgba.subarray(0, byteLen),
      0
    );
    return alignedScratchU32;
  }
  ensureAlignedScratch(byteLen);
  const copyLen = Math.min(rgba.length, byteLen);
  for (let i = 0; i < copyLen; i += 1) {
    alignedScratchU8[i] = rgba[i];
  }
  if (copyLen < byteLen) {
    alignedScratchU8.fill(0, copyLen, byteLen);
  }
  return alignedScratchU32;
}

function yuvDistance(rgba, idxA, idxB) {
  const dr = Math.abs(rgba[idxA] - rgba[idxB]);
  const dg = Math.abs(rgba[idxA + 1] - rgba[idxB + 1]);
  const db = Math.abs(rgba[idxA + 2] - rgba[idxB + 2]);
  return (dr * 77 + dg * 150 + db * 29) >> 8;
}

function blendPixel(src, idxA, idxB, weightB, dst, dstIdx) {
  const wA = 256 - weightB;
  dst[dstIdx] = (src[idxA] * wA + src[idxB] * weightB) >> 8;
  dst[dstIdx + 1] = (src[idxA + 1] * wA + src[idxB + 1] * weightB) >> 8;
  dst[dstIdx + 2] = (src[idxA + 2] * wA + src[idxB + 2] * weightB) >> 8;
  dst[dstIdx + 3] = src[idxA + 3];
}

function copyPixel(src, srcIdx, dst, dstIdx) {
  dst[dstIdx] = src[srcIdx];
  dst[dstIdx + 1] = src[srcIdx + 1];
  dst[dstIdx + 2] = src[srcIdx + 2];
  dst[dstIdx + 3] = src[srcIdx + 3];
}

/**
 * Applies Eric Johnston's EPX / Scale2x pixel-art edge-preserving 2x upscaler
 * on an RGBA8888 buffer of size `width * height * 4`.
 */
export function applyScale2xRgba(rgba, width, height) {
  const w = Math.max(1, Number(width) || 1);
  const h = Math.max(1, Number(height) || 1);
  const outW = w * 2;
  const outH = h * 2;
  const out = new Uint8ClampedArray(outW * outH * 4);
  const out32 = new Uint32Array(out.buffer);
  const src32 = toAlignedUint32Pixels(rgba, w * h);
  const lastX = w - 1;

  for (let y = 0; y < h; y += 1) {
    const rowCur = y * w;
    const rowUp = y > 0 ? rowCur - w : rowCur;
    const rowDown = y + 1 < h ? rowCur + w : rowCur;
    let dstTop = y * 2 * outW;
    let dstBot = dstTop + outW;

    let P = src32[rowCur];
    let D = P;
    let F = w > 1 ? src32[rowCur + 1] : P;

    for (let x = 0; x < w; x += 1) {
      const B = src32[rowUp + x];
      const H = src32[rowDown + x];

      if (B === H || D === F) {
        out32[dstTop] = P;
        out32[dstTop + 1] = P;
        out32[dstBot] = P;
        out32[dstBot + 1] = P;
      } else {
        out32[dstTop] = D === B ? D : P;
        out32[dstTop + 1] = B === F ? F : P;
        out32[dstBot] = D === H ? D : P;
        out32[dstBot + 1] = H === F ? F : P;
      }

      dstTop += 2;
      dstBot += 2;
      D = P;
      P = F;
      if (x + 2 <= lastX) {
        F = src32[rowCur + x + 2];
      }
    }
  }

  return { width: outW, height: outH, rgba: out };
}

function yuvDistance32LE(a, b) {
  let dr = (a & 0xff) - (b & 0xff);
  if (dr < 0) dr = -dr;
  let dg = ((a >>> 8) & 0xff) - ((b >>> 8) & 0xff);
  if (dg < 0) dg = -dg;
  let db = ((a >>> 16) & 0xff) - ((b >>> 16) & 0xff);
  if (db < 0) db = -db;
  return (dr * 77 + dg * 150 + db * 29) >> 8;
}

function blend192LE(p, q) {
  const r = ((p & 0xff) + (q & 0xff) * 3) >> 2;
  const g = (((p >>> 8) & 0xff) + ((q >>> 8) & 0xff) * 3) >> 2;
  const b = (((p >>> 16) & 0xff) + ((q >>> 16) & 0xff) * 3) >> 2;
  return r | (g << 8) | (b << 16) | (p & 0xff000000);
}

/**
 * Applies a Zenju-style xBRZ 2x edge-directed YUV luma vector upscaler with smooth
 * diagonal corner interpolation (`192/256` edge weight) on an RGBA8888 buffer.
 */
export function applyXbrz2xRgba(rgba, width, height) {
  const w = Math.max(1, Number(width) || 1);
  const h = Math.max(1, Number(height) || 1);
  const outW = w * 2;
  const outH = h * 2;
  const out = new Uint8ClampedArray(outW * outH * 4);

  if (IS_LITTLE_ENDIAN) {
    const out32 = new Uint32Array(out.buffer);
    const src32 = toAlignedUint32Pixels(rgba, w * h);
    const lastX = w - 1;

    for (let y = 0; y < h; y += 1) {
      const rowCur = y * w;
      const rowUp = y > 0 ? rowCur - w : rowCur;
      const rowDown = y + 1 < h ? rowCur + w : rowCur;
      let dstTop = y * 2 * outW;
      let dstBot = dstTop + outW;

      let P = src32[rowCur];
      let D = P;
      let F = w > 1 ? src32[rowCur + 1] : P;

      for (let x = 0; x < w; x += 1) {
        const B = src32[rowUp + x];
        const H = src32[rowDown + x];

        let e0 = P;
        let e1 = P;
        let e2 = P;
        let e3 = P;

        if (B !== H && D !== F && yuvDistance32LE(B, H) > 16 && yuvDistance32LE(D, F) > 16) {
          if (P !== D && yuvDistance32LE(P, D) > 20) {
            const blendD = blend192LE(P, D);
            if (D === B || yuvDistance32LE(D, B) < 32) {
              e0 = blendD;
            }
            if (D === H || yuvDistance32LE(D, H) < 32) {
              e2 = blendD;
            }
          }
          if (P !== F && yuvDistance32LE(P, F) > 20) {
            const blendF = blend192LE(P, F);
            if (B === F || yuvDistance32LE(B, F) < 32) {
              e1 = blendF;
            }
            if (H === F || yuvDistance32LE(H, F) < 32) {
              e3 = blendF;
            }
          }
        }

        out32[dstTop] = e0;
        out32[dstTop + 1] = e1;
        out32[dstBot] = e2;
        out32[dstBot + 1] = e3;

        dstTop += 2;
        dstBot += 2;
        D = P;
        P = F;
        if (x + 2 <= lastX) {
          F = src32[rowCur + x + 2];
        }
      }
    }

    return { width: outW, height: outH, rgba: out };
  }

  for (let y = 0; y < h; y += 1) {
    const yUp = y > 0 ? y - 1 : y;
    const yDown = y + 1 < h ? y + 1 : y;

    for (let x = 0; x < w; x += 1) {
      const xLeft = x > 0 ? x - 1 : x;
      const xRight = x + 1 < w ? x + 1 : x;

      const p = (y * w + x) * 4;
      const b = (yUp * w + x) * 4;
      const hIdx = (yDown * w + x) * 4;
      const d = (y * w + xLeft) * 4;
      const f = (y * w + xRight) * 4;

      const dstTopRow = y * 2 * outW + x * 2;
      const dstBotRow = (y * 2 + 1) * outW + x * 2;

      copyPixel(rgba, p, out, dstTopRow * 4);
      copyPixel(rgba, p, out, (dstTopRow + 1) * 4);
      copyPixel(rgba, p, out, dstBotRow * 4);
      copyPixel(rgba, p, out, (dstBotRow + 1) * 4);

      if (yuvDistance(rgba, b, hIdx) > 16 && yuvDistance(rgba, d, f) > 16) {
        if (yuvDistance(rgba, p, d) > 20) {
          if (yuvDistance(rgba, d, b) < 32) {
            blendPixel(rgba, p, d, 192, out, dstTopRow * 4);
          }
          if (yuvDistance(rgba, d, hIdx) < 32) {
            blendPixel(rgba, p, d, 192, out, dstBotRow * 4);
          }
        }
        if (yuvDistance(rgba, p, f) > 20) {
          if (yuvDistance(rgba, b, f) < 32) {
            blendPixel(rgba, p, f, 192, out, (dstTopRow + 1) * 4);
          }
          if (yuvDistance(rgba, hIdx, f) < 32) {
            blendPixel(rgba, p, f, 192, out, (dstBotRow + 1) * 4);
          }
        }
      }
    }
  }

  return { width: outW, height: outH, rgba: out };
}

/**
 * Applies a warm CRT phosphor bloom on high-luma pixels with horizontal glow bleed.
 */
export function applyCrtPhosphorBloomRgba(rgba, width, height) {
  const w = Math.max(1, Number(width) || 1);
  const h = Math.max(1, Number(height) || 1);
  const out = new Uint8ClampedArray(rgba.length);
  const lastX = w - 1;

  if (IS_LITTLE_ENDIAN && out.byteLength >= w * h * 4) {
    const out32 = new Uint32Array(out.buffer, out.byteOffset, w * h);
    const src32 = toAlignedUint32Pixels(rgba, w * h);

    for (let y = 0; y < h; y += 1) {
      const rowOff = y * w;
      let cur = src32[rowOff];
      let lr = cur & 0xff;
      let lg = (cur >>> 8) & 0xff;
      let lb = (cur >>> 16) & 0xff;

      let r = lr;
      let g = lg;
      let b = lb;

      let next = w > 1 ? src32[rowOff + 1] : cur;
      let rr = next & 0xff;
      let rg = (next >>> 8) & 0xff;
      let rb = (next >>> 16) & 0xff;

      for (let x = 0; x < w; x += 1) {
        const luma = (r * 77 + g * 150 + b * 29) >> 8;
        const boost = luma > 140 ? (luma - 140) >> 2 : 0;

        let outR = r + ((lr + rr) >> 3) + boost + 4;
        if (outR > 255) outR = 255;
        let outG = g + ((lg + rg) >> 3) + boost + 2;
        if (outG > 255) outG = 255;
        let outB = b + ((lb + rb) >> 3) + (boost >> 1);
        if (outB > 255) outB = 255;

        out32[rowOff + x] = outR | (outG << 8) | (outB << 16) | (cur & 0xff000000);

        lr = r;
        lg = g;
        lb = b;
        cur = next;
        r = rr;
        g = rg;
        b = rb;
        if (x + 2 <= lastX) {
          next = src32[rowOff + x + 2];
          rr = next & 0xff;
          rg = (next >>> 8) & 0xff;
          rb = (next >>> 16) & 0xff;
        }
      }
    }

    return { width: w, height: h, rgba: out };
  }

  for (let y = 0; y < h; y += 1) {
    const rowOff = y * w * 4;
    let r = rgba[rowOff];
    let g = rgba[rowOff + 1];
    let b = rgba[rowOff + 2];
    let lr = r;
    let lg = g;
    let lb = b;
    let rr = w > 1 ? rgba[rowOff + 4] : r;
    let rg = w > 1 ? rgba[rowOff + 5] : g;
    let rb = w > 1 ? rgba[rowOff + 6] : b;

    for (let x = 0; x < w; x += 1) {
      const idx = rowOff + x * 4;
      const luma = (r * 77 + g * 150 + b * 29) >> 8;
      const boost = luma > 140 ? (luma - 140) >> 2 : 0;

      out[idx] = r + ((lr + rr) >> 3) + boost + 4;
      out[idx + 1] = g + ((lg + rg) >> 3) + boost + 2;
      out[idx + 2] = b + ((lb + rb) >> 3) + (boost >> 1);
      out[idx + 3] = rgba[idx + 3];

      lr = r;
      lg = g;
      lb = b;
      r = rr;
      g = rg;
      b = rb;
      if (x + 2 <= lastX) {
        const nextIdx = idx + 8;
        rr = rgba[nextIdx];
        rg = rgba[nextIdx + 1];
        rb = rgba[nextIdx + 2];
      }
    }
  }

  return { width: w, height: h, rgba: out };
}

export const NDS_LAYOUT_MODES = [
  "vertical",
  "hybrid",
  "horizontal",
  "vertical-swapped",
  "horizontal-swapped",
];

/**
 * Transforms a frame `{ width, height, rgba }` according to the active upscaler mode.
 */
export function upscaleFrameIfEnabled(frame, mode = "OFF") {
  if (!frame || !frame.rgba) {
    return frame;
  }
  const normalized = String(mode || "OFF").toUpperCase();
  if (normalized === "SCALE2X") {
    return applyScale2xRgba(frame.rgba, frame.width, frame.height);
  }
  if (normalized === "XBRZ") {
    return applyXbrz2xRgba(frame.rgba, frame.width, frame.height);
  }
  if (normalized === "BLOOM") {
    return applyCrtPhosphorBloomRgba(frame.rgba, frame.width, frame.height);
  }
  return frame;
}

/**
 * Rearranges a vertically stacked Nintendo DS dual-screen RGBA frame `(W, H)`
 * (top screen `0..H/2`, touchscreen `H/2..H`) according to the chosen layout:
 * - `"vertical"` -> `(W, H)` unchanged
 * - `"vertical-swapped"` -> `(W, H)` with touchscreen on top, main screen below
 * - `"horizontal"` -> `(2*W, H/2)` with main screen left, touchscreen right
 * - `"horizontal-swapped"` -> `(2*W, H/2)` with touchscreen left, main screen right
 * - `"hybrid"` -> `(3*W, H)` with 2x main screen on left `(0..2*W, 0..H)` and 1x touchscreen centered on right `(2*W..3*W, H/4..3*H/4)`
 */
export function arrangeNdsDualScreenFrame(frame, layout = "vertical") {
  if (!frame || !frame.rgba) {
    return frame;
  }
  const mode = String(layout || "vertical").toLowerCase();
  if (mode === "vertical") {
    return frame;
  }
  const w = Math.floor(Number(frame.width) || 0);
  const h = Math.floor(Number(frame.height) || 0);
  if (w <= 0 || h < 2 || (h & 1) !== 0) {
    return frame;
  }
  const halfH = h >> 1;
  const rowBytes = w * 4;
  const halfBytes = halfH * rowBytes;
  const src = frame.rgba;
  if (src.length < halfBytes * 2) {
    return frame;
  }

  if (mode === "vertical-swapped") {
    const out = new Uint8ClampedArray(w * h * 4);
    out.set(src.subarray(halfBytes, halfBytes * 2), 0);
    out.set(src.subarray(0, halfBytes), halfBytes);
    return { width: w, height: h, rgba: out };
  }

  if (mode === "horizontal" || mode === "horizontal-swapped") {
    const outW = w * 2;
    const outH = halfH;
    const outRowBytes = outW * 4;
    const out = new Uint8ClampedArray(outW * outH * 4);
    const swapped = mode === "horizontal-swapped";

    for (let y = 0; y < halfH; y += 1) {
      const topOff = y * rowBytes;
      const botOff = (y + halfH) * rowBytes;
      const dstOff = y * outRowBytes;
      if (swapped) {
        out.set(src.subarray(botOff, botOff + rowBytes), dstOff);
        out.set(src.subarray(topOff, topOff + rowBytes), dstOff + rowBytes);
      } else {
        out.set(src.subarray(topOff, topOff + rowBytes), dstOff);
        out.set(src.subarray(botOff, botOff + rowBytes), dstOff + rowBytes);
      }
    }
    return { width: outW, height: outH, rgba: out };
  }

  if (mode === "hybrid") {
    const outW = w * 3;
    const outH = h;
    const out = new Uint8ClampedArray(outW * outH * 4);
    // Fill background with obsidian (#090c10)
    for (let i = 0; i < out.length; i += 4) {
      out[i] = 9;
      out[i + 1] = 12;
      out[i + 2] = 16;
      out[i + 3] = 255;
    }
    // Left 2W x H: 2x nearest-neighbor scaled top screen
    for (let y = 0; y < halfH; y += 1) {
      const srcRow = y * rowBytes;
      const dstRow0 = y * 2 * outW * 4;
      const dstRow1 = (y * 2 + 1) * outW * 4;
      for (let x = 0; x < w; x += 1) {
        const sIdx = srcRow + x * 4;
        const r = src[sIdx];
        const g = src[sIdx + 1];
        const b = src[sIdx + 2];
        const a = src[sIdx + 3];
        const d0 = dstRow0 + x * 8;
        const d1 = dstRow1 + x * 8;
        out[d0] = r;
        out[d0 + 1] = g;
        out[d0 + 2] = b;
        out[d0 + 3] = a;
        out[d0 + 4] = r;
        out[d0 + 5] = g;
        out[d0 + 6] = b;
        out[d0 + 7] = a;
        out[d1] = r;
        out[d1 + 1] = g;
        out[d1 + 2] = b;
        out[d1 + 3] = a;
        out[d1 + 4] = r;
        out[d1 + 5] = g;
        out[d1 + 6] = b;
        out[d1 + 7] = a;
      }
    }
    // Right W x halfH centered vertically at yOffset = floor(halfH / 2)
    const yOffset = halfH >> 1;
    for (let y = 0; y < halfH; y += 1) {
      const botOff = (y + halfH) * rowBytes;
      const dstOff = ((yOffset + y) * outW + w * 2) * 4;
      out.set(src.subarray(botOff, botOff + rowBytes), dstOff);
    }
    return { width: outW, height: outH, rgba: out };
  }

  return frame;
}
