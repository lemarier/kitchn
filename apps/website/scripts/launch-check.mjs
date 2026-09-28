// Lists content that shows intended output for features that have not shipped.
// Replace each `planned: true` entry with captured output as its feature lands.
import { readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";

const dir = new URL("../src/data/", import.meta.url).pathname;
const planned = readdirSync(dir)
  .filter((file) => file.endsWith(".ts"))
  .flatMap((file) =>
    readFileSync(join(dir, file), "utf8")
      .split("\n")
      .map((line, i) => ({ file, line: i + 1, text: line }))
      .filter(({ text }) => /^\s*planned: true,/.test(text)),
  );

for (const p of planned) console.warn(`src/data/${p.file}:${p.line}: planned output`);
if (planned.length > 0)
  console.warn(`${planned.length} planned entries still show intended output.`);
