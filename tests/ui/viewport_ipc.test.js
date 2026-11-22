import test from "node:test";
import assert from "node:assert/strict";
import { decodeViewportBinaryPacket } from "../../ui/viewport_ipc.js";
import { resampleInterleavedStereo } from "../../ui/audio.js";

function buildZmfrPacket({
  version = 1,
  flags = 0,
  sampleRateHz = 48000,
  frameNumber = 1,
  audioLevel = 0.5,
  audioFreqHz = 880,
  rewindSnapshotsCount = 12,
  rewindMemoryBytes = 65536,
  views = [{ width: 160, height: 144, rgba: new Uint8Array(160 * 144 * 4).fill(0x7f) }],
  pcmSamples = [],
  meta = { console: "GBC", cpu: { pcHex: "0x0150", score: 42 } }
} = {}) {
  const encoder = new TextEncoder();
  const metaBytes = meta === null ? new Uint8Array(0) : encoder.encode(JSON.stringify(meta));
  const viewsTotal = views.reduce((acc, v) => acc + 8 + v.rgba.byteLength, 0);
  const totalBytes = 35 + viewsTotal + 4 + pcmSamples.length * 4 + 4 + metaBytes.byteLength;
  const buf = new ArrayBuffer(totalBytes);
  const bytes = new Uint8Array(buf);
  const dv = new DataView(buf);

  bytes.set([0x5a, 0x4d, 0x46, 0x52], 0); // "ZMFR"
  bytes[4] = version;
  bytes[5] = flags;
  dv.setUint32(6, sampleRateHz, true);

  const lo = frameNumber >>> 0;
  const hi = Math.floor(frameNumber / 4294967296) >>> 0;
  dv.setUint32(10, lo, true);
  dv.setUint32(14, hi, true);

  dv.setFloat32(18, audioLevel, true);
  dv.setUint16(22, audioFreqHz, true);
  dv.setUint16(24, 0, true);
  dv.setUint32(26, rewindSnapshotsCount, true);
  dv.setUint32(30, rewindMemoryBytes, true);
  bytes[34] = views.length;

  let off = 35;
  for (const v of views) {
    dv.setUint16(off, v.width, true);
    dv.setUint16(off + 2, v.height, true);
    dv.setUint32(off + 4, v.rgba.byteLength, true);
    off += 8;
    bytes.set(v.rgba, off);
    off += v.rgba.byteLength;
  }

  dv.setUint32(off, pcmSamples.length, true);
  off += 4;
  for (let i = 0; i < pcmSamples.length; i += 1) {
    dv.setFloat32(off + i * 4, pcmSamples[i], true);
  }
  off += pcmSamples.length * 4;

  dv.setUint32(off, metaBytes.byteLength, true);
  off += 4;
  bytes.set(metaBytes, off);
  return buf;
}

test("decodeViewportBinaryPacket decodes single view with unaligned PCM offset cleanly", () => {
  // 35 (header) + 8 (view header) + 160*144*4 (RGBA) + 4 (floatCount) = 92207 (mod 4 === 3 -> unaligned!)
  const rgba = new Uint8Array(160 * 144 * 4);
  rgba[0] = 0x12;
  rgba[1] = 0x34;
  rgba[2] = 0x56;
  rgba[3] = 0xff;
  const expectedPcm = [0.25, -0.25, 0.75, -0.75, 0.125, -0.5];
  const pkt = buildZmfrPacket({
    flags: 0x04, // sramDirty
    frameNumber: 9001,
    audioLevel: 0.625,
    audioFreqHz: 440,
    views: [{ width: 160, height: 144, rgba }],
    pcmSamples: expectedPcm,
    meta: { console: "GBC", cpu: { pcHex: "0x0150", score: 100 } }
  });

  const decoded = decodeViewportBinaryPacket(pkt);
  assert.equal(decoded.version, 1);
  assert.equal(decoded.hasSecondary, false);
  assert.equal(decoded.quadMode, false);
  assert.equal(decoded.sramDirty, true);
  assert.equal(decoded.macroActive, false);
  assert.equal(decoded.sampleRateHz, 48000);
  assert.equal(decoded.frameNumber, 9001);
  assert.ok(Math.abs(decoded.audioLevel - 0.625) < 1e-6);
  assert.equal(decoded.audioFreqHz, 440);
  assert.equal(decoded.views.length, 1);
  assert.equal(decoded.views[0].width, 160);
  assert.equal(decoded.views[0].height, 144);
  assert.equal(decoded.views[0].rgba[0], 0x12);
  assert.equal(decoded.views[0].rgba[3], 0xff);
  assert.equal(decoded.pcmSamples.length, expectedPcm.length);
  for (let i = 0; i < expectedPcm.length; i += 1) {
    assert.ok(Math.abs(decoded.pcmSamples[i] - expectedPcm[i]) < 1e-6);
  }
  // Confirm returned PCM owns an isolated buffer suitable for Worklet transfer
  assert.equal(decoded.pcmSamples.buffer.byteLength, expectedPcm.length * 4);
  assert.notEqual(decoded.pcmSamples.buffer, pkt);
  assert.equal(decoded.meta.console, "GBC");
  assert.equal(decoded.meta.cpu.score, 100);
});

