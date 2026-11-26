import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { execFileSync } from "node:child_process";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const require = createRequire(import.meta.url);

const entryJs = path.join(__dirname, "app.js");
const cssPath = path.join(__dirname, "styles.css");
const templatePath = path.join(__dirname, "template.html");
const distDir = path.join(__dirname, "dist");
const outHtml = path.join(distDir, "panel.html");
const BOOT_TOKEN = "/*__COLAB_MCP_BOOT__*/";
const MODULES = ["bridge.js", "i18n.js"];
const SIZE_FAIL_BYTES = 43_500;
const SIZE_WARN_BYTES = 43_000;

function tryLoadEsbuild() {
  if (process.env.COLAB_MCP_BUNDLER === "bun") return null;
  for (const mod of [process.env.ESBUILD_PATH, "esbuild"].filter(Boolean)) {
    try { return require(mod); } catch {}
  }
  return null;
}

function tryCliBundle() {
  let tmpDir = "";
  try {
    tmpDir = fs.mkdtempSync(path.join(os.tmpdir(), "colab-mcp-pack-"));
    for (const f of [...MODULES, "app.js"]) {
      fs.writeFileSync(path.join(tmpDir, f), fs.readFileSync(path.join(__dirname, f), "utf8"), "utf8");
    }
    const tmpEntry = path.join(tmpDir, "app.js");
    for (const esbBin of ["esbuild", "/opt/homebrew/bin/esbuild", "/usr/local/bin/esbuild"]) {
      try {
        const out = execFileSync(
          esbBin,
          [tmpEntry, "--bundle", "--minify", "--tree-shaking=true", "--charset=utf8", "--legal-comments=none", "--format=iife", "--platform=browser", "--target=es2022"],
          { encoding: "utf8", maxBuffer: 4 * 1024 * 1024, stdio: ["ignore", "pipe", "ignore"] }
        );
        if (out.trim()) return { code: out.trim(), bundler: "esbuild-cli" };
      } catch {}
    }
    for (const bunBin of ["bun", "/opt/homebrew/bin/bun", path.join(os.homedir(), ".bun", "bin", "bun")]) {
      try {
        const out = execFileSync(
          bunBin,
          ["build", tmpEntry, "--bundle", "--minify", "--format=iife", "--target=browser"],
          { encoding: "utf8", maxBuffer: 4 * 1024 * 1024, stdio: ["ignore", "pipe", "ignore"] }
        );
        if (out.trim()) return { code: out.trim(), bundler: "bun" };
      } catch {}
    }
    return null;
  } catch {
    return null;
  } finally {
    if (tmpDir) try { fs.rmSync(tmpDir, { recursive: true, force: true }); } catch {}
  }
}

function cleanCss(css) {
  return css
    .replace(/\/\*[\s\S]*?\*\//g, "")
    .replace(/--buncss-[a-z0-9-]+:[^;}"']+;?/gi, "")
    .replace(/\s+/g, " ")
    .replace(/\s*([{}:;,>+~])\s*/g, "$1")
    .replace(/;}/g, "}")
    .replace(/<\/style/gi, "<\\/style")
    .trim();
}

function compactJsFallback(src) {
  return src
    .replace(/^\s*\/\/.*$/gm, "")
    .replace(/^\s+/gm, "")
    .replace(/\n{2,}/g, "\n")
    .trim();
}

function fallbackBundleJs() {
  const parts = MODULES.map((f) =>
    fs.readFileSync(path.join(__dirname, f), "utf8").replace(/^\s*export\s+/gm, "")
  );
  const appSrc = fs
    .readFileSync(entryJs, "utf8")
    .replace(/^\s*import\s+[\s\S]*?from\s+["']\.\/[^"']+["'];?\s*/gm, "");
  return `(()=>{${compactJsFallback(`${parts.join("\n")}\n${appSrc}`)}})();`;
}

async function main() {
  const rawCss = fs.readFileSync(cssPath, "utf8");
  const esbuild = tryLoadEsbuild();
  let jsCode;
  let cssCode;
  let bundler = "fallback";

  if (esbuild) {
    bundler = "esbuild";
    const res = await esbuild.build({
      entryPoints: [entryJs],
      bundle: true,
      minify: true,
      treeShaking: true,
      charset: "utf8",
      legalComments: "none",
      format: "iife",
      platform: "browser",
      target: ["es2022"],
      write: false,
    });
    jsCode = res.outputFiles[0].text.trim().replace(/<\/script/gi, "<\\/script");
    const minCss = await esbuild.transform(rawCss, { loader: "css", minify: true, charset: "utf8", legalComments: "none" });
    cssCode = cleanCss(minCss.code);
  } else {
    const cli = tryCliBundle();
    if (cli) {
      bundler = cli.bundler;
      jsCode = cli.code.replace(/<\/script/gi, "<\\/script");
    } else {
      jsCode = fallbackBundleJs().replace(/<\/script/gi, "<\\/script");
    }
    cssCode = cleanCss(rawCss);
  }

  const template = fs.readFileSync(templatePath, "utf8").replace(/>\s+</g, "><").replace(/\r?\n/g, "").trim();
  if (!template.includes(BOOT_TOKEN)) throw new Error(`Missing ${BOOT_TOKEN} placeholder in template.html`);

  const html = template.replace("/*__INLINE_CSS__*/", () => cssCode).replace("/*__INLINE_JS__*/", () => jsCode);
  const jsBytes = Buffer.byteLength(jsCode, "utf8");
  const cssBytes = Buffer.byteLength(cssCode, "utf8");
  const totalBytes = Buffer.byteLength(html, "utf8");

  if (totalBytes > SIZE_FAIL_BYTES) throw new Error(`Panel bundle ${totalBytes} B exceeds hard ${SIZE_FAIL_BYTES} B limit`);
  if (totalBytes > SIZE_WARN_BYTES) console.warn(`WARN: Panel bundle ${totalBytes} B exceeds ${SIZE_WARN_BYTES} B warning threshold`);

  fs.mkdirSync(distDir, { recursive: true });
  fs.writeFileSync(outHtml, html, "utf8");
  console.log(`Built ${outHtml} (${totalBytes} B / ${(totalBytes / 1024).toFixed(1)} KiB total: JS ${jsBytes} B + CSS ${cssBytes} B, bundler=${bundler})`);
}

main().catch((err) => { console.error(err); process.exit(1); });
