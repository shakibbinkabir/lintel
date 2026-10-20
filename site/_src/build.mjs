// Writes the static pages of site/ from the fragments in pages/. Run from anywhere: node site/_src/build.mjs
// The site itself needs no build step: this only saves typing the shared header and footer eight times.
import { readFileSync, writeFileSync } from "node:fs";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const out = process.argv[2] ?? join(here, "..");
const ORIGIN = "https://lintel.thisissbk.com/";
const REPO = "https://github.com/shakibbinkabir/lintel";

const NAV = [["features.html", "Features"], ["screenshots.html", "Screenshots"], ["guide.html", "Guide"], ["docs.html", "Docs"], ["contribute.html", "Contribute"], ["about.html", "About"]];
const PAGES = [
  { file: "index.html", frag: "home", title: "Lintel: an island for Windows, hidden until something needs you", desc: "A free, open-source island at the top of your Windows screen: answer your coding agents, control music and reach your tools, then it gets out of the way." },
  { file: "features.html", frag: "features", title: "Features: everything Lintel can do | Lintel", desc: "The whole feature list of Lintel 2.0.0, group by group: coding agents (also in WSL and over SSH), chat, media, window layout, capture, cleaner, voice and more. Each one has a switch." },
  { file: "screenshots.html", frag: "screenshots", title: "Screenshots of Lintel on Windows | Lintel", desc: "See Lintel at work: the island with a permission card from a coding agent, the player, the clipboard, the mixer, capture tools and the settings window." },
  { file: "download.html", frag: "download", title: "Download Lintel for Windows 10 and 11 | Lintel", desc: "Download the free Lintel installer for Windows 10 and 11. No administrator rights needed. What Windows will ask, how to remove it, and how to build it yourself." },
  { file: "guide.html", frag: "guide", title: "Guide: your first day with Lintel | Lintel", desc: "A plain guide for new users: bring the island out, connect a coding agent, answer its first request, turn features on and off, and try the voice assistant." },
  { file: "docs.html", frag: "docs", title: "Documentation: the island, shortcuts, privacy, voice | Lintel", desc: "How Lintel works in detail: the island's rules, every default shortcut, what leaves your computer, the hardware helper, the voice assistant and building from source." },
  { file: "contribute.html", frag: "contribute", title: "Contribute to Lintel: start, rules and wanted help | Lintel", desc: "Everyone is welcome. Build Lintel from source in four commands, learn how a feature is one folder, read the rules in plain words and see what help is wanted most." },
  { file: "about.html", frag: "about", title: "About Lintel: why it exists and who to thank | Lintel", desc: "Why Lintel was built for Windows, who made it, the three projects it grew from, its GPL-3.0-or-later licence, and how to report a problem." },
  { file: "404.html", frag: "404", title: "Page not found | Lintel", desc: "This page does not exist.", base: "/", noindex: true },
];

const BOT = `<svg class="bot" viewBox="0 0 58 58" aria-hidden="true"><defs><linearGradient id="bot-body" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="#b0beff"/><stop offset=".55" stop-color="#8ea2ff"/><stop offset="1" stop-color="#616ead"/></linearGradient></defs><g id="bot"><circle cx="29" cy="29" r="26.7" fill="url(#bot-body)"/><rect x="8.7" y="15.1" width="40.6" height="16.2" rx="8.1" fill="#070708" fill-opacity=".94"/><rect class="bot-bar" x="20.3" y="21" width="17.4" height="4.4" rx="2.2" fill="#fff"/></g></svg>`;

