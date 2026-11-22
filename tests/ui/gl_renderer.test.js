import test from "node:test";
import assert from "node:assert/strict";
import {
  UPSCALER_ID,
  SHADER_ID,
  NDS_LAYOUT_ID,
  resolveUpscalerId,
  resolveShaderId,
  resolveNdsLayoutId,
  computeAspectRatioForViewport,
  computeIntegerScaleDimensions,
  VERTEX_SHADER_GLSL,
  FRAGMENT_SHADER_GLSL,
  WebGlViewportRenderer,
} from "../../ui/gl_renderer.js";

test("resolveUpscalerId, resolveShaderId, and resolveNdsLayoutId map all GPU pipeline modes", () => {
  assert.equal(resolveUpscalerId("OFF"), UPSCALER_ID.OFF);
  assert.equal(resolveUpscalerId("SHARP"), UPSCALER_ID.SHARP);
  assert.equal(resolveUpscalerId("SCALE2X"), UPSCALER_ID.SCALE2X);
  assert.equal(resolveUpscalerId("XBRZ"), UPSCALER_ID.XBRZ);
  assert.equal(resolveUpscalerId("BLOOM"), UPSCALER_ID.BLOOM);
  assert.equal(resolveUpscalerId("unknown"), UPSCALER_ID.OFF);

  assert.equal(resolveShaderId("mode-clean"), SHADER_ID["mode-clean"]);
  assert.equal(resolveShaderId("mode-lcd"), SHADER_ID["mode-lcd"]);
  assert.equal(resolveShaderId("mode-crt"), SHADER_ID["mode-crt"]);
  assert.equal(resolveShaderId("mode-trinitron"), SHADER_ID["mode-trinitron"]);
  assert.equal(resolveShaderId("mode-dmg"), SHADER_ID["mode-dmg"]);

  assert.equal(resolveNdsLayoutId(false, "hybrid"), NDS_LAYOUT_ID.single);
  assert.equal(resolveNdsLayoutId(true, "vertical"), NDS_LAYOUT_ID.vertical);
  assert.equal(resolveNdsLayoutId(true, "vertical-swapped"), NDS_LAYOUT_ID["vertical-swapped"]);
  assert.equal(resolveNdsLayoutId(true, "horizontal"), NDS_LAYOUT_ID.horizontal);
  assert.equal(resolveNdsLayoutId(true, "horizontal-swapped"), NDS_LAYOUT_ID["horizontal-swapped"]);
  assert.equal(resolveNdsLayoutId(true, "hybrid"), NDS_LAYOUT_ID.hybrid);
});

test("computeAspectRatioForViewport returns exact hardware aspect ratios for GB/GBC, GBA, and NDS layouts", () => {
  const gb = computeAspectRatioForViewport("GB");
  assert.equal(gb.widthUnit, 10);
  assert.equal(gb.heightUnit, 9);
  assert.equal(gb.baseWidth, 160);
  assert.equal(gb.baseHeight, 144);

  const gba = computeAspectRatioForViewport("GBA");
  assert.equal(gba.widthUnit, 3);
  assert.equal(gba.heightUnit, 2);
  assert.equal(gba.baseWidth, 240);
  assert.equal(gba.baseHeight, 160);

  const ndsV = computeAspectRatioForViewport("NDS", "vertical");
  assert.equal(ndsV.widthUnit, 2);
  assert.equal(ndsV.heightUnit, 3);

  const ndsH = computeAspectRatioForViewport("NDS", "horizontal");
  assert.equal(ndsH.widthUnit, 8);
  assert.equal(ndsH.heightUnit, 3);

  const ndsHybrid = computeAspectRatioForViewport("NDS", "hybrid");
  assert.equal(ndsHybrid.widthUnit, 2);
  assert.equal(ndsHybrid.heightUnit, 1);
  assert.equal(ndsHybrid.baseWidth, 768);
  assert.equal(ndsHybrid.baseHeight, 384);
});

test("computeIntegerScaleDimensions computes exact integer multiples of native resolution", () => {
  const fitGba = computeIntegerScaleDimensions(1000, 700, 240, 160);
  assert.equal(fitGba.scale, 4);
  assert.equal(fitGba.width, 960);
  assert.equal(fitGba.height, 640);

  const fitGb = computeIntegerScaleDimensions(900, 800, 160, 144);
  assert.equal(fitGb.scale, 5);
  assert.equal(fitGb.width, 800);
  assert.equal(fitGb.height, 720);
});

test("WebGL2 GLSL ES 3.00 shaders include all upscalers, display shaders, and NDS layouts", () => {
  assert.ok(VERTEX_SHADER_GLSL.startsWith("#version 300 es"));
  assert.ok(FRAGMENT_SHADER_GLSL.startsWith("#version 300 es"));
  assert.ok(FRAGMENT_SHADER_GLSL.includes("sampleXbrz"));
  assert.ok(FRAGMENT_SHADER_GLSL.includes("sampleSharpBilinear"));
  assert.ok(FRAGMENT_SHADER_GLSL.includes("sampleScale2x"));
  assert.ok(FRAGMENT_SHADER_GLSL.includes("sampleBloom"));

  // Headless fallback when canvas has no WebGL2 context
  const renderer = new WebGlViewportRenderer(null);
  assert.equal(renderer.isSupported(), false);
  assert.equal(renderer.renderFrame({ width: 160, height: 144, rgba: new Uint8Array(160 * 144 * 4) }), false);
});
