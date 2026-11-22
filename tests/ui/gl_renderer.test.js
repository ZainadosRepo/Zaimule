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

test("WebGlViewportRenderer uses preserveDrawingBuffer:false and uploads RGB565 and RGBA textures cleanly", () => {
  const calls = [];
  let contextOpts = null;
  const mockGl = {
    VERTEX_SHADER: 1,
    FRAGMENT_SHADER: 2,
    COMPILE_STATUS: 3,
    LINK_STATUS: 4,
    TEXTURE_2D: 5,
    TEXTURE_MIN_FILTER: 6,
    TEXTURE_MAG_FILTER: 7,
    TEXTURE_WRAP_S: 8,
    TEXTURE_WRAP_T: 9,
    NEAREST: 10,
    CLAMP_TO_EDGE: 11,
    UNPACK_ALIGNMENT: 12,
    RGBA8: 13,
    RGBA: 14,
    UNSIGNED_BYTE: 15,
    RGB565: 16,
    RGB: 17,
    UNSIGNED_SHORT_5_6_5: 18,
    TEXTURE0: 19,
    TRIANGLES: 20,
    createShader: () => ({}),
    shaderSource: () => {},
    compileShader: () => {},
    getShaderParameter: () => true,
    deleteShader: () => {},
    createProgram: () => ({}),
    attachShader: () => {},
    linkProgram: () => {},
    getProgramParameter: () => true,
    deleteProgram: () => {},
    createVertexArray: () => ({}),
    createTexture: () => ({}),
    bindTexture: () => {},
    texParameteri: () => {},
    getUniformLocation: (_p, name) => name,
    pixelStorei: (pname, val) => calls.push(["pixelStorei", pname, val]),
    texImage2D: (_t, _l, internalFmt, w, h, _b, fmt, type, data) =>
      calls.push(["texImage2D", internalFmt, w, h, fmt, type, data.length]),
    texSubImage2D: (_t, _l, _x, _y, w, h, fmt, type, data) =>
      calls.push(["texSubImage2D", w, h, fmt, type, data.length]),
    viewport: () => {},
    useProgram: () => {},
    bindVertexArray: () => {},
    activeTexture: () => {},
    uniform1i: () => {},
    uniform2f: () => {},
    drawArrays: () => calls.push(["drawArrays"])
  };
  const mockCanvas = {
    width: 0,
    height: 0,
    clientWidth: 512,
    clientHeight: 768,
    getContext: (kind, opts) => {
      assert.equal(kind, "webgl2");
      contextOpts = opts;
      return mockGl;
    }
  };

  const renderer = new WebGlViewportRenderer(mockCanvas);
  assert.equal(renderer.isSupported(), true);
  assert.equal(contextOpts.preserveDrawingBuffer, false);

  const rgb565 = new Uint16Array(4).fill(0xf800);
  assert.equal(renderer.renderFrame({ width: 2, height: 2, pixelFormat: "rgb565", rgb565 }), true);
  assert.equal(renderer.renderFrame({ width: 2, height: 2, pixelFormat: "rgb565", rgb565 }), true);
  assert.equal(renderer.renderFrame({ width: 2, height: 2, pixelFormat: "rgba", rgba: new Uint8Array(16) }), true);

  assert.deepEqual(
    calls.filter(([op]) => op !== "drawArrays"),
    [
      ["pixelStorei", mockGl.UNPACK_ALIGNMENT, 4],
      ["pixelStorei", mockGl.UNPACK_ALIGNMENT, 2],
      ["texImage2D", mockGl.RGB565, 2, 2, mockGl.RGB, mockGl.UNSIGNED_SHORT_5_6_5, 4],
      ["texSubImage2D", 2, 2, mockGl.RGB, mockGl.UNSIGNED_SHORT_5_6_5, 4],
      ["pixelStorei", mockGl.UNPACK_ALIGNMENT, 4],
      ["texImage2D", mockGl.RGBA8, 2, 2, mockGl.RGBA, mockGl.UNSIGNED_BYTE, 16]
    ]
  );
});