function page(p) {
  const b = p.base ?? "";
  const url = ORIGIN + (p.file === "index.html" ? "" : p.file.replace(/\.html$/, ""));
  const body = readFileSync(join(here, "pages", p.frag + ".html"), "utf8").replaceAll("{REPO}", REPO).replaceAll("{B}", b).split(/(<pre[\s\S]*?<\/pre>|<code[\s\S]*?<\/code>|<[^>]+>)/).map((s, i) => i % 2 ? s : s.replace(/"([^"]*)"/g, "“$1”").replace(/'/g, "’").replace(/(\d) (MB|GB|px)\b/g, "$1 $2")).join("");
  const links = NAV.map(([href, label]) => `<a href="${b}${href}"${href === p.file ? ' aria-current="page"' : ""}>${label}</a>`).join("\n      ");
  const ld = p.file !== "index.html" ? "" : `
<script type="application/ld+json">
{"@context":"https://schema.org","@type":"SoftwareApplication","name":"Lintel","url":"${ORIGIN}","description":"${p.desc}","applicationCategory":"UtilitiesApplication","operatingSystem":"Windows 10, Windows 11","softwareVersion":"2.0.0","license":"https://www.gnu.org/licenses/gpl-3.0.html","downloadUrl":"${REPO}/releases/download/v2.0.0/lintel-2.0.0-setup.exe","offers":{"@type":"Offer","price":"0","priceCurrency":"USD"},"author":{"@type":"Person","name":"Shakib Bin Kabir","url":"https://github.com/shakibbinkabir"}}
</script>`;
  return `<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="color-scheme" content="dark">
<title>${p.title}</title>
<meta name="description" content="${p.desc}">
${p.noindex ? '<meta name="robots" content="noindex">' : `<link rel="canonical" href="${url}">`}
<meta property="og:type" content="website">
<meta property="og:site_name" content="Lintel">
<meta property="og:url" content="${url}">
<meta property="og:title" content="${p.title.replace(" | Lintel", "")}">
<meta property="og:description" content="${p.desc}">
<meta property="og:image" content="${ORIGIN}img/og.png">
<meta property="og:image:width" content="1200">
<meta property="og:image:height" content="630">
<meta name="twitter:card" content="summary_large_image">
<meta name="theme-color" content="#0a0a0b">
<link rel="icon" href="${b}favicon.ico" sizes="32x32">
<link rel="icon" href="${b}img/icon-128.png" type="image/png">
<link rel="apple-touch-icon" href="${b}img/icon-256.png">
<link rel="preload" href="${b}fonts/Geist-Variable.woff2" as="font" type="font/woff2" crossorigin>
<link rel="stylesheet" href="${b}css/site.css">
<script>document.documentElement.classList.add("js")</script>
<script defer src="${b}js/site.js"></script>${ld}
</head>
<body>
<a class="skip" href="#main">Skip to content</a>

<header class="nav">
  <nav class="nav-in wrap" aria-label="Main">
    <a class="wordmark" href="${b}index.html" aria-label="Lintel, home" translate="no">${BOT}Lintel</a>
    <button type="button" class="nav-toggle" aria-label="Menu" aria-expanded="false" aria-controls="nav-links"><svg class="ico" viewBox="0 0 24 24" aria-hidden="true"><path d="M4 8.5h16M4 15.5h16"/></svg></button>
    <div class="nav-links" id="nav-links">
      ${links}
      <a class="nav-get" href="${b}download.html"${p.file === "download.html" ? ' aria-current="page"' : ""}>Download</a>
    </div>
  </nav>
</header>

<main id="main" tabindex="-1">
${body.trim()}
</main>

<footer class="footer wrap">
  <nav aria-label="Footer">
    <a href="${b}index.html">Home</a>
    ${NAV.map(([href, label]) => `<a href="${b}${href}">${label === "Docs" ? "Documentation" : label}</a>`).join("\n    ")}
    <a href="${b}download.html">Download</a>
    <a href="${REPO}">GitHub</a>
    <a href="${REPO}/releases">Releases</a>
    <a href="${b}download.html#signing">Code signing policy</a>
  </nav>
  <p>Lintel is free to use, to change and to share under the <a href="${REPO}/blob/main/LICENSE">GNU General Public License, version 3 or later</a>. It is an independent project. The work it builds on is credited on the <a href="${b}about.html">About</a> page and in <a href="${REPO}/blob/main/NOTICE.md">NOTICE.md</a>.</p>
  <p>Made by <a href="https://github.com/shakibbinkabir">Shakib Bin Kabir</a>. This site sets no cookies. The download page asks GitHub which release is the latest; nothing else is loaded from anywhere else.</p>
</footer>

</body>
</html>
`;
}

// The pages are served without ".html" (site/.htaccess): the links say so, the files keep their names.
const clean = (html) =>
  html.replace(/href="((?:\.\/|\/)?)(index|features|screenshots|download|guide|docs|contribute|about)\.html(#[^"]*)?"/g, (_, pre, name, hash = "") => `href="${name === "index" ? pre || "./" : pre + name}${hash}"`);
for (const p of PAGES) writeFileSync(join(out, p.file), clean(page(p)));
writeFileSync(join(out, "robots.txt"), `User-agent: *\nAllow: /\n\nSitemap: ${ORIGIN}sitemap.xml\n`);
writeFileSync(join(out, "sitemap.xml"), `<?xml version="1.0" encoding="UTF-8"?>\n<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">\n${PAGES.filter((p) => !p.noindex).map((p) => `  <url><loc>${ORIGIN}${p.file === "index.html" ? "" : p.file.replace(/\.html$/, "")}</loc><lastmod>2026-10-12</lastmod></url>`).join("\n")}\n</urlset>\n`);
writeFileSync(join(out, "img", "mark.svg"), `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 58 58"><defs><linearGradient id="b" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="#b0beff"/><stop offset=".55" stop-color="#8ea2ff"/><stop offset="1" stop-color="#616ead"/></linearGradient></defs><circle cx="29" cy="29" r="26.7" fill="url(#b)"/><rect x="8.7" y="15.1" width="40.6" height="16.2" rx="8.1" fill="#070708" fill-opacity=".94"/><rect x="20.3" y="21" width="17.4" height="4.4" rx="2.2" fill="#fff"/></svg>\n`);
console.log("wrote", PAGES.length, "pages");
