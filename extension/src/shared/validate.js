// Client-side limits mirroring crates/snipdesk-server/src/validate.rs,
// for instant editor feedback. The server re-validates regardless.
export const LIMITS = {
  TITLE: 300,
  BODY: 100_000,
  TAG: 60,
  MAX_TAGS: 50,
  FOLDER: 300,
};

// True if `s` contains a control character. Bodies pass
// allowWhitespace=true to permit tab (0x09), newline (0x0A), and
// carriage return (0x0D); one-line fields reject all control chars.
function hasControl(s, allowWhitespace) {
  for (const ch of s) {
    const c = ch.codePointAt(0);
    if (c < 0x20 || c === 0x7f) {
      if (allowWhitespace && (c === 0x09 || c === 0x0a || c === 0x0d)) continue;
      return true;
    }
  }
  return false;
}

export function validateSnippet({ title, body, tags, folder_path }) {
  const t = (title || "").trim();
  if (!t) return "Title is required.";
  if ([...(title || "")].length > LIMITS.TITLE) return `Title is too long (max ${LIMITS.TITLE}).`;
  if (hasControl(title || "", false)) return "Title contains control characters.";
  if ([...(body || "")].length > LIMITS.BODY) return `Body is too long (max ${LIMITS.BODY}).`;
  if (hasControl(body || "", true)) return "Body contains control characters.";
  const list = tags || [];
  if (list.length > LIMITS.MAX_TAGS) return `Too many tags (max ${LIMITS.MAX_TAGS}).`;
  for (const tag of list) {
    if ([...tag].length > LIMITS.TAG) return `Tag "${tag}" is too long (max ${LIMITS.TAG}).`;
    if (hasControl(tag, false)) return "A tag contains control characters.";
  }
  if (folder_path) {
    if ([...folder_path].length > LIMITS.FOLDER) return `Folder path is too long (max ${LIMITS.FOLDER}).`;
    if (hasControl(folder_path, false)) return "Folder path contains control characters.";
  }
  return null;
}
