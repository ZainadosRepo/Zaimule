// Zaimule — Hardware-Accelerated WebGL2 Viewport Renderer & Retro Shader Pipeline
// Eliminates CPU upscaling allocations, NDS layout buffer copies, and Canvas2D putImageData stalls.
// Pure ES module compatible with Node.js (`node --test`) and WebView2 / WebKit2GTK / WKWebView.

export const UPSCALER_ID = Object.freeze({
  OFF: 0,
  SHARP: 1,
  SCALE2X: 2,
  XBRZ: 3,
  BLOOM: 4,
});

export const SHADER_ID = Object.freeze({
  "mode-clean": 0,
  "mode-lcd": 1,
  "mode-crt": 2,
  "mode-trinitron": 3,
  "mode-dmg": 4,
});

export const NDS_LAYOUT_ID = Object.freeze({
  single: 0,
  vertical: 1,
  "vertical-swapped": 2,
  horizontal: 3,
  "horizontal-swapped": 4,
  hybrid: 5,
});

export function resolveUpscalerId(mode) {
  const key = String(mode || "OFF").trim().toUpperCase();
  return UPSCALER_ID[key] ?? UPSCALER_ID.OFF;
}

export function resolveShaderId(shaderCss) {
  const key = String(shaderCss || "mode-clean").trim().toLowerCase();
  return SHADER_ID[key] ?? SHADER_ID["mode-clean"];
}

export function resolveNdsLayoutId(isNds, ndsLayout = "vertical") {
  if (!isNds) return NDS_LAYOUT_ID.single;
  const key = String(ndsLayout || "vertical").trim().toLowerCase();
  return NDS_LAYOUT_ID[key] ?? NDS_LAYOUT_ID.vertical;
}

/**
 * Returns the true hardware aspect ratio for a console and NDS dual-screen layout.
 * - GB / GBC: 160×144 -> 10:9
 * - GBA: 240×160 -> 3:2
 * - NDS vertical / vertical-swapped: 256×384 -> 2:3
 * - NDS horizontal / horizontal-swapped: 512×192 -> 8:3
 * - NDS hybrid (2x main screen left + 1x touch right): 768×384 -> 2:1
 */
export function computeAspectRatioForViewport(consoleKind, ndsLayout = "vertical") {
  const kind = String(consoleKind || "GBC").trim().toUpperCase();
  if (kind === "GBA") {
    return { widthUnit: 3, heightUnit: 2, ratio: 1.5, css: "3 / 2", baseWidth: 240, baseHeight: 160 };
  }
  if (kind === "NDS") {
    const layout = String(ndsLayout || "vertical").trim().toLowerCase();
    if (layout === "horizontal" || layout === "horizontal-swapped") {
      return { widthUnit: 8, heightUnit: 3, ratio: 8 / 3, css: "8 / 3", baseWidth: 512, baseHeight: 192 };
    }
    if (layout === "hybrid") {
      return { widthUnit: 2, heightUnit: 1, ratio: 2.0, css: "2 / 1", baseWidth: 768, baseHeight: 384 };
    }
    return { widthUnit: 2, heightUnit: 3, ratio: 2 / 3, css: "2 / 3", baseWidth: 256, baseHeight: 384 };
  }
  return { widthUnit: 10, heightUnit: 9, ratio: 10 / 9, css: "10 / 9", baseWidth: 160, baseHeight: 144 };
}

/**
 * Computes the largest exact integer multiple of `(baseW, baseH)` that fits inside
 * `(containerW, containerH)`. Falls back to aspect-fit when the container is smaller than 1x.
 */
export function computeIntegerScaleDimensions(containerW, containerH, baseW, baseH) {
  const cw = Math.max(1, Math.floor(Number(containerW) || 1));
  const ch = Math.max(1, Math.floor(Number(containerH) || 1));
  const bw = Math.max(1, Math.floor(Number(baseW) || 1));
  const bh = Math.max(1, Math.floor(Number(baseH) || 1));
  const scale = Math.min(Math.floor(cw / bw), Math.floor(ch / bh));
  if (scale >= 1) {
    return { width: bw * scale, height: bh * scale, scale };
  }
  const floatScale = Math.min(cw / bw, ch / bh);
  return {
    width: Math.max(1, Math.round(bw * floatScale)),
    height: Math.max(1, Math.round(bh * floatScale)),
    scale: floatScale,
  };
}

export const VERTEX_SHADER_GLSL = `#version 300 es
precision highp float;
out vec2 v_uv;
void main() {
  // Full-screen triangle via gl_VertexID (0, 1, 2)
  vec2 pos = vec2(float((gl_VertexID << 1) & 2), float(gl_VertexID & 2));
  v_uv = vec2(pos.x, pos.y);
  gl_Position = vec4(pos * 2.0 - 1.0, 0.0, 1.0);
}
`;

