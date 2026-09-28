// Refuses to publish while the site still shows intended output for features
// that have not shipped. Replace each `planned: true` entry with captured output.
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

if (planned.length > 0) {
  for (const p of planned) console.error(`src/data/${p.file}:${p.line}: planned output`);
  console.error(`${planned.length} planned entries remain; not publishing.`);
  process.exit(1);
}