test("decodeViewportBinaryPacket handles 4P quad view, >32-bit frames, subarray offset, and array input", () => {
  const makeView = (w, h, fill) => ({
    width: w,
    height: h,
    rgba: new Uint8Array(w * h * 4).fill(fill)
  });
  const bigFrame = 4294967338; // 2^32 + 42
  const buf = buildZmfrPacket({
    flags: 0x01 | 0x02 | 0x08, // secondary + quadMode + macroActive
    frameNumber: bigFrame,
    views: [
      makeView(256, 384, 0x10),
      makeView(160, 144, 0x20),
      makeView(160, 144, 0x30),
      makeView(160, 144, 0x40)
    ],
    pcmSamples: [0.1, 0.2],
    meta: {
      console: "NDS",
      linkTelemetry: { enabled: true, quadMode: true, packetsTransferred: 512 },
      newlyUnlockedAchievement: "🏆 Link Master (+50 pts)"
    }
  });

  // Wrap inside a larger ArrayBuffer with an odd non-zero byteOffset (3)
  const padded = new Uint8Array(buf.byteLength + 7);
  padded.set(new Uint8Array(buf), 3);
  const sub = padded.subarray(3, 3 + buf.byteLength);

  const decodedSub = decodeViewportBinaryPacket(sub);
  assert.equal(decodedSub.hasSecondary, true);
  assert.equal(decodedSub.quadMode, true);
  assert.equal(decodedSub.macroActive, true);
  assert.equal(decodedSub.sramDirty, false);
  assert.equal(decodedSub.frameNumber, bigFrame);
  assert.equal(decodedSub.views.length, 4);
  assert.deepEqual(
    decodedSub.views.map((v) => [v.width, v.height, v.rgba[0]]),
    [
      [256, 384, 0x10],
      [160, 144, 0x20],
      [160, 144, 0x30],
      [160, 144, 0x40]
    ]
  );
  assert.equal(decodedSub.meta.linkTelemetry.packetsTransferred, 512);
  assert.equal(decodedSub.meta.newlyUnlockedAchievement, "🏆 Link Master (+50 pts)");

  const decodedArr = decodeViewportBinaryPacket(Array.from(new Uint8Array(buf)));
  assert.equal(decodedArr.frameNumber, bigFrame);
  assert.equal(decodedArr.views.length, 4);
});

