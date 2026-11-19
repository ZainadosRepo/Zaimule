import test from "node:test";
import assert from "node:assert/strict";
import {
  normalizeSearchText,
  formatPlaytime,
  formatFileSize,
  filterCatalogEntries,
  computeConsoleCounts,
  sortCatalogEntries,
  formatHardwareProfileLabel,
  formatSwitchEngineLabel
} from "../../ui/library.js";

const SAMPLE_ENTRIES = [
  {
    id: "gb-tetris-dmg",
    title: "Tetris (World)",
    console: "GB",
    engineKind: "nativeRust",
    coreId: "zaimule-rust-gb",
    region: "WORLD",
    crc32Hex: "46DF91AD",
    playtimeSeconds: 1800,
    favorite: false,
    linkCableReady: true
  },
  {
    id: "gbc-pocket-arena",
    title: "Pocket Monster Link Arena",
    console: "GBC",
    engineKind: "nativeRust",
    coreId: "zaimule-rust-gb",
    region: "EUR · FR",
    crc32Hex: "DCE0682B",
    playtimeSeconds: 600,
    favorite: true,
    linkCableReady: true
  },
  {
    id: "gba-emerald",
    title: "Pokémon Version Émeraude",
    console: "GBA",
    engineKind: "nativeRust",
    coreId: "zaimule-rust-gba",
    region: "EUR · FR",
    crc32Hex: "1F1C08FB",
    playtimeSeconds: 5400,
    favorite: true,
    linkCableReady: true
  },
  {
    id: "nds-platinum",
    title: "Pokémon Version Platine",
    console: "NDS",
    engineKind: "nativeRust",
    coreId: "zaimule-rust-nds",
    region: "EUR · FR",
    crc32Hex: "A1B2C3D4",
    titleId: "CPUF",
    playtimeSeconds: 3600,
    favorite: true,
    linkCableReady: false
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

test("filterCatalogEntries filters by exact console, GB,GBC group, nativeRust, linkCable, and accent-free query", () => {
  const byQuery = filterCatalogEntries(SAMPLE_ENTRIES, { query: "emeraude" });
  assert.equal(byQuery.length, 1);
  assert.equal(byQuery[0].id, "gba-emerald");

  const byNdsCode = filterCatalogEntries(SAMPLE_ENTRIES, { query: "cpuf" });
  assert.equal(byNdsCode.length, 1);
  assert.equal(byNdsCode[0].id, "nds-platinum");

  const byGbExact = filterCatalogEntries(SAMPLE_ENTRIES, { consoleFilter: "GB" });
  assert.equal(byGbExact.length, 1);
  assert.equal(byGbExact[0].id, "gb-tetris-dmg");

  const byGbcExact = filterCatalogEntries(SAMPLE_ENTRIES, { consoleFilter: "GBC" });
  assert.equal(byGbcExact.length, 1);
  assert.equal(byGbcExact[0].id, "gbc-pocket-arena");

  const byGbGroup = filterCatalogEntries(SAMPLE_ENTRIES, { consoleFilter: "GB,GBC" });
  assert.equal(byGbGroup.length, 2);
  assert.deepEqual(
    byGbGroup.map((e) => e.id),
    ["gb-tetris-dmg", "gbc-pocket-arena"]
  );

  const byRust = filterCatalogEntries(SAMPLE_ENTRIES, { onlyNativeRust: true });
  assert.equal(byRust.length, 4);

  const byLink = filterCatalogEntries(SAMPLE_ENTRIES, { onlyLinkReady: true });
  assert.equal(byLink.length, 3);

  const byFav = filterCatalogEntries(SAMPLE_ENTRIES, { onlyFavorites: true });
  assert.equal(byFav.length, 3);
});

test("computeConsoleCounts tallies consoles, favorites, rust cores, and link cable games", () => {
  const counts = computeConsoleCounts(SAMPLE_ENTRIES);
  assert.equal(counts.ALL, 4);
  assert.equal(counts.FAVORITES, 3);
  assert.equal(counts.GB, 1);
  assert.equal(counts.GBC, 1);
  assert.equal(counts.GBA, 1);
  assert.equal(counts.NDS, 1);
  assert.equal(counts.RUST, 4);
  assert.equal(counts.LINK, 3);
});

test("sortCatalogEntries sorts by recent, title, playtime, and console without mutating input", () => {
  const originalOrder = SAMPLE_ENTRIES.map((e) => e.id);
  const recent = sortCatalogEntries(SAMPLE_ENTRIES, "recent");
  assert.deepEqual(
    recent.map((e) => e.id),
    originalOrder
  );

  const byTitle = sortCatalogEntries(SAMPLE_ENTRIES, "title");
  assert.deepEqual(
    byTitle.map((e) => e.id),
    ["gbc-pocket-arena", "gba-emerald", "nds-platinum", "gb-tetris-dmg"]
  );

  const byPlaytime = sortCatalogEntries(SAMPLE_ENTRIES, "playtime");
  assert.deepEqual(
    byPlaytime.map((e) => e.id),
    ["gba-emerald", "nds-platinum", "gb-tetris-dmg", "gbc-pocket-arena"]
  );

  const byConsole = sortCatalogEntries(SAMPLE_ENTRIES, "console");
  assert.deepEqual(
    byConsole.map((e) => e.id),
    ["gb-tetris-dmg", "gba-emerald", "gbc-pocket-arena", "nds-platinum"]
  );

  assert.deepEqual(
    SAMPLE_ENTRIES.map((e) => e.id),
    originalOrder
  );
});

test("formatHardwareProfileLabel formats CPU overclock and PGXP state", () => {
  assert.equal(formatHardwareProfileLabel(100, true), "100% Natif · PGXP ON");
  assert.equal(formatHardwareProfileLabel(200, false), "200% Turbo · PGXP OFF");
});

test("formatFileSize and filterCatalogEntries handle Switch 1 (Torzu) & Switch 2 (Fritchii) entries", () => {
  assert.equal(formatFileSize(4 * 1024 * 1024 * 1024), "4.00 Go");
  const fullCatalog = [
    ...SAMPLE_ENTRIES,
    {
      id: "nsw-zelda",
      title: "Tears of the Kingdom",
      console: "SWITCH",
      engineKind: "externalLauncher",
      switchEngine: "auto",
      coreId: "torzu-external",
      region: "NSP",
      crc32Hex: "CAFE1234",
      titleId: "0100F2C0115B6000",
      favorite: false,
      linkCableReady: false
    },
    {
      id: "ns2-kart",
      title: "Mario Kart 9",
      console: "SWITCH2",
      engineKind: "externalLauncher",
      switchEngine: "auto",
      coreId: "fritchii-external",
      region: "NX",
      crc32Hex: "FEEDBEEF",
      titleId: "0200999000011000",
      favorite: true,
      linkCableReady: false
    }
  ];

  const bySwitch = filterCatalogEntries(fullCatalog, { consoleFilter: "SWITCH" });
  assert.equal(bySwitch.length, 1);
  assert.equal(bySwitch[0].id, "nsw-zelda");

  const bySwitch2 = filterCatalogEntries(fullCatalog, { consoleFilter: "SWITCH2" });
  assert.equal(bySwitch2.length, 1);
  assert.equal(bySwitch2[0].id, "ns2-kart");

  const byTitleId = filterCatalogEntries(fullCatalog, { query: "0100f2c0115b6000" });
  assert.equal(byTitleId.length, 1);
  assert.equal(byTitleId[0].id, "nsw-zelda");

  const counts = computeConsoleCounts(fullCatalog);
  assert.equal(counts.ALL, 6);
  assert.equal(counts.FAVORITES, 4);
  assert.equal(counts.SWITCH, 1);
  assert.equal(counts.SWITCH2, 1);
  assert.equal(counts.RUST, 4);
});

test("formatSwitchEngineLabel resolves Auto and explicit Torzu/Fritchii overrides", () => {
  assert.equal(formatSwitchEngineLabel("auto", "SWITCH"), "Auto (Torzu)");
  assert.equal(formatSwitchEngineLabel("auto", "SWITCH2"), "Auto (Fritchii)");
  assert.equal(formatSwitchEngineLabel("torzu", "SWITCH2"), "Torzu");
  assert.equal(formatSwitchEngineLabel("fritchii", "SWITCH"), "Fritchii");
});

