import test from "node:test";
import assert from "node:assert/strict";
import {
  normalizeSearchText,
  formatPlaytime,
  formatFileSize,
  filterCatalogEntries,
  computeConsoleCounts,
  formatHardwareProfileLabel
} from "../../ui/library.js";

const SAMPLE_ENTRIES = [
  {
    id: "gb-pocket-arena",
    title: "Pocket Monster Link Arena",
    console: "GBC",
    engineKind: "nativeRust",
    coreId: "zaimule-rust-gb",
    region: "EUR · FR",
    crc32Hex: "DCE0682B",
    favorite: true,
    linkCableReady: true
  },
  {
    id: "gba-emerald",
    title: "Pokémon Version Émeraude",
    console: "GBA",
    engineKind: "libretroAbi",
    coreId: "mgba_libretro",
    region: "EUR · FR",
    crc32Hex: "1F1C08FB",
    favorite: true,
    linkCableReady: true
  },
  {
    id: "ps1-ff7",
    title: "Final Fantasy VII",
    console: "PS1",
    engineKind: "nativeRust",
    coreId: "zaimule-rust-ps1",
    region: "EUR · FR",
    crc32Hex: "9B8E71A2",
    favorite: false,
    linkCableReady: false
  },
  {
    id: "psp-pulse-horizon",
    title: "Pulse Horizon : Widescreen 333MHz (PSP)",
    console: "PSP",
    engineKind: "nativeRust",
    coreId: "zaimule-rust-psp",
    region: "EUR · FR",
    crc32Hex: "77E49C01",
    favorite: true,
    linkCableReady: true
  }
];

test("normalizeSearchText strips accents and lowercases", () => {
  assert.equal(normalizeSearchText("Pokémon Émeraude"), "pokemon emeraude");
});

test("formatPlaytime and formatFileSize format values cleanly", () => {
  assert.equal(formatPlaytime(15420), "4h 17m");
  assert.equal(formatPlaytime(300), "5m");
  assert.equal(formatFileSize(32768), "32 Ko");
  assert.equal(formatFileSize(16 * 1024 * 1024), "16.0 Mo");
});

test("filterCatalogEntries filters by console, nativeRust, linkCable, and accent-free query", () => {
  const byQuery = filterCatalogEntries(SAMPLE_ENTRIES, { query: "emeraude" });
  assert.equal(byQuery.length, 1);
  assert.equal(byQuery[0].id, "gba-emerald");

  const byPsp = filterCatalogEntries(SAMPLE_ENTRIES, { consoleFilter: "PSP" });
  assert.equal(byPsp.length, 1);
  assert.equal(byPsp[0].id, "psp-pulse-horizon");

  const byRust = filterCatalogEntries(SAMPLE_ENTRIES, { onlyNativeRust: true });
  assert.equal(byRust.length, 3);
  assert.equal(byRust[0].id, "gb-pocket-arena");

  const byLink = filterCatalogEntries(SAMPLE_ENTRIES, { onlyLinkReady: true });
  assert.equal(byLink.length, 3);
});

test("computeConsoleCounts tallies consoles, rust cores, and link cable games", () => {
  const counts = computeConsoleCounts(SAMPLE_ENTRIES);
  assert.equal(counts.ALL, 4);
  assert.equal(counts.GBC, 1);
  assert.equal(counts.GBA, 1);
  assert.equal(counts.PS1, 1);
  assert.equal(counts.PSP, 1);
  assert.equal(counts.RUST, 3);
  assert.equal(counts.LINK, 3);
});

test("formatHardwareProfileLabel formats Yuzu CPU overclock and PGXP state", () => {
  assert.equal(formatHardwareProfileLabel(100, true), "100% Natif · PGXP ON");
  assert.equal(formatHardwareProfileLabel(200, false), "200% Turbo · PGXP OFF");
});

