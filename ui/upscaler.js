// Zaimule — Real-Time Pixel-Art Upscaler & CRT Phosphor Bloom Pipeline
// Pure ES module testable under Node.js (`node --test`) and browser WebView.

export const UPSCALER_MODES = ["OFF", "SCALE2X", "XBRZ", "BLOOM"];

function samePixel(rgba, idxA, idxB) {
  return (
    rgba[idxA] === rgba[idxB] &&
    rgba[idxA + 1] === rgba[idxB + 1] &&
    rgba[idxA + 2] === rgba[idxB + 2] &&
    rgba[idxA + 3] === rgba[idxB + 3]
  );
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

  for (let y = 0; y < h; y += 1) {
    const yUp = y > 0 ? y - 1 : y;
    const yDown = y + 1 < h ? y + 1 : y;

    for (let x = 0; x < w; x += 1) {
      const xLeft = x > 0 ? x - 1 : x;
      const xRight = x + 1 < w ? x + 1 : x;

      const p = (y * w + x) * 4;
      const b = (yUp * w + x) * 4; // Top (B)
      const hIdx = (yDown * w + x) * 4; // Bottom (H)
      const d = (y * w + xLeft) * 4; // Left (D)
      const f = (y * w + xRight) * 4; // Right (F)

      let e0 = p;
      let e1 = p;
      let e2 = p;
      let e3 = p;

      if (!samePixel(rgba, b, hIdx) && !samePixel(rgba, d, f)) {
        if (samePixel(rgba, d, b)) e0 = d;
        if (samePixel(rgba, b, f)) e1 = f;
        if (samePixel(rgba, d, hIdx)) e2 = d;
        if (samePixel(rgba, hIdx, f)) e3 = f;
      }

      const dstTopRow = y * 2 * outW + x * 2;
      const dstBotRow = (y * 2 + 1) * outW + x * 2;

      copyPixel(rgba, e0, out, dstTopRow * 4);
      copyPixel(rgba, e1, out, (dstTopRow + 1) * 4);
      copyPixel(rgba, e2, out, dstBotRow * 4);
      copyPixel(rgba, e3, out, (dstBotRow + 1) * 4);
    }
  }

  return { width: outW, height: outH, rgba: out };
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

      const vertDiff = yuvDistance(rgba, b, hIdx) > 16;
      const horizDiff = yuvDistance(rgba, d, f) > 16;

      if (vertDiff && horizDiff) {
        if (yuvDistance(rgba, d, b) < 32 && yuvDistance(rgba, p, d) > 20) {
          blendPixel(rgba, p, d, 192, out, dstTopRow * 4);
        }
        if (yuvDistance(rgba, b, f) < 32 && yuvDistance(rgba, p, f) > 20) {
          blendPixel(rgba, p, f, 192, out, (dstTopRow + 1) * 4);
        }
        if (yuvDistance(rgba, d, hIdx) < 32 && yuvDistance(rgba, p, d) > 20) {
          blendPixel(rgba, p, d, 192, out, dstBotRow * 4);
        }
        if (yuvDistance(rgba, hIdx, f) < 32 && yuvDistance(rgba, p, f) > 20) {
          blendPixel(rgba, p, f, 192, out, (dstBotRow + 1) * 4);
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

  for (let y = 0; y < h; y += 1) {
    for (let x = 0; x < w; x += 1) {
      const idx = (y * w + x) * 4;
      const leftIdx = (y * w + (x > 0 ? x - 1 : x)) * 4;
      const rightIdx = (y * w + (x + 1 < w ? x + 1 : x)) * 4;

      const r = rgba[idx];
      const g = rgba[idx + 1];
      const b = rgba[idx + 2];

      const neighborGlowR = (rgba[leftIdx] + rgba[rightIdx]) >> 3;
      const neighborGlowG = (rgba[leftIdx + 1] + rgba[rightIdx + 1]) >> 3;
      const neighborGlowB = (rgba[leftIdx + 2] + rgba[rightIdx + 2]) >> 3;

      const luma = (r * 77 + g * 150 + b * 29) >> 8;
      const boost = luma > 140 ? (luma - 140) >> 2 : 0;

      out[idx] = Math.min(255, r + neighborGlowR + boost + 4);
      out[idx + 1] = Math.min(255, g + neighborGlowG + boost + 2);
      out[idx + 2] = Math.min(255, b + neighborGlowB + (boost >> 1));
      out[idx + 3] = rgba[idx + 3];
    }
  }

  return { width: w, height: h, rgba: out };
}

export const NDS_LAYOUT_MODES = [
  "vertical",
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

  return frame;
}
