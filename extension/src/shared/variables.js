// Variable helpers, ported from the desktop client. A variable is
// {name} where name is [A-Za-z0-9_-].
const RE = /\{([A-Za-z0-9_\-]+)\}/g;

export function extractVarNames(body) {
  const out = new Set();
  let m;
  while ((m = RE.exec(body)) !== null) out.add(m[1]);
  RE.lastIndex = 0;
  return [...out];
}

export function substitute(body, values) {
  return body.replace(RE, (full, name) =>
    Object.prototype.hasOwnProperty.call(values, name) ? values[name] : full,
  );
}

// Split into text/var chunks so the preview can highlight variables.
export function splitForPreview(body) {
  const out = [];
  let last = 0;
  let m;
  while ((m = RE.exec(body)) !== null) {
    if (m.index > last) out.push({ type: "text", text: body.slice(last, m.index) });
    out.push({ type: "var", name: m[1] });
    last = m.index + m[0].length;
  }
  RE.lastIndex = 0;
  if (last < body.length) out.push({ type: "text", text: body.slice(last) });
  return out;
}
