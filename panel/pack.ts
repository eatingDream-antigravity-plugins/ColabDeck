import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { execFileSync } from "node:child_process";
import { createRequire, stripTypeScriptTypes } from "node:module";
import { fileURLToPath } from "node:url";
import { assertPanelInvariants, DEFAULT_MAX_BYTES, WARN_BYTES } from "./kit/guard.ts";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const require = createRequire(import.meta.url);

const entryTs = path.join(__dirname, "app.ts");
const tokensCssPath = path.join(__dirname, "kit", "tokens.css");
const cssPath = path.join(__dirname, "styles.css");
const templatePath = path.join(__dirname, "template.html");
const distDir = path.join(__dirname, "dist");
const outHtml = path.join(distDir, "panel.html");
const BOOT_TOKEN = "/*__COLAB_MCP_BOOT__*/";
const MODULES = ["kit/bridge.ts", "i18n.ts", "sync.ts"];
const SIZE_FAIL_BYTES = DEFAULT_MAX_BYTES;
const SIZE_WARN_BYTES = WARN_BYTES;

type EsbuildModule = typeof import("esbuild");

interface BundleResult {
  code: string;
  bundler: string;
}

function tryLoadEsbuild(): EsbuildModule | null {
  if (process.env.COLAB_MCP_BUNDLER === "bun") return null;
  for (const mod of [process.env.ESBUILD_PATH, "esbuild"].filter((m): m is string => Boolean(m))) {
    try {
      return require(mod) as EsbuildModule;
    } catch {
      // Optional local esbuild path unavailable.
    }
  }
  return null;
}

function tryCliBundle(): BundleResult | null {
  let tmpDir = "";
  try {
    tmpDir = fs.mkdtempSync(path.join(os.tmpdir(), "colab-mcp-pack-"));
    for (const f of [...MODULES, "app.ts"]) {
      const dst = path.join(tmpDir, f);
      fs.mkdirSync(path.dirname(dst), { recursive: true });
      fs.writeFileSync(dst, fs.readFileSync(path.join(__dirname, f), "utf8"), "utf8");
    }
    const tmpEntry = path.join(tmpDir, "app.ts");
    for (const esbBin of ["esbuild", "/opt/homebrew/bin/esbuild", "/usr/local/bin/esbuild"]) {
      try {
        const out = execFileSync(
          esbBin,
          [tmpEntry, "--bundle", "--minify", "--tree-shaking=true", "--charset=utf8", "--legal-comments=none", "--format=iife", "--platform=browser", "--target=es2022"],
          { encoding: "utf8", maxBuffer: 4 * 1024 * 1024, stdio: ["ignore", "pipe", "ignore"] }
        );
        if (out.trim()) return { code: out.trim(), bundler: "esbuild-cli" };
      } catch {
        // Try next bundler candidate.
      }
    }
    for (const bunBin of ["bun", "/opt/homebrew/bin/bun", path.join(os.homedir(), ".bun", "bin", "bun")]) {
      try {
        const out = execFileSync(
          bunBin,
          ["build", tmpEntry, "--bundle", "--minify", "--format=iife", "--target=browser"],
          { encoding: "utf8", maxBuffer: 4 * 1024 * 1024, stdio: ["ignore", "pipe", "ignore"] }
        );
        if (out.trim()) return { code: out.trim(), bundler: "bun" };
      } catch {
        // Try next bundler candidate.
      }
    }
    return null;
  } catch {
    return null;
  } finally {
    if (tmpDir) {
      try {
        fs.rmSync(tmpDir, { recursive: true, force: true });
      } catch {
        // Best-effort temp cleanup.
      }
    }
  }
}

function cleanCss(css: string): string {
  return css
    .replace(/\/\*[\s\S]*?\*\//g, "")
    .replace(/--buncss-[a-z0-9-]+:[^;}"']+;?/gi, "")
    .replace(/\s+/g, " ")
    .replace(/\s*([{}:;,>+~])\s*/g, "$1")
    .replace(/;}/g, "}")
    .replace(/<\/style/gi, "<\\/style")
    .trim();
}

function compactJsFallback(src: string): string {
  return src
    .replace(/^\s*\/\/.*$/gm, "")
    .replace(/^\s+/gm, "")
    .replace(/\n{2,}/g, "\n")
    .trim();
}

function stripTsModule(src: string): string {
  const js = stripTypeScriptTypes(src, { mode: "strip" });
  return js
    .replace(/^\s*import\s+[\s\S]*?from\s+["']\.\/[^"']+["'];?\s*/gm, "")
    .replace(/^\s*export\s+/gm, "");
}

function fallbackBundleJs(): string {
  const parts = MODULES.map((f) => stripTsModule(fs.readFileSync(path.join(__dirname, f), "utf8")));
  const appSrc = stripTsModule(fs.readFileSync(entryTs, "utf8"));
  return `(()=>{${compactJsFallback(`${parts.join("\n")}\n${appSrc}`)}})();`;
}

async function main(): Promise<void> {
  const rawCss = `${fs.readFileSync(tokensCssPath, "utf8")}\n${fs.readFileSync(cssPath, "utf8")}`;
  const esbuild = tryLoadEsbuild();
  let jsCode: string;
  let cssCode: string;
  let bundler = "fallback";

  if (esbuild) {
    bundler = "esbuild";
    const res = await esbuild.build({
      entryPoints: [entryTs],
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
  const report = assertPanelInvariants(html, {
    maxBytes: SIZE_FAIL_BYTES,
    hardMaxBytes: SIZE_FAIL_BYTES,
    warnBytes: SIZE_WARN_BYTES,
    bootPlaceholder: BOOT_TOKEN,
  });
  for (const w of report.warnings) console.warn(`WARN: ${w}`);

  fs.mkdirSync(distDir, { recursive: true });
  fs.writeFileSync(outHtml, html, "utf8");
  console.log(`Built ${outHtml} (${report.bytes} B / ${(report.bytes / 1024).toFixed(1)} KiB total: JS ${jsBytes} B + CSS ${cssBytes} B, bundler=${bundler})`);
}

main().catch((err: unknown) => {
  console.error(err);
  process.exit(1);
});
