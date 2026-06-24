// Render a snippet body as formatted text, driven by the configurable
// format rules (the same markup the desktop app inserts, which is what
// WHMCS expects). Shared by the manager editor preview and the launcher
// preview so both look identical.

function escapeRe(s) {
  return s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

// A rule whose suffix looks like "](...)" is a markdown link, not a plain
// wrap (matches the desktop's isLinkRule).
function isLinkRule(rule) {
  return /\]\s*\([^)]*\)\s*$/.test(rule.suffix || "");
}

function fmtTag(rule) {
  const l = (rule.label || "").toLowerCase();
  if (l.includes("bold")) return "strong";
  if (l.includes("italic")) return "em";
  if (l.includes("code")) return "code";
  if (l.includes("underline")) return "u";
  if (l.includes("strike")) return "s";
  return "strong";
}

function buildMatchers(rules) {
  const m = [{ kind: "var", re: /\{[A-Za-z0-9_.-]+\}/ }];
  // Link text excludes brackets so a stray outer "[" can't swallow an inner
  // [text](url); the inner link matches and the outer brackets stay literal.
  if (rules.some(isLinkRule)) m.push({ kind: "link", re: /\[([^[\]]+)\]\(([^)\s]+)\)/ });
  for (const r of rules
    .filter((r) => r.prefix && r.suffix && !isLinkRule(r))
    .sort((a, b) => b.prefix.length - a.prefix.length)) {
    m.push({ kind: "wrap", tag: fmtTag(r), re: new RegExp(`${escapeRe(r.prefix)}([\\s\\S]+?)${escapeRe(r.suffix)}`) });
  }
  m.push({ kind: "url", re: /https?:\/\/[^\s)]+/ });
  return m;
}

function appendInline(parent, text, matchers) {
  let rest = text;
  while (rest) {
    let best = null;
    for (const p of matchers) {
      const m = p.re.exec(rest);
      if (m && (!best || m.index < best.m.index)) best = { p, m };
    }
    if (!best) {
      parent.appendChild(document.createTextNode(rest));
      break;
    }
    const { p, m } = best;
    if (m.index > 0) parent.appendChild(document.createTextNode(rest.slice(0, m.index)));
    if (p.kind === "var") {
      const v = document.createElement("var");
      v.textContent = m[0];
      parent.appendChild(v);
    } else if (p.kind === "link" || p.kind === "url") {
      const url = p.kind === "link" ? m[2] : m[0];
      const a = document.createElement("a");
      if (p.kind === "link") appendInline(a, m[1], matchers); // link text can itself be formatted
      else a.textContent = m[0];
      if (/^(https?:|mailto:)/i.test(url)) {
        a.href = url;
        a.target = "_blank";
        a.rel = "noopener noreferrer";
      }
      parent.appendChild(a);
    } else {
      const node = document.createElement(p.tag);
      if (p.tag === "code") node.textContent = m[1];
      else appendInline(node, m[1], matchers); // allow nested marks
      parent.appendChild(node);
    }
    rest = rest.slice(m.index + m[0].length);
  }
}

// Render `text` into `container` using `rules`. Block structure (bullet and
// numbered lists, headings) is standard; inline marks come from the rules.
export function renderFormatted(container, text, rules) {
  const matchers = buildMatchers(rules);
  container.replaceChildren();
  let list = null;
  for (const line of (text || "").split("\n")) {
    const bullet = /^\s*[-*]\s+(.*)$/.exec(line);
    const num = /^\s*\d+\.\s+(.*)$/.exec(line);
    const head = /^(#{1,3})\s+(.*)$/.exec(line);
    if (bullet) {
      if (list?.tagName !== "UL") {
        list = document.createElement("ul");
        list.className = "md-list";
        container.appendChild(list);
      }
      const li = document.createElement("li");
      appendInline(li, bullet[1], matchers);
      list.appendChild(li);
      continue;
    }
    if (num) {
      if (list?.tagName !== "OL") {
        list = document.createElement("ol");
        list.className = "md-list";
        container.appendChild(list);
      }
      const li = document.createElement("li");
      appendInline(li, num[1], matchers);
      list.appendChild(li);
      continue;
    }
    list = null;
    if (head) {
      const h = document.createElement("div");
      h.className = `md-h md-h${head[1].length}`;
      appendInline(h, head[2], matchers);
      container.appendChild(h);
      continue;
    }
    const div = document.createElement("div");
    div.className = "md-line";
    appendInline(div, line, matchers);
    container.appendChild(div);
  }
}