test("decodeViewportBinaryPacket rejects malformed, truncated, or wrong-version packets", () => {
  assert.throws(() => decodeViewportBinaryPacket("not-bytes"));
  assert.throws(() => decodeViewportBinaryPacket(new ArrayBuffer(12)), /tronqué/);

  const badMagic = new Uint8Array(buildZmfrPacket());
  badMagic.set([0x4e, 0x4f, 0x50, 0x45], 0); // "NOPE"
  assert.throws(() => decodeViewportBinaryPacket(badMagic), /magique/);

  const badVer = new Uint8Array(buildZmfrPacket({ version: 2 }));
  assert.throws(() => decodeViewportBinaryPacket(badVer), /Version ZMFR non supportée/);

  const full = new Uint8Array(buildZmfrPacket({ pcmSamples: [0.1, 0.2, 0.3, 0.4] }));
  assert.throws(
    () => decodeViewportBinaryPacket(full.subarray(0, 40)),
    /En-tête vue|Tampon RGBA vue/
  );
  assert.throws(
    () => decodeViewportBinaryPacket(full.subarray(0, full.byteLength - 2)),
    /Métadonnées ZMFR tronquées/
  );

  const truncPcm = new Uint8Array(buildZmfrPacket({
    views: [{ width: 2, height: 2, rgba: new Uint8Array(16) }],
    pcmSamples: [0.1, 0.2, 0.3, 0.4]
  }));
  // Slice right inside PCM payload
  assert.throws(
    () => decodeViewportBinaryPacket(truncPcm.subarray(0, 35 + 8 + 16 + 8)),
    /Tampon PCM ZMFR tronqué/
  );
});

test("resampleInterleavedStereo converts sample rates cleanly while preserving stereo parity", () => {
  const stereo48k = new Float32Array(800 * 2);
  for (let i = 0; i < 800; i += 1) {
    stereo48k[i * 2] = Math.sin(i * 0.1);
    stereo48k[i * 2 + 1] = -Math.sin(i * 0.1);
  }

  const same = resampleInterleavedStereo(stereo48k, 48000, 48000);
  assert.equal(same.length, stereo48k.length);

  const resampled44k = resampleInterleavedStereo(stereo48k, 48000, 44100);
  assert.equal(resampled44k.length % 2, 0);
  assert.equal(resampled44k.length, Math.round((800 * 44100) / 48000) * 2);
  for (let f = 0; f < resampled44k.length / 2; f += 1) {
    assert.ok(Math.abs(resampled44k[f * 2] + resampled44k[f * 2 + 1]) < 1e-5);
  }

  const empty = resampleInterleavedStereo(new Float32Array(0), 48000, 44100);
  assert.equal(empty.length, 0);
});

test("decodeViewportBinaryPacket decodes RGB565 views (flags & 0x10) and zero-view skip-render packets", () => {
  // 2x2 RGB565: red (0xF800), green (0x07E0), blue (0x001F), white (0xFFFF)
  const rgb565Bytes = new Uint8Array(8);
  const dv = new DataView(rgb565Bytes.buffer);
  dv.setUint16(0, 0xf800, true);
  dv.setUint16(2, 0x07e0, true);
  dv.setUint16(4, 0x001f, true);
  dv.setUint16(6, 0xffff, true);

  const pkt = buildZmfrPacket({
    flags: 0x10,
    frameNumber: 77,
    views: [{ width: 2, height: 2, rgba: rgb565Bytes }],
    pcmSamples: [0.25, -0.25],
    meta: null
  });

  const decoded = decodeViewportBinaryPacket(pkt);
  assert.equal(decoded.rgb565, true);
  assert.equal(decoded.views.length, 1);
  assert.equal(decoded.views[0].pixelFormat, "rgb565");
  assert.ok(decoded.views[0].rgb565 instanceof Uint16Array);
  assert.deepEqual(Array.from(decoded.views[0].rgb565), [0xf800, 0x07e0, 0x001f, 0xffff]);
  assert.equal(decoded.pcmSamples.length, 2);

  // Skip-render packet: viewCount = 0 with live PCM audio still present
  const skipPkt = buildZmfrPacket({
    flags: 0,
    frameNumber: 78,
    views: [],
    pcmSamples: [0.5, -0.5, 0.25, -0.25],
    meta: null
  });
  const decodedSkip = decodeViewportBinaryPacket(skipPkt);
  assert.equal(decodedSkip.frameNumber, 78);
  assert.equal(decodedSkip.views.length, 0);
  assert.equal(decodedSkip.pcmSamples.length, 4);
});
