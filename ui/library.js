/**
 * Pure ES module for Zaimule's multi-console ROM catalog filtering,
 * CRC32/accent-insensitive search, playtime formatting, and Switch engine routing labels.
 */

export function normalizeSearchText(input) {
  if (!input || typeof input !== "string") return "";
  return input
    .normalize("NFD")
    .replace(/[\u0300-\u036f]/g, "")
    .toLowerCase()
    .trim();
}

export function formatPlaytime(seconds) {
  const total = Math.max(0, Number(seconds) || 0);
  const hours = Math.floor(total / 3600);
  const minutes = Math.floor((total % 3600) / 60);
  if (hours === 0) return `${minutes}m`;
  return `${hours}h ${String(minutes).padStart(2, "0")}m`;
}

export function formatFileSize(bytes) {
  const b = Math.max(0, Number(bytes) || 0);
  if (b < 1024) return `${b} o`;
  if (b < 1024 * 1024) return `${(b / 1024).toFixed(0)} Ko`;
  if (b < 1024 * 1024 * 1024) return `${(b / (1024 * 1024)).toFixed(1)} Mo`;
  return `${(b / (1024 * 1024 * 1024)).toFixed(2)} Go`;
}

export function filterCatalogEntries(entries, filterOptions = {}) {
  if (!Array.isArray(entries)) return [];
  const {
    consoleFilter = "ALL",
    collectionFilter = "",
    onlyNativeRust = false,
    onlyLinkReady = false,
    onlyFavorites = false,
    query = ""
  } = filterOptions;

  const normalizedQuery = normalizeSearchText(query);
  const normalizedCollection = normalizeSearchText(collectionFilter);
  const rawFilter = String(consoleFilter || "ALL").toUpperCase();
  const allowedConsoles =
    rawFilter === "ALL"
      ? null
      : rawFilter
          .split(",")
          .map((s) => s.trim())
          .filter(Boolean);

  return entries.filter((item) => {
    if (!item) return false;
    if (allowedConsoles && allowedConsoles.length > 0) {
      const itemConsole = String(item.console || "").toUpperCase();
      if (!allowedConsoles.includes(itemConsole)) {
        return false;
      }
    }
    if (normalizedCollection) {
      const cols = Array.isArray(item.collections) ? item.collections : [];
      if (!cols.some((c) => normalizeSearchText(c) === normalizedCollection)) {
        return false;
      }
    }
    if (onlyNativeRust && item.engineKind !== "nativeRust") {
      return false;
    }
    if (onlyLinkReady && !item.linkCableReady) {
      return false;
    }
    if (onlyFavorites && !item.favorite) {
      return false;
    }
    if (!normalizedQuery) return true;

    const colsJoined = Array.isArray(item.collections) ? item.collections.join(" ") : "";
    const haystack = normalizeSearchText(
      `${item.title || ""} ${item.console || ""} ${item.crc32Hex || ""} ${item.titleId || ""} ${item.region || ""} ${item.coreId || ""} ${item.metadataGenre || ""} ${item.metadataDeveloper || ""} ${item.metadataPublisher || ""} ${item.metadataReleaseYear || ""} ${colsJoined}`
    );
    return haystack.includes(normalizedQuery);
  });
}

export function computeConsoleCounts(entries) {
  const counts = { ALL: 0, FAVORITES: 0, RUST: 0, LINK: 0 };
  if (!Array.isArray(entries)) return counts;

  for (const g of entries) {
    if (!g) continue;
    counts.ALL += 1;
    const c = String(g.console || "").toUpperCase();
    counts[c] = (counts[c] || 0) + 1;
    if (g.favorite) counts.FAVORITES += 1;
    if (g.engineKind === "nativeRust") counts.RUST += 1;
    if (g.linkCableReady) counts.LINK += 1;
  }
  return counts;
}

