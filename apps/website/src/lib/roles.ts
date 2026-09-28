// Role cards are read from the repository's roles/ directory at build time so
// the site cannot drift from the contracts Kitchen embeds.
const sources = import.meta.glob<string>("../../../../roles/*.md", {
  query: "?raw",
  import: "default",
  eager: true,
});

/** Brigade order, kitchen names and one-line jobs, keyed by role card file name. */
const BRIGADE = [
  ["chef-owner", "Chef-owner", "You. Sets the house rules and makes the calls evidence can't."],
  ["sous-chef", "Sous-chef", "Runs the line: picks up ready work, hands it out, keeps count."],
  ["station-cook", "Station cook", "Builds one task in its own worktree, with proof it works."],
  ["commis", "Commis", "Preps research and small subtasks for a named station."],
  ["expediter", "Expediter", "Checks every plate at the pass before it leaves the kitchen."],
  ["inspector", "Inspector", "Tastes what already shipped and reports what's off."],
  ["gardener", "Gardener", "Keeps the backlog clean: specs, dependencies, duplicates."],
  ["dishwasher", "Dishwasher", "Clears finished worktrees, and only finished ones."],
] as const;

export interface Role {
  slug: (typeof BRIGADE)[number][0];
  kitchenName: string;
  job: string;
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

function parse(slug: Role["slug"], kitchenName: string, job: string): Role {
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
    job,
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

export const roles: readonly Role[] = BRIGADE.map(([slug, name, job]) => parse(slug, name, job));
