/**
 * Zero-JSON-array binary packet decoder for Zaimule's `"ZMFR"` v1 viewport IPC stream.
 * Layout (little-endian):
 * - Header (35 bytes): "ZMFR" (4) | version=1 (u8) | flags (u8) | sampleRateHz (u32) |
 *   frameNumber (u64) | audioLevel (f32) | audioFreqHz (u16) | reserved (u16) |
 *   rewindSnapshotsCount (u32) | rewindMemoryBytes (u32) | viewCount (u8)
 * - Views (viewCount ×): width (u16) | height (u16) | rgbaLen (u32) | rgba ([u8; rgbaLen])
 * - PCM: floatCount (u32) | pcm ([f32; floatCount])
 * - Trailer: metaLen (u32) | utf8Json ([u8; metaLen])
 */

const TEXT_DECODER = new TextDecoder();
const HEADER_BYTES = 35;

const IS_LITTLE_ENDIAN = (() => {
  const buf = new ArrayBuffer(2);
  new DataView(buf).setUint16(0, 0x0102, true);
  return new Uint8Array(buf)[0] === 0x02;
})();

function toUint8View(input) {
  if (input instanceof ArrayBuffer) {
    return new Uint8Array(input);
  }
  if (ArrayBuffer.isView(input)) {
    return new Uint8Array(input.buffer, input.byteOffset, input.byteLength);
  }
  if (Array.isArray(input)) {
    return Uint8Array.from(input);
  }
  throw new Error("Paquet binaire ZMFR invalide : type d'entrée non pris en charge");
}

function readLittleEndianFloat32Array(bytes, dv, offset, floatCount) {
  if (floatCount === 0) {
    return new Float32Array(0);
  }
  const byteLen = floatCount * 4;
  if (IS_LITTLE_ENDIAN) {
    // Copy into an aligned, owned ArrayBuffer (also safe to transfer to AudioWorklet)
    const alignedCopy = bytes.slice(offset, offset + byteLen);
    return new Float32Array(alignedCopy.buffer);
  }
  const out = new Float32Array(floatCount);
  for (let i = 0; i < floatCount; i += 1) {
    out[i] = dv.getFloat32(offset + i * 4, true);
  }
  return out;
}

/**
 * Decodes a raw `"ZMFR"` v1 binary response into typed views, owned PCM Float32Array, and metadata.
 */
export function decodeViewportBinaryPacket(input) {
  const bytes = toUint8View(input);
  if (bytes.byteLength < HEADER_BYTES) {
    throw new Error(`Paquet ZMFR tronqué (${bytes.byteLength} < ${HEADER_BYTES} octets)`);
  }

  // "ZMFR" magic
  if (
    bytes[0] !== 0x5a ||
    bytes[1] !== 0x4d ||
    bytes[2] !== 0x46 ||
    bytes[3] !== 0x52
  ) {
    throw new Error("En-tête magique ZMFR invalide");
  }

  const version = bytes[4];
  if (version !== 1) {
    throw new Error(`Version ZMFR non supportée : ${version}`);
  }

  const dv = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  const flags = bytes[5];
  const sampleRateHz = dv.getUint32(6, true);
  const frameLo = dv.getUint32(10, true);
  const frameHi = dv.getUint32(14, true);
  const frameNumber = frameLo + frameHi * 4294967296;
  const audioLevel = dv.getFloat32(18, true);
  const audioFreqHz = dv.getUint16(22, true);
  const rewindSnapshotsCount = dv.getUint32(26, true);
  const rewindMemoryBytes = dv.getUint32(30, true);
  const viewCount = bytes[34];

  let offset = HEADER_BYTES;
  const views = [];

  for (let i = 0; i < viewCount; i += 1) {
    if (offset + 8 > bytes.byteLength) {
      throw new Error(`En-tête vue #${i} tronqué`);
    }
    const width = dv.getUint16(offset, true);
    const height = dv.getUint16(offset + 2, true);
    const rgbaLen = dv.getUint32(offset + 4, true);
    offset += 8;

    if (offset + rgbaLen > bytes.byteLength) {
      throw new Error(`Tampon RGBA vue #${i} tronqué (${rgbaLen} octets attendus)`);
    }
    const rgba = new Uint8ClampedArray(bytes.buffer, bytes.byteOffset + offset, rgbaLen);
    views.push({ width, height, rgba });
    offset += rgbaLen;
  }

  if (offset + 4 > bytes.byteLength) {
    throw new Error("En-tête PCM ZMFR tronqué");
  }
  const floatCount = dv.getUint32(offset, true);
  offset += 4;
  const pcmByteLen = floatCount * 4;
  if (offset + pcmByteLen > bytes.byteLength) {
    throw new Error(`Tampon PCM ZMFR tronqué (${pcmByteLen} octets attendus)`);
  }
  const pcmSamples = readLittleEndianFloat32Array(bytes, dv, offset, floatCount);
  offset += pcmByteLen;

  if (offset + 4 > bytes.byteLength) {
    throw new Error("En-tête métadonnées ZMFR tronqué");
  }
  const metaLen = dv.getUint32(offset, true);
  offset += 4;
  if (offset + metaLen > bytes.byteLength) {
    throw new Error(`Métadonnées ZMFR tronquées (${metaLen} octets attendus)`);
  }

  let meta = {};
  if (metaLen > 0) {
    const jsonText = TEXT_DECODER.decode(bytes.subarray(offset, offset + metaLen));
    meta = JSON.parse(jsonText);
  }

  return {
    version,
    flags,
    hasSecondary: (flags & 0x01) !== 0,
    quadMode: (flags & 0x02) !== 0,
    sramDirty: (flags & 0x04) !== 0,
    macroActive: (flags & 0x08) !== 0,
    sampleRateHz,
    frameNumber,
    audioLevel,
    audioFreqHz,
    rewindSnapshotsCount,
    rewindMemoryBytes,
    views,
    pcmSamples,
    meta
  };
}
