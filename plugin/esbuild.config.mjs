import esbuild from "esbuild";
import fs from "fs";
import process from "process";
import builtins from "builtin-modules";

// manifest.json lives at the repo root (Obsidian's community-plugin tooling reads
// it there). Copy it next to main.js so plugin/ holds the three files Obsidian
// loads and the release workflow uploads.
fs.copyFileSync("../manifest.json", "manifest.json");

const banner = `/*
Samsyn — conflict-free live sync for Obsidian.
This is a generated bundle; see https://github.com/mrboatsman/samsyn for sources.
*/
`;

const prod = process.argv[2] === "production";

const context = await esbuild.context({
  banner: { js: banner },
  entryPoints: ["src/main.ts"],
  bundle: true,
  external: [
    "obsidian",
    "electron",
    "@codemirror/autocomplete",
    "@codemirror/collab",
    "@codemirror/commands",
    "@codemirror/language",
    "@codemirror/lint",
    "@codemirror/search",
    "@codemirror/state",
    "@codemirror/view",
    "@lezer/common",
    "@lezer/highlight",
    "@lezer/lr",
    ...builtins,
  ],
  format: "cjs",
  target: "es2020",
  logLevel: "info",
  sourcemap: prod ? false : "inline",
  treeShaking: true,
  outfile: "main.js",
  minify: prod,
});

if (prod) {
  await context.rebuild();
  process.exit(0);
} else {
  await context.watch();
}