export const FRAGMENT_SHADER_GLSL = `#version 300 es
precision highp float;

in vec2 v_uv;
out vec4 fragColor;

uniform sampler2D u_tex;
uniform vec2 u_srcSize;      // Full texture size (e.g. 256x384 or 512x768 for NDS)
uniform vec2 u_panelSrcSize; // Single panel texel size (e.g. 256x192 for NDS, or full size for GB/GBA)
uniform vec2 u_dstSize;      // Physical output framebuffer size in device pixels
uniform int u_upscaler;      // 0=OFF, 1=SHARP, 2=SCALE2X, 3=XBRZ, 4=BLOOM
uniform int u_shader;        // 0=Clean, 1=LCD, 2=CRT, 3=Trinitron, 4=DMG
uniform int u_ndsLayout;     // 0=Single, 1=Vert, 2=VertSwap, 3=Horiz, 4=HorizSwap, 5=Hybrid

float luma(vec3 c) {
  return dot(c, vec3(0.299, 0.587, 0.114));
}

float yuvDist(vec3 a, vec3 b) {
  vec3 d = abs(a - b);
  return dot(d, vec3(0.301, 0.586, 0.113));
}

// Samples a texel at integer panel coordinates (px, py) clamped strictly within
// the active screen panel [0..panelW-1, 0..panelH-1] so top/bottom NDS screens never bleed.
vec3 fetchPanelTexel(ivec2 p, ivec2 panelSize, int panelYOffset) {
  ivec2 clamped = clamp(p, ivec2(0), panelSize - ivec2(1));
  return texelFetch(u_tex, ivec2(clamped.x, clamped.y + panelYOffset), 0).rgb;
}

// Sharp-Bilinear (anti-shimmer pixel-art interpolation that preserves crisp pixel edges
// at any non-integer scale factor while eliminating uneven pixel widths).
vec3 sampleSharpBilinear(vec2 panelUv, vec2 panelSize, int panelYOffset, vec2 panelScreenPixels) {
  vec2 texelPos = panelUv * panelSize;
  vec2 flo = floor(texelPos - 0.5) + 0.5;
  vec2 f = texelPos - flo;
  vec2 scale = max(panelScreenPixels / panelSize, vec2(1.0));
  vec2 region = 0.5 - 0.5 / scale;
  vec2 dist = f - 0.5;
  vec2 s = clamp((dist - sign(dist) * region) * scale + 0.5, 0.0, 1.0);
  vec2 sharpPos = flo + s - 0.5;
  ivec2 p0 = ivec2(floor(sharpPos));
  vec2 frac = fract(sharpPos);
  ivec2 pSize = ivec2(panelSize);
  vec3 c00 = fetchPanelTexel(p0, pSize, panelYOffset);
  vec3 c10 = fetchPanelTexel(p0 + ivec2(1, 0), pSize, panelYOffset);
  vec3 c01 = fetchPanelTexel(p0 + ivec2(0, 1), pSize, panelYOffset);
  vec3 c11 = fetchPanelTexel(p0 + ivec2(1, 1), pSize, panelYOffset);
  return mix(mix(c00, c10, frac.x), mix(c01, c11, frac.x), frac.y);
}

// GPU Scale2x / EPX with smooth sub-texel anti-shimmering
vec3 sampleScale2x(vec2 panelUv, vec2 panelSize, int panelYOffset) {
  vec2 texelPos = panelUv * panelSize;
  ivec2 tc = ivec2(floor(texelPos));
  vec2 fp = fract(texelPos);
  ivec2 pSize = ivec2(panelSize);

  vec3 P = fetchPanelTexel(tc, pSize, panelYOffset);
  vec3 B = fetchPanelTexel(tc + ivec2(0, -1), pSize, panelYOffset);
  vec3 H = fetchPanelTexel(tc + ivec2(0, 1), pSize, panelYOffset);
  vec3 D = fetchPanelTexel(tc + ivec2(-1, 0), pSize, panelYOffset);
  vec3 F = fetchPanelTexel(tc + ivec2(1, 0), pSize, panelYOffset);

  if (yuvDist(B, H) > 0.03 && yuvDist(D, F) > 0.03) {
    if (fp.x < 0.5 && fp.y < 0.5 && yuvDist(D, B) < 0.04) return mix(P, D, 0.85);
    if (fp.x >= 0.5 && fp.y < 0.5 && yuvDist(B, F) < 0.04) return mix(P, F, 0.85);
    if (fp.x < 0.5 && fp.y >= 0.5 && yuvDist(D, H) < 0.04) return mix(P, D, 0.85);
    if (fp.x >= 0.5 && fp.y >= 0.5 && yuvDist(H, F) < 0.04) return mix(P, F, 0.85);
  }
  return P;
}

// GPU xBR-lv2 / xBRZ HD vector edge-directed pixel-art upscaler (5x5 neighborhood)
vec3 sampleXbrz(vec2 panelUv, vec2 panelSize, int panelYOffset) {
  vec2 texelPos = panelUv * panelSize;
  ivec2 tc = ivec2(floor(texelPos));
  vec2 fp = fract(texelPos);
  ivec2 pSize = ivec2(panelSize);

  // 3x3 core + extended 5x5 cardinal neighbors
  vec3 A = fetchPanelTexel(tc + ivec2(-1, -1), pSize, panelYOffset);
  vec3 B = fetchPanelTexel(tc + ivec2( 0, -1), pSize, panelYOffset);
  vec3 C = fetchPanelTexel(tc + ivec2( 1, -1), pSize, panelYOffset);
  vec3 D = fetchPanelTexel(tc + ivec2(-1,  0), pSize, panelYOffset);
  vec3 E = fetchPanelTexel(tc,                 pSize, panelYOffset);
  vec3 F = fetchPanelTexel(tc + ivec2( 1,  0), pSize, panelYOffset);
  vec3 G = fetchPanelTexel(tc + ivec2(-1,  1), pSize, panelYOffset);
  vec3 H = fetchPanelTexel(tc + ivec2( 0,  1), pSize, panelYOffset);
  vec3 I = fetchPanelTexel(tc + ivec2( 1,  1), pSize, panelYOffset);

  vec3 B0 = fetchPanelTexel(tc + ivec2( 0, -2), pSize, panelYOffset);
  vec3 D0 = fetchPanelTexel(tc + ivec2(-2,  0), pSize, panelYOffset);
  vec3 F4 = fetchPanelTexel(tc + ivec2( 2,  0), pSize, panelYOffset);
  vec3 H5 = fetchPanelTexel(tc + ivec2( 0,  2), pSize, panelYOffset);

  vec3 res = E;

  // Evaluate all 4 corners with weighted YUV gradient & shallow 2:1 / 1:2 slope detection
  // Top-Left corner (B & D)
  {
    float e_tl = yuvDist(E, A) + yuvDist(E, C) + yuvDist(E, G) + yuvDist(B, D0) + yuvDist(D, B0) + 4.0 * yuvDist(D, B);
    float i_tl = yuvDist(B, C) + yuvDist(B, B0) + yuvDist(D, G) + yuvDist(D, D0) + 4.0 * yuvDist(E, A);
    if (e_tl < i_tl && yuvDist(E, D) > 0.04 && yuvDist(E, B) > 0.04) {
      vec3 nc = yuvDist(E, B) <= yuvDist(E, D) ? B : D;
      float d_bd = yuvDist(B, D);
      if (d_bd < 0.18) {
        // Line equation in sub-pixel space: x + y < 0.75 (steep/45deg/shallow)
        float slope = 1.0;
        if (yuvDist(D, C) < 0.08 && yuvDist(B, C) < 0.08) slope = 1.5;
        float edge = 0.68 - (fp.x * slope + fp.y);
        float w = smoothstep(-0.18, 0.18, edge);
        res = mix(res, nc, w * 0.88);
      }
    }
  }
  // Top-Right corner (B & F)
  {
    float e_tr = yuvDist(E, C) + yuvDist(E, A) + yuvDist(E, I) + yuvDist(B, F4) + yuvDist(F, B0) + 4.0 * yuvDist(B, F);
    float i_tr = yuvDist(B, A) + yuvDist(B, B0) + yuvDist(F, I) + yuvDist(F, F4) + 4.0 * yuvDist(E, C);
    if (e_tr < i_tr && yuvDist(E, F) > 0.04 && yuvDist(E, B) > 0.04) {
      vec3 nc = yuvDist(E, B) <= yuvDist(E, F) ? B : F;
      if (yuvDist(B, F) < 0.18) {
        float slope = 1.0;
        if (yuvDist(F, A) < 0.08 && yuvDist(B, A) < 0.08) slope = 1.5;
        float edge = 0.68 - ((1.0 - fp.x) * slope + fp.y);
        float w = smoothstep(-0.18, 0.18, edge);
        res = mix(res, nc, w * 0.88);
      }
    }
  }
  // Bottom-Left corner (H & D)
  {
    float e_bl = yuvDist(E, G) + yuvDist(E, A) + yuvDist(E, I) + yuvDist(H, D0) + yuvDist(D, H5) + 4.0 * yuvDist(D, H);
    float i_bl = yuvDist(H, I) + yuvDist(H, H5) + yuvDist(D, A) + yuvDist(D, D0) + 4.0 * yuvDist(E, G);
    if (e_bl < i_bl && yuvDist(E, D) > 0.04 && yuvDist(E, H) > 0.04) {
      vec3 nc = yuvDist(E, H) <= yuvDist(E, D) ? H : D;
      if (yuvDist(D, H) < 0.18) {
        float slope = 1.0;
        if (yuvDist(D, I) < 0.08 && yuvDist(H, I) < 0.08) slope = 1.5;
        float edge = 0.68 - (fp.x * slope + (1.0 - fp.y));
        float w = smoothstep(-0.18, 0.18, edge);
        res = mix(res, nc, w * 0.88);
      }
    }
  }
  // Bottom-Right corner (H & F)
  {
    float e_br = yuvDist(E, I) + yuvDist(E, C) + yuvDist(E, G) + yuvDist(H, F4) + yuvDist(F, H5) + 4.0 * yuvDist(F, H);
    float i_br = yuvDist(H, G) + yuvDist(H, H5) + yuvDist(F, C) + yuvDist(F, F4) + 4.0 * yuvDist(E, I);
    if (e_br < i_br && yuvDist(E, F) > 0.04 && yuvDist(E, H) > 0.04) {
      vec3 nc = yuvDist(E, H) <= yuvDist(E, F) ? H : F;
      if (yuvDist(F, H) < 0.18) {
        float slope = 1.0;
        if (yuvDist(F, G) < 0.08 && yuvDist(H, G) < 0.08) slope = 1.5;
        float edge = 0.68 - ((1.0 - fp.x) * slope + (1.0 - fp.y));
        float w = smoothstep(-0.18, 0.18, edge);
        res = mix(res, nc, w * 0.88);
      }
    }
  }

  return res;
}

// GPU Phosphor Bloom + Sharp-Bilinear base
vec3 sampleBloom(vec2 panelUv, vec2 panelSize, int panelYOffset, vec2 panelScreenPixels) {
  vec3 base = sampleSharpBilinear(panelUv, panelSize, panelYOffset, panelScreenPixels);
  ivec2 tc = ivec2(floor(panelUv * panelSize));
  ivec2 pSize = ivec2(panelSize);
  vec3 glow = vec3(0.0);
  glow += fetchPanelTexel(tc + ivec2(-1,  0), pSize, panelYOffset) * 0.22;
  glow += fetchPanelTexel(tc + ivec2( 1,  0), pSize, panelYOffset) * 0.22;
  glow += fetchPanelTexel(tc + ivec2( 0, -1), pSize, panelYOffset) * 0.16;
  glow += fetchPanelTexel(tc + ivec2( 0,  1), pSize, panelYOffset) * 0.16;
  glow += fetchPanelTexel(tc + ivec2(-2,  0), pSize, panelYOffset) * 0.12;
  glow += fetchPanelTexel(tc + ivec2( 2,  0), pSize, panelYOffset) * 0.12;
  float glowLuma = luma(glow);
  float boost = max(0.0, glowLuma - 0.42) * 0.55;
  return clamp(base + glow * boost + vec3(0.015, 0.008, 0.002) * glowLuma, 0.0, 1.0);
}

void main() {
  // Flip Y because HTML/Canvas top-left is (0,0) while WebGL UV bottom-left is (0,0)
  vec2 uv = vec2(v_uv.x, 1.0 - v_uv.y);

  vec2 panelUv = uv;
  int panelIdx = 0; // 0 = top half of texture, 1 = bottom half of texture
  vec2 panelScreenPixels = u_dstSize;

  if (u_ndsLayout == 1 || u_ndsLayout == 2) {
    // Vertical or Vertical-Swapped
    float hingeDist = abs(uv.y - 0.5) * u_dstSize.y;
    if (hingeDist < 1.2) {
      fragColor = vec4(0.035, 0.047, 0.063, 1.0);
      return;
    }
    bool bottomHalf = uv.y >= 0.5;
    panelUv = vec2(uv.x, bottomHalf ? (uv.y - 0.5) * 2.0 : uv.y * 2.0);
    if (u_ndsLayout == 1) {
      panelIdx = bottomHalf ? 1 : 0;
    } else {
      panelIdx = bottomHalf ? 0 : 1;
    }
    panelScreenPixels = vec2(u_dstSize.x, u_dstSize.y * 0.5);
  } else if (u_ndsLayout == 3 || u_ndsLayout == 4) {
    // Horizontal or Horizontal-Swapped
    float hingeDist = abs(uv.x - 0.5) * u_dstSize.x;
    if (hingeDist < 1.2) {
      fragColor = vec4(0.035, 0.047, 0.063, 1.0);
      return;
    }
    bool rightHalf = uv.x >= 0.5;
    panelUv = vec2(rightHalf ? (uv.x - 0.5) * 2.0 : uv.x * 2.0, uv.y);
    if (u_ndsLayout == 3) {
      panelIdx = rightHalf ? 1 : 0;
    } else {
      panelIdx = rightHalf ? 0 : 1;
    }
    panelScreenPixels = vec2(u_dstSize.x * 0.5, u_dstSize.y);
  } else if (u_ndsLayout == 5) {
    // Hybrid Focus layout: Left 2/3 = Full-Height Main Screen, Right 1/3 (Y 0.25..0.75) = Touch Screen
    float splitX = 2.0 / 3.0;
    float hingeDist = abs(uv.x - splitX) * u_dstSize.x;
    if (hingeDist < 1.5) {
      fragColor = vec4(0.055, 0.067, 0.086, 1.0);
      return;
    }
    if (uv.x < splitX) {
      panelUv = vec2(uv.x / splitX, uv.y);
      panelIdx = 0;
      panelScreenPixels = vec2(u_dstSize.x * splitX, u_dstSize.y);
    } else {
      if (uv.y < 0.25 || uv.y > 0.75) {
        // Obsidian bezel around the right touch screen with a subtle warm hairline border
        float borderDist = min(abs(uv.y - 0.25), abs(uv.y - 0.75)) * u_dstSize.y;
        if (borderDist < 1.5) {
          fragColor = vec4(0.18, 0.15, 0.12, 1.0);
        } else {
          fragColor = vec4(0.035, 0.047, 0.063, 1.0);
        }
        return;
      }
      panelUv = vec2((uv.x - splitX) / (1.0 - splitX), (uv.y - 0.25) / 0.5);
      panelIdx = 1;
      panelScreenPixels = vec2(u_dstSize.x * (1.0 - splitX), u_dstSize.y * 0.5);
    }
  }

  // Optional CRT barrel curvature for Trinitron mode (u_shader == 3)
  if (u_shader == 3) {
    vec2 centered = panelUv * 2.0 - 1.0;
    vec2 offset = centered.yx * 0.16;
    centered = centered + centered * (offset * offset);
    if (abs(centered.x) > 1.0 || abs(centered.y) > 1.0) {
      fragColor = vec4(0.02, 0.025, 0.035, 1.0);
      return;
    }
    panelUv = centered * 0.5 + 0.5;
  }

  int panelH = int(u_panelSrcSize.y);
  int panelYOffset = panelIdx * panelH;

  vec3 col;
  if (u_upscaler == 3) {
    col = sampleXbrz(panelUv, u_panelSrcSize, panelYOffset);
  } else if (u_upscaler == 2) {
    col = sampleScale2x(panelUv, u_panelSrcSize, panelYOffset);
  } else if (u_upscaler == 4) {
    col = sampleBloom(panelUv, u_panelSrcSize, panelYOffset, panelScreenPixels);
  } else if (u_upscaler == 1) {
    col = sampleSharpBilinear(panelUv, u_panelSrcSize, panelYOffset, panelScreenPixels);
  } else {
    // OFF: exact integer nearest texel
    ivec2 tc = ivec2(floor(panelUv * u_panelSrcSize));
    col = fetchPanelTexel(tc, ivec2(u_panelSrcSize), panelYOffset);
  }

  // Apply hardware display shader in screen space / texel space
  if (u_shader == 1) {
    // Grille LCD Subpixel (authentic handheld RGB subpixel triad + fine inter-pixel grid)
    vec2 texelCoord = panelUv * u_panelSrcSize;
    vec2 sub = fract(texelCoord);
    float edgeX = smoothstep(0.0, 0.11, sub.x) * (1.0 - smoothstep(0.89, 1.0, sub.x));
    float edgeY = smoothstep(0.0, 0.11, sub.y) * (1.0 - smoothstep(0.89, 1.0, sub.y));
    float cellMask = mix(0.82, 1.0, edgeX * edgeY);

    // Subtle RGB subpixel triad tint across each source texel
    vec3 subTint = vec3(1.0);
    if (sub.x < 0.333) {
      subTint = vec3(1.06, 0.97, 0.97);
    } else if (sub.x < 0.666) {
      subTint = vec3(0.97, 1.05, 0.97);
    } else {
      subTint = vec3(0.97, 0.97, 1.06);
    }
    col = clamp(col * subTint * cellMask * 1.03, 0.0, 1.0);
  } else if (u_shader == 2) {
    // CRT Scanlines Pro (luminance-adaptive Gaussian scanlines locked to source scanline Y)
    float scanPhase = fract(panelUv.y * u_panelSrcSize.y);
    float distFromCenter = abs(scanPhase - 0.5) * 2.0;
    float lum = luma(col);
    // Bright pixels widen the electron beam so highlights bloom naturally through scanlines
    float beamWidth = mix(0.32, 0.14, lum);
    float scanWeight = 1.0 - beamWidth * (distFromCenter * distFromCenter);
    col = clamp(col * scanWeight * 1.08, 0.0, 1.0);
  } else if (u_shader == 3) {
    // Trinitron PVM (Sony aperture grille + beam scanlines + phosphor halation + corner vignette)
    float scanPhase = fract(panelUv.y * u_panelSrcSize.y);
    float distY = abs(scanPhase - 0.5) * 2.0;
    float lum = luma(col);
    float scanWeight = 1.0 - mix(0.38, 0.15, lum) * (distY * distY);

    // Vertical RGB aperture grille wires in physical screen pixels
    float triad = mod(gl_FragCoord.x, 3.0);
    vec3 mask = vec3(0.92);
    if (triad < 1.0) {
      mask = vec3(1.10, 0.93, 0.93);
    } else if (triad < 2.0) {
      mask = vec3(0.93, 1.09, 0.93);
    } else {
      mask = vec3(0.93, 0.93, 1.10);
    }

    // Corner vignette
    vec2 v = panelUv * (1.0 - panelUv.yx);
    float vig = clamp(pow(v.x * v.y * 18.0, 0.18), 0.72, 1.0);

    col = clamp(col * scanWeight * mask * vig * 1.12, 0.0, 1.0);
  } else if (u_shader == 4) {
    // DMG Olive Classic (4-shade olive-green reflective LCD palette + dot-matrix shadow)
    float lum = clamp(luma(col), 0.0, 1.0);
    vec3 c0 = vec3(0.059, 0.220, 0.059); // #0F380F darkest
    vec3 c1 = vec3(0.188, 0.384, 0.188); // #306230
    vec3 c2 = vec3(0.545, 0.675, 0.059); // #8BAC0F
    vec3 c3 = vec3(0.608, 0.737, 0.059); // #9BBC0F lightest
    vec3 olive;
    if (lum < 0.333) {
      olive = mix(c0, c1, lum * 3.0);
    } else if (lum < 0.666) {
      olive = mix(c1, c2, (lum - 0.333) * 3.0);
    } else {
      olive = mix(c2, c3, (lum - 0.666) * 3.0);
    }
    vec2 sub = fract(panelUv * u_panelSrcSize);
    float dotGrid = smoothstep(0.0, 0.12, sub.x) * smoothstep(0.0, 0.12, sub.y);
    col = mix(olive * 0.86, olive, dotGrid);
  }

  fragColor = vec4(col, 1.0);
}
`;

