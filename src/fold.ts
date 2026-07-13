// Accent-insensitive text folding for name matching — the frontend twin of the
// backend's SQL `fold()` (db.rs): lowercase with diacritics stripped, so "mia"
// finds "Mía" and "angel" finds "Ángel" in every picker and search box, and
// committing "mia" merges into the existing Mía instead of minting an
// accent-duplicate person.

export function fold(s: string): string {
  return s
    .normalize("NFD")
    .replace(/\p{M}/gu, "")
    .toLowerCase();
}
