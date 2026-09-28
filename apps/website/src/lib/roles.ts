// Role cards are read from the repository's roles/ directory at build time so
// the site cannot drift from the contracts Kitchen embeds.
const sources = import.meta.glob<string>("../../../../roles/*.md", {
  query: "?raw",
  import: "default",
  eager: true,
});

/** Brigade order and kitchen names, keyed by role card file name. */
const BRIGADE = [
  ["chef-owner", "Chef-owner"],
  ["sous-chef", "Sous-chef"],
  ["station-cook", "Station cook"],
  ["commis", "Commis"],
  ["expediter", "Expediter"],
  ["inspector", "Inspector"],
  ["gardener", "Gardener"],
  ["dishwasher", "Dishwasher"],
] as const;

export interface Role {
  slug: (typeof BRIGADE)[number][0];
  kitchenName: string;
  function: string;
  responsibility: string;
  evidence: string;
  boundary: string;
}

function field(slug: string, text: string, label: string): string {
  const line = text.split("\n").find((l) => l.startsWith(`${label}: `));
  if (line === undefined) {
    throw new Error(`roles/${slug}.md has no "${label}:" line`);
  }
  return line.slice(label.length + 2).trim();
}

function parse(slug: Role["slug"], kitchenName: string): Role {
  const text = sources[`../../../../roles/${slug}.md`];
  if (text === undefined) {
    throw new Error(`roles/${slug}.md is missing`);
  }
  const heading = text.split("\n")[0];
  if (heading === undefined || !heading.startsWith("# ")) {
    throw new Error(`roles/${slug}.md must start with a "# " heading`);
  }
  return {
    slug,
    kitchenName,
    function: heading.slice(2).trim(),
    responsibility: field(slug, text, "Responsibility"),
    evidence: field(slug, text, "Evidence"),
    boundary: field(slug, text, "Boundary"),
  };
}

const known = new Set<string>(BRIGADE.map(([slug]) => `../../../../roles/${slug}.md`));
const unlisted = Object.keys(sources).filter(
  (path) => !path.endsWith("/README.md") && !known.has(path),
);
if (unlisted.length > 0) {
  throw new Error(`Role cards missing from the site's brigade order: ${unlisted.join(", ")}`);
}

export const roles: readonly Role[] = BRIGADE.map(([slug, name]) => parse(slug, name));
