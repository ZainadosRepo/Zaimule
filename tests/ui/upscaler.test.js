import test from "node:test";
import assert from "node:assert/strict";
import {
  UPSCALER_MODES,
  NDS_LAYOUT_MODES,
  applyScale2xRgba,
  applyXbrz2xRgba,
  applyCrtPhosphorBloomRgba,
  upscaleFrameIfEnabled,
  arrangeNdsDualScreenFrame,
} from "../../ui/upscaler.js";

test("applyScale2xRgba doubles dimensions and smooths diagonal pixel edges", () => {
  // 3x3 test grid with a diagonal white edge on top-left
  const width = 3;
  const height = 3;
  const rgba = new Uint8ClampedArray(width * height * 4);
  const setPx = (x, y, r, g, b) => {
    const idx = (y * width + x) * 4;
    rgba[idx] = r;
    rgba[idx + 1] = g;
    rgba[idx + 2] = b;
    rgba[idx + 3] = 255;
  };
  // Top & Left neighbors of center (1,1) are gold (245, 166, 35)
  setPx(1, 0, 245, 166, 35); // B (top)
  setPx(0, 1, 245, 166, 35); // D (left)
  setPx(1, 1, 16, 20, 32); // P (center dark)
  setPx(2, 1, 16, 20, 32); // F (right dark)
  setPx(1, 2, 16, 20, 32); // H (bottom dark)

  const upscaled = applyScale2xRgba(rgba, width, height);
  assert.equal(upscaled.width, 6);
  assert.equal(upscaled.height, 6);
  assert.equal(upscaled.rgba.length, 6 * 6 * 4);

  // Center pixel (1,1) maps to 2x2 block starting at (2,2) in 6x6 output.
  // Because D == B and B != H and D != F, top-left subpixel E0 at (2,2) takes D's gold color!
  const e0Idx = (2 * 6 + 2) * 4;
  assert.equal(upscaled.rgba[e0Idx], 245);
  assert.equal(upscaled.rgba[e0Idx + 1], 166);
  assert.equal(upscaled.rgba[e0Idx + 2], 35);
});

test("applyXbrz2xRgba blends diagonal edge corners using YUV luma distance", () => {
  const width = 3;
  const height = 3;
  const rgba = new Uint8ClampedArray(width * height * 4);
  const setPx = (x, y, r, g, b) => {
    const idx = (y * width + x) * 4;
    rgba[idx] = r;
    rgba[idx + 1] = g;
    rgba[idx + 2] = b;
    rgba[idx + 3] = 255;
  };
  setPx(1, 0, 240, 160, 40);
  setPx(0, 1, 232, 155, 38);
  setPx(1, 1, 16, 20, 32);
  setPx(2, 1, 16, 20, 32);
  setPx(1, 2, 16, 20, 32);

  const xbrz = applyXbrz2xRgba(rgba, width, height);
  assert.equal(xbrz.width, 6);
  assert.equal(xbrz.height, 6);
  const e0Idx = (2 * 6 + 2) * 4;
  assert.ok(xbrz.rgba[e0Idx] > 150);
});

test("applyCrtPhosphorBloomRgba boosts bright phosphor pixels while keeping dimensions", () => {
  const rgba = new Uint8ClampedArray([
    200, 180, 100, 255,
    10, 20, 30, 255,
  ]);
  const bloomed = applyCrtPhosphorBloomRgba(rgba, 2, 1);
  assert.equal(bloomed.width, 2);
  assert.equal(bloomed.height, 1);
  assert.ok(bloomed.rgba[0] > 200);
  assert.ok(bloomed.rgba[4] > 10); // Neighbor glow bleed received from bright left pixel
});

test("upscaleFrameIfEnabled dispatches OFF, SCALE2X, XBRZ, and BLOOM modes cleanly", () => {
  assert.deepEqual(UPSCALER_MODES, ["OFF", "SCALE2X", "XBRZ", "BLOOM"]);
  const baseFrame = {
    width: 2,
    height: 2,
    rgba: new Uint8ClampedArray(2 * 2 * 4).fill(128),
  };

  const off = upscaleFrameIfEnabled(baseFrame, "OFF");
  assert.equal(off.width, 2);

  const scale2x = upscaleFrameIfEnabled(baseFrame, "SCALE2X");
  assert.equal(scale2x.width, 4);
  assert.equal(scale2x.height, 4);

  const xbrz = upscaleFrameIfEnabled(baseFrame, "XBRZ");
  assert.equal(xbrz.width, 4);
  assert.equal(xbrz.height, 4);

  const bloom = upscaleFrameIfEnabled(baseFrame, "BLOOM");
  assert.equal(bloom.width, 2);
  assert.equal(bloom.height, 2);
});

test("arrangeNdsDualScreenFrame rearranges top/touch halves across all 4 layouts", () => {
  assert.deepEqual(NDS_LAYOUT_MODES, [
    "vertical",
    "horizontal",
    "vertical-swapped",
    "horizontal-swapped",
  ]);
  // 2x4 frame: rows 0..1 = top screen (red), rows 2..3 = touch screen (blue)
  const w = 2;
  const h = 4;
  const rgba = new Uint8ClampedArray(w * h * 4);
  for (let y = 0; y < h; y++) {
    for (let x = 0; x < w; x++) {
      const idx = (y * w + x) * 4;
      rgba[idx] = y < 2 ? 220 : 10;
      rgba[idx + 1] = 20;
      rgba[idx + 2] = y < 2 ? 10 : 240;
      rgba[idx + 3] = 255;
    }
  }
  const frame = { width: w, height: h, rgba };

  // Vertical -> identity
  const vert = arrangeNdsDualScreenFrame(frame, "vertical");
  assert.equal(vert, frame);

  // Vertical-swapped -> blue on top (y=0), red on bottom (y=2)
  const vSwap = arrangeNdsDualScreenFrame(frame, "vertical-swapped");
  assert.equal(vSwap.width, 2);
  assert.equal(vSwap.height, 4);
  assert.equal(vSwap.rgba[2], 240);
  assert.equal(vSwap.rgba[(2 * 2) * 4], 220);

  // Horizontal -> 4x2: red on left (x=0..1), blue on right (x=2..3)
  const horiz = arrangeNdsDualScreenFrame(frame, "horizontal");
  assert.equal(horiz.width, 4);
  assert.equal(horiz.height, 2);
  assert.equal(horiz.rgba[0], 220);
  assert.equal(horiz.rgba[2 * 4 + 2], 240);

  // Horizontal-swapped -> 4x2: blue on left (x=0..1), red on right (x=2..3)
  const hSwap = arrangeNdsDualScreenFrame(frame, "horizontal-swapped");
  assert.equal(hSwap.width, 4);
  assert.equal(hSwap.height, 2);
  assert.equal(hSwap.rgba[2], 240);
  assert.equal(hSwap.rgba[2 * 4], 220);
});