/**
 * Hardware-accelerated WebGL2 viewport renderer with automatic HiDPI sizing
 * and zero-copy NDS dual-screen layout composition.
 */
export class WebGlViewportRenderer {
  constructor(canvas) {
    this.canvas = canvas;
    this.gl = null;
    this.program = null;
    this.vao = null;
    this.texture = null;
    this.uniforms = {};
    this.texWidth = 0;
    this.texHeight = 0;
    this.texFormat = "rgba";
    this.unpackAlignment = 4;
    this.cachedClientW = 0;
    this.cachedClientH = 0;
    this._resizeObserver = null;
    this.lastFrame = null;
    this.lastOptions = {
      upscalerMode: "XBRZ",
      shaderCss: "mode-clean",
      isNds: false,
      ndsLayout: "vertical",
      integerScale: false,
    };
    this._initWebGl2();
    this._attachResizeObserver();
  }

  isSupported() {
    return Boolean(this.gl && this.program);
  }

  _attachResizeObserver() {
    if (!this.canvas || typeof ResizeObserver !== "function") {
      return;
    }
    try {
      this.cachedClientW = this.canvas.clientWidth || 0;
      this.cachedClientH = this.canvas.clientHeight || 0;
      const obs = new ResizeObserver((entries) => {
        for (const entry of entries) {
          const box = entry.contentRect;
          if (box && box.width > 0 && box.height > 0) {
            this.cachedClientW = Math.round(box.width);
            this.cachedClientH = Math.round(box.height);
          }
        }
      });
      obs.observe(this.canvas);
      this._resizeObserver = obs;
    } catch {
      this._resizeObserver = null;
    }
  }