export function computeCollectionCounts(entries, allCollections = []) {
  const map = new Map();
  if (Array.isArray(allCollections)) {
    for (const raw of allCollections) {
      const name = String(raw || "").trim();
      if (!name) continue;
      const key = normalizeSearchText(name);
      if (!map.has(key)) {
        map.set(key, { name, count: 0 });
      }
    }
  }
  if (Array.isArray(entries)) {
    for (const g of entries) {
      if (!g || !Array.isArray(g.collections)) continue;
      const seenForGame = new Set();
      for (const raw of g.collections) {
        const name = String(raw || "").trim();
        if (!name) continue;
        const key = normalizeSearchText(name);
        if (seenForGame.has(key)) continue;
        seenForGame.add(key);
        if (!map.has(key)) {
          map.set(key, { name, count: 1 });
        } else {
          map.get(key).count += 1;
        }
      }
    }
  }
  return Array.from(map.values()).sort((a, b) =>
    a.name.localeCompare(b.name, "fr", { sensitivity: "base" })
  );
}

export function formatShortGenre(genre) {
  const raw = String(genre || "").trim();
  if (!raw) return "";
  const first = raw.split("/")[0].trim();
  if (/role[- ]playing/i.test(first)) return "RPG";
  if (/racing|driving/i.test(first)) return "Course";
  if (/platform/i.test(first)) return "Plateforme";
  if (/adventure/i.test(first)) return "Aventure";
  if (/strategy/i.test(first)) return "Stratégie";
  if (/fighting/i.test(first)) return "Combat";
  if (/puzzle/i.test(first)) return "Puzzle";
  if (/sports/i.test(first)) return "Sport";
  return first;
}

export function formatGameSubtitle(game, fallbackConsoleLabel) {
  const year = String(game?.metadataReleaseYear || "").trim();
  const genre = formatShortGenre(game?.metadataGenre);
  if (genre && year) return `${genre} · ${year}`;
  if (year) return `${fallbackConsoleLabel} · ${year}`;
  if (genre) return genre;
  return fallbackConsoleLabel;
}

export function sortCatalogEntries(entries, sortBy = "recent") {
  if (!Array.isArray(entries)) return [];
  const copy = [...entries];
  const mode = String(sortBy || "recent").toLowerCase();
  if (mode === "title") {
    return copy.sort((a, b) =>
      String(a?.title || "").localeCompare(String(b?.title || ""), "fr", {
        sensitivity: "base"
      })
    );
  }
  if (mode === "playtime") {
    return copy.sort((a, b) => {
      const diff = (Number(b?.playtimeSeconds) || 0) - (Number(a?.playtimeSeconds) || 0);
      if (diff !== 0) return diff;
      return String(a?.title || "").localeCompare(String(b?.title || ""), "fr", {
        sensitivity: "base"
      });
    });
  }
  if (mode === "year") {
    const parseYear = (v) => {
      const m = String(v || "").match(/\b(19\d{2}|20\d{2})\b/);
      return m ? Number(m[1]) : 0;
    };
    return copy.sort((a, b) => {
      const diff = parseYear(b?.metadataReleaseYear) - parseYear(a?.metadataReleaseYear);
      if (diff !== 0) return diff;
      return String(a?.title || "").localeCompare(String(b?.title || ""), "fr", {
        sensitivity: "base"
      });
    });
  }
  if (mode === "console") {
    return copy.sort((a, b) => {
      const cDiff = String(a?.console || "").localeCompare(String(b?.console || ""), "fr", {
        sensitivity: "base"
      });
      if (cDiff !== 0) return cDiff;
      return String(a?.title || "").localeCompare(String(b?.title || ""), "fr", {
        sensitivity: "base"
      });
    });
  }
  return copy;
}

export function formatHardwareProfileLabel(cpuOverclockPct = 100, pgxpSubpixel = true) {
  const clamped = Math.max(50, Math.min(400, Number(cpuOverclockPct) || 100));
  const mode = clamped > 100 ? "Turbo" : "Natif";
  const pgxp = pgxpSubpixel ? "PGXP ON" : "PGXP OFF";
  return `${clamped}% ${mode} · ${pgxp}`;
}

export function formatSwitchEngineLabel(overrideChoice, consoleKind) {
  const choice = String(overrideChoice || "auto").toLowerCase();
  const c = String(consoleKind || "").toUpperCase();
  if (choice === "torzu") return "Torzu";
  if (choice === "fritchii") return "Fritchii";
  return c === "SWITCH2" ? "Auto (Fritchii)" : "Auto (Torzu)";
}
