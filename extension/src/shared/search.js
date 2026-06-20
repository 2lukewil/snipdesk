// Substring search + sort, ported from the desktop's client-side
// filtering. Snippets are {title, body, tags, folder_path, uses,
// source}. Searching ranks title matches above body/tag matches.

export function filterSnippets(snippets, query) {
  const q = (query || "").trim().toLowerCase();
  if (!q) return snippets.slice();
  const scored = [];
  for (const s of snippets) {
    const title = (s.title || "").toLowerCase();
    const tags = (s.tags || []).join(" ").toLowerCase();
    const body = (s.body || "").toLowerCase();
    let rank;
    if (title.startsWith(q)) rank = 0;
    else if (title.includes(q)) rank = 1;
    else if (tags.includes(q)) rank = 2;
    else if (body.includes(q)) rank = 3;
    else continue;
    scored.push({ s, rank });
  }
  scored.sort((a, b) => a.rank - b.rank);
  return scored.map((x) => x.s);
}

export function sortSnippets(list, sortByUsage) {
  const out = list.slice();
  const byTitle = (a, b) =>
    (a.title || "").localeCompare(b.title || "", undefined, { sensitivity: "base" });
  if (sortByUsage) {
    out.sort((a, b) => (b.uses || 0) - (a.uses || 0) || byTitle(a, b));
  } else {
    out.sort(byTitle);
  }
  return out;
}
