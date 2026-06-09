import { defineConfig } from "vitepress";

// Repo coordinates. Update if you rename the repo / change the user.
const GH_USER = "2lukewil";
const GH_REPO = "snipdesk";
const GH_BRANCH = "main";

// GitHub Pages serves project sites under /<repo>/, so the built site
// must be aware of that prefix. If you later point a custom domain at
// the Pages site, drop the trailing path: `base: "/"`.
const BASE = `/${GH_REPO}/`;

export default defineConfig({
  base: BASE,
  lang: "en-US",
  title: "SnipDesk",
  description:
    "Fast snippet launcher for support agents. Hotkey, search, paste. Built with Tauri (Rust + web UI), with an optional self-hosted Teams backend.",
  cleanUrls: true,
  lastUpdated: true,

  // Cross-folder relative links (../README.md, ../brands/_template/README.md,
  // ../crates/...) point at source files that aren't part of the docs tree.
  // The markdown.config hook below rewrites them to GitHub URLs so they open
  // in a new tab on the docs site. This flag is the backstop for anything
  // the rewriter misses; turn it off once every link resolves locally.
  ignoreDeadLinks: true,

  head: [
    ["meta", { name: "theme-color", content: "#4c9aff" }],
    ["meta", { name: "og:type", content: "website" }],
    ["meta", { name: "og:title", content: "SnipDesk Documentation" }],
    [
      "meta",
      {
        name: "og:description",
        content:
          "Hotkey-driven snippet launcher with optional self-hosted Teams sync.",
      },
    ],
  ],

  themeConfig: {
    siteTitle: "SnipDesk",

    nav: [
      { text: "Guide", link: "/getting-started", activeMatch: "/getting-started" },
      {
        text: "Self-hosting",
        items: [
          { text: "Docker Quickstart", link: "/docker-quickstart" },
          { text: "Production Deploy", link: "/deploy" },
          { text: "Server Architecture", link: "/server-design" },
        ],
      },
      {
        text: "Development",
        items: [
          { text: "Releases & Auto-update", link: "/auto-update" },
          { text: "Roadmap", link: "/ROADMAP" },
          { text: "Browser integration (design)", link: "/browser-integration" },
        ],
      },
      {
        text: "Releases",
        link: `https://github.com/${GH_USER}/${GH_REPO}/releases`,
      },
    ],

    sidebar: {
      "/": [
        {
          text: "Getting started",
          items: [
            { text: "Introduction", link: "/" },
            { text: "Install & first run", link: "/getting-started" },
          ],
        },
        {
          text: "Self-hosting the server",
          collapsed: false,
          items: [
            { text: "Docker quickstart (5 min)", link: "/docker-quickstart" },
            { text: "Production deployment", link: "/deploy" },
            { text: "Server architecture", link: "/server-design" },
          ],
        },
        {
          text: "Releases & whitelabel",
          collapsed: false,
          items: [
            { text: "Auto-update & release flow", link: "/auto-update" },
            { text: "Whitelabel brand bundles", link: "/whitelabel" },
          ],
        },
        {
          text: "Reference",
          collapsed: false,
          items: [
            { text: "Roadmap", link: "/ROADMAP" },
            { text: "Browser integration (design)", link: "/browser-integration" },
          ],
        },
      ],
    },

    socialLinks: [
      { icon: "github", link: `https://github.com/${GH_USER}/${GH_REPO}` },
    ],

    editLink: {
      pattern: `https://github.com/${GH_USER}/${GH_REPO}/edit/${GH_BRANCH}/docs/:path`,
      text: "Edit this page on GitHub",
    },

    footer: {
      message: "Released under the MIT License.",
      copyright: `Source on <a href="https://github.com/${GH_USER}/${GH_REPO}">GitHub</a>.`,
    },

    // Built-in MiniSearch. No API key, no external requests, indexes every
    // page at build time. Works on the GitHub Pages static host.
    search: {
      provider: "local",
      options: {
        detailedView: true,
        miniSearch: {
          searchOptions: {
            fuzzy: 0.2,
            prefix: true,
            boost: { title: 4, text: 2, titles: 1 },
          },
        },
      },
    },

    outline: {
      level: [2, 3],
      label: "On this page",
    },

    docFooter: {
      prev: "Previous",
      next: "Next",
    },
  },

  markdown: {
    lineNumbers: false,

    // Rewrite cross-folder relative links (../foo, ../../foo) to GitHub URLs.
    // The docs tree references several source files outside docs/ (README.md,
    // brands/_template/, crates/snipdesk-server/snipdesk-server.example.toml).
    // Those targets are not part of the docs site, so the most useful thing
    // we can do is open them on GitHub.
    config(md) {
      const ghBase = `https://github.com/${GH_USER}/${GH_REPO}/blob/${GH_BRANCH}/`;
      const defaultRender =
        md.renderer.rules.link_open ||
        ((tokens, idx, options, env, self) => self.renderToken(tokens, idx, options));

      md.renderer.rules.link_open = (tokens, idx, options, env, self) => {
        const token = tokens[idx];
        const href = token.attrGet("href");

        if (href && /^(\.\.\/)+/.test(href)) {
          // Strip every leading "../" and append to the GitHub URL.
          const cleaned = href.replace(/^(\.\.\/)+/, "");
          // Anchor links inside the same external file are preserved.
          token.attrSet("href", `${ghBase}${cleaned}`);
          token.attrSet("target", "_blank");
          token.attrSet("rel", "noopener noreferrer");
        }

        return defaultRender(tokens, idx, options, env, self);
      };
    },
  },

  sitemap: {
    hostname: `https://${GH_USER}.github.io${BASE}`,
  },
});