  _initWebGl2() {
    if (!this.canvas || typeof this.canvas.getContext !== "function") {
      return;
    }
    let gl = null;
    try {
      gl = this.canvas.getContext("webgl2", {
        alpha: false,
        antialias: false,
        depth: false,
        stencil: false,
        preserveDrawingBuffer: false,
        powerPreference: "high-performance",
      });
    } catch {
      gl = null;
    }
    if (!gl) return;

    const vs = this._compileShader(gl, gl.VERTEX_SHADER, VERTEX_SHADER_GLSL);
    const fs = this._compileShader(gl, gl.FRAGMENT_SHADER, FRAGMENT_SHADER_GLSL);
    if (!vs || !fs) return;

    const program = gl.createProgram();
    gl.attachShader(program, vs);
    gl.attachShader(program, fs);
    gl.linkProgram(program);
    gl.deleteShader(vs);
    gl.deleteShader(fs);

    if (!gl.getProgramParameter(program, gl.LINK_STATUS)) {
      gl.deleteProgram(program);
      return;
    }

    const vao = gl.createVertexArray();
    const texture = gl.createTexture();
    gl.bindTexture(gl.TEXTURE_2D, texture);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, gl.NEAREST);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, gl.NEAREST);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE);
    gl.pixelStorei(gl.UNPACK_ALIGNMENT, 4);

    this.gl = gl;
    this.program = program;
    this.vao = vao;
    this.texture = texture;
    this.uniforms = {
      u_tex: gl.getUniformLocation(program, "u_tex"),
      u_srcSize: gl.getUniformLocation(program, "u_srcSize"),
      u_panelSrcSize: gl.getUniformLocation(program, "u_panelSrcSize"),
      u_dstSize: gl.getUniformLocation(program, "u_dstSize"),
      u_upscaler: gl.getUniformLocation(program, "u_upscaler"),
      u_shader: gl.getUniformLocation(program, "u_shader"),
      u_ndsLayout: gl.getUniformLocation(program, "u_ndsLayout"),
    };
  }

  _compileShader(gl, type, source) {
    const shader = gl.createShader(type);
    gl.shaderSource(shader, source);
    gl.compileShader(shader);
    if (!gl.getShaderParameter(shader, gl.COMPILE_STATUS)) {
      gl.deleteShader(shader);
      return null;
    }
    return shader;
  }

  /**
   * Synchronizes the WebGL drawingBuffer dimensions with the canvas CSS box * devicePixelRatio
   * so GPU shaders execute at native monitor resolution (1080p / 1440p / 4K).
   */
  _syncCanvasResolution(srcW, srcH, isNds, ndsLayout, integerScale) {
    const dpr = (typeof window !== "undefined" && window.devicePixelRatio) || 1;
    const rectW = this.cachedClientW || this.canvas.clientWidth || srcW;
    const rectH = this.cachedClientH || this.canvas.clientHeight || srcH;
    let targetW = Math.max(srcW, Math.round(rectW * dpr));
    let targetH = Math.max(srcH, Math.round(rectH * dpr));

    if (integerScale) {
      let baseW = srcW;
      let baseH = srcH;
      if (isNds) {
        const halfH = Math.max(1, srcH >> 1);
        const mode = String(ndsLayout || "vertical").toLowerCase();
        if (mode === "horizontal" || mode === "horizontal-swapped") {
          baseW = srcW * 2;
          baseH = halfH;
        } else if (mode === "hybrid") {
          baseW = srcW * 3;
          baseH = srcH;
        }
      }
      const fit = computeIntegerScaleDimensions(targetW, targetH, baseW, baseH);
      targetW = fit.width;
      targetH = fit.height;
    }

    if (this.canvas.width !== targetW || this.canvas.height !== targetH) {
      this.canvas.width = targetW;
      this.canvas.height = targetH;
    }
    return { dstW: targetW, dstH: targetH };
  }

  /**
   * Uploads and renders a frame `{ width, height, rgba }` or `{ width, height, pixelFormat: "rgb565", rgb565 }`
   * directly on the GPU. If `frame` is omitted, re-renders the most recently uploaded texture.
   */
  renderFrame(frame, options = {}) {
    if (!this.isSupported()) return false;
    const gl = this.gl;

    if (frame && frame.width > 0 && frame.height > 0 && (frame.rgb565 || frame.rgba)) {
      const w = frame.width | 0;
      const h = frame.height | 0;
      const isRgb565 = frame.pixelFormat === "rgb565" && frame.rgb565;

      if (isRgb565) {
        const expectedPixels = w * h;
        const u16 =
          frame.rgb565 instanceof Uint16Array
            ? frame.rgb565
            : new Uint16Array(frame.rgb565.buffer, frame.rgb565.byteOffset, frame.rgb565.byteLength >>> 1);
        if (u16.length < expectedPixels) {
          return false;
        }
        const uploadSlice = u16.length === expectedPixels ? u16 : u16.subarray(0, expectedPixels);

        if (this.unpackAlignment !== 2) {
          gl.pixelStorei(gl.UNPACK_ALIGNMENT, 2);
          this.unpackAlignment = 2;
        }
        gl.bindTexture(gl.TEXTURE_2D, this.texture);
        if (this.texWidth !== w || this.texHeight !== h || this.texFormat !== "rgb565") {
          this.texWidth = w;
          this.texHeight = h;
          this.texFormat = "rgb565";
          gl.texImage2D(
            gl.TEXTURE_2D,
            0,
            gl.RGB565,
            w,
            h,
            0,
            gl.RGB,
            gl.UNSIGNED_SHORT_5_6_5,
            uploadSlice
          );
        } else {
          gl.texSubImage2D(
            gl.TEXTURE_2D,
            0,
            0,
            0,
            w,
            h,
            gl.RGB,
            gl.UNSIGNED_SHORT_5_6_5,
            uploadSlice
          );
        }
      } else {
        const expectedBytes = w * h * 4;
        const raw = frame.rgba;
        const u8 =
          raw instanceof Uint8Array
            ? raw
            : new Uint8Array(raw.buffer, raw.byteOffset, raw.byteLength);
        if (u8.byteLength < expectedBytes) {
          return false;
        }
        const uploadSlice = u8.byteLength === expectedBytes ? u8 : u8.subarray(0, expectedBytes);

        if (this.unpackAlignment !== 4) {
          gl.pixelStorei(gl.UNPACK_ALIGNMENT, 4);
          this.unpackAlignment = 4;
        }
        gl.bindTexture(gl.TEXTURE_2D, this.texture);
        if (this.texWidth !== w || this.texHeight !== h || this.texFormat !== "rgba") {
          this.texWidth = w;
          this.texHeight = h;
          this.texFormat = "rgba";
          gl.texImage2D(
            gl.TEXTURE_2D,
            0,
            gl.RGBA8,
            w,
            h,
            0,
            gl.RGBA,
            gl.UNSIGNED_BYTE,
            uploadSlice
          );
        } else {
          gl.texSubImage2D(
            gl.TEXTURE_2D,
            0,
            0,
            0,
            w,
            h,
            gl.RGBA,
            gl.UNSIGNED_BYTE,
            uploadSlice
          );
        }
      }
      this.lastFrame = { width: w, height: h };
    }

    if (!this.lastFrame || this.texWidth <= 0 || this.texHeight <= 0) {
      return false;
    }

    this.lastOptions = { ...this.lastOptions, ...options };
    const { upscalerMode, shaderCss, isNds, ndsLayout, integerScale } = this.lastOptions;
    const { dstW, dstH } = this._syncCanvasResolution(
      this.texWidth,
      this.texHeight,
      Boolean(isNds),
      ndsLayout,
      Boolean(integerScale)
    );

    const panelW = this.texWidth;
    const panelH = isNds ? Math.max(1, this.texHeight >> 1) : this.texHeight;

    gl.viewport(0, 0, dstW, dstH);
    gl.useProgram(this.program);
    gl.bindVertexArray(this.vao);
    gl.activeTexture(gl.TEXTURE0);
    gl.bindTexture(gl.TEXTURE_2D, this.texture);

    gl.uniform1i(this.uniforms.u_tex, 0);
    gl.uniform2f(this.uniforms.u_srcSize, this.texWidth, this.texHeight);
    gl.uniform2f(this.uniforms.u_panelSrcSize, panelW, panelH);
    gl.uniform2f(this.uniforms.u_dstSize, dstW, dstH);
    gl.uniform1i(this.uniforms.u_upscaler, resolveUpscalerId(upscalerMode));
    gl.uniform1i(this.uniforms.u_shader, resolveShaderId(shaderCss));
    gl.uniform1i(this.uniforms.u_ndsLayout, resolveNdsLayoutId(Boolean(isNds), ndsLayout));

    gl.drawArrays(gl.TRIANGLES, 0, 3);
    return true;
  }

  redraw(options = {}) {
    return this.renderFrame(null, options);
  }
}
