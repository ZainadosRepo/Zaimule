/**
 * Pure ES module for Zaimule's multi-console ROM catalog filtering,
 * CRC32/accent-insensitive search, playtime formatting, and Yuzu-style override resolution.
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
  return `${(b / (1024 * 1024)).toFixed(1)} Mo`;
}

export function filterCatalogEntries(entries, filterOptions = {}) {
  if (!Array.isArray(entries)) return [];
  const {
    consoleFilter = "ALL",
    onlyNativeRust = false,
    onlyLinkReady = false,
    onlyFavorites = false,
    query = ""
  } = filterOptions;

  const normalizedQuery = normalizeSearchText(query);

  return entries.filter((item) => {
    if (!item) return false;
    if (
      consoleFilter &&
      consoleFilter !== "ALL" &&
      String(item.console).toUpperCase() !== String(consoleFilter).toUpperCase()
    ) {
      return false;
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

    const haystack = normalizeSearchText(
      `${item.title || ""} ${item.console || ""} ${item.crc32Hex || ""} ${item.region || ""} ${item.coreId || ""}`
    );
    return haystack.includes(normalizedQuery);
  });
}

export function computeConsoleCounts(entries) {
  const counts = { ALL: 0, RUST: 0, LINK: 0 };
  if (!Array.isArray(entries)) return counts;

  for (const g of entries) {
    if (!g) continue;
    counts.ALL += 1;
    const c = String(g.console || "").toUpperCase();
    counts[c] = (counts[c] || 0) + 1;
    if (g.engineKind === "nativeRust") counts.RUST += 1;
    if (g.linkCableReady) counts.LINK += 1;
  }
  return counts;
}
