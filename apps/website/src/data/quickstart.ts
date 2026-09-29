// The two-minute setup at /docs/start/quickstart/, one entry per step.
//
// Blocks without `planned` are output captured from the kitchn CLI built from
// PR #96 (#93), and for `house init` from the #98 branch built with
// KITCHEN_COMMIT set to its base commit, against a scratch registry and a
// checkout whose origin remote is github.com/acme/app. Typed answers are shown
// on their prompt lines and the printed configuration is abridged. Paths are
// shortened to ~/.kitchn, the 40-character pins to 7, and long reports are
// trimmed; the wording is otherwise the CLI's own.
//
// Blocks marked `planned: true` show the intended behavior of features that
// are still being built. Replace each with captured output when its feature
// lands; deploys list any that remain. Orca screenshots must come from a real
// session: until one is captured, its entry stays planned and the page shows
// the dialog's fields as text instead.

import type { TermLine } from "./terminal";

const cmd = (text: string): TermLine => ({ tone: "cmd", text });
const out = (text: string): TermLine => ({ tone: "out", text });
const dim = (text: string): TermLine => ({ tone: "dim", text });
const ok = (text: string): TermLine => ({ tone: "ok", text });
const warn = (text: string): TermLine => ({ tone: "warn", text });
const you = (text: string): TermLine => ({ tone: "you", text });

/** Terminal session: a command and what it prints. */
export interface TermBlock {
  kind: "term";
  label: string;
  planned?: true;
  lines: TermLine[];
}

/** A file the reader writes or reviews. */
export interface FileBlock {
  kind: "file";
  label: string;
  planned?: true;
  body: string;
}

/** Fields of an Orca dialog, shown as text until a real screenshot exists. */
export interface FormBlock {
  kind: "form";
  label: string;
  fields: { name: string; value: string }[];
  screenshot: { planned: true; caption: string } | { src: string; alt: string };
}

export type Block = TermBlock | FileBlock | FormBlock;

export interface Step {
  id: string;
  title: string;
  /** One or two sentences: what this step does and why. */
  lede: string;
  blocks: Block[];
  /** How to tell the step worked. */
  worked: string[];
  /** Issue that ships the planned parts, shown next to the step. */
  tracking?: { issue: number; what: string }[];
}

export const steps: Step[] = [
  {
    id: "install",
    title: "Install kitchn",
    lede: "One binary. It keeps its state outside your repositories and starts nothing on its own.",
    blocks: [
      {
        kind: "term",
        label: "macOS / Linux",
        planned: true,
        lines: [
          cmd("curl -fsSL https://getkitchn.com/install.sh | sh"),
          out("Downloaded kitchn 0.1.0 for aarch64-apple-darwin; SHA-256 verified."),
          ok("Installed kitchn to ~/.local/bin/kitchn"),
        ],
      },
      {
        kind: "term",
        label: "Cargo",
        planned: true,
        lines: [cmd("cargo install kitchn --locked"), ok("Installed package `kitchn v0.1.0`")],
      },
      {
        kind: "term",
        label: "From source, today",
        lines: [
          cmd("git clone https://github.com/lemarier/kitchen && cd kitchen"),
          cmd("just install"),
          ok("recording commit <sha>"),
          cmd("kitchn --version"),
          ok("kitchn 0.1.0"),
        ],
      },
    ],
    worked: [
      "`kitchn --version` prints a version.",
      "`kitchn --help` lists `house`, `init` and `adopt`.",
    ],
    tracking: [{ issue: 18, what: "install script, crate and the kitchn binary name" }],
  },
  {
    id: "skills",
    title: "Put /kitchn in your agents",
    lede: "The /kitchn skill is how you talk to kitchn from inside a session. Install it once per agent you use.",
    blocks: [
      {
        kind: "term",
        label: "Claude Code and Codex",
        planned: true,
        lines: [
          cmd("kitchn skill install claude-code codex"),
          ok("Claude Code: installed /kitchn in ~/.claude/skills/kitchn"),
          ok("Codex: installed /kitchn in ~/.codex/skills/kitchn"),
          dim("The skill reads the house from the repository you open. It grants nothing."),
        ],
      },
    ],
    worked: [
      "Type `/kitchn` in a new Claude Code or Codex session and the skill shows up.",
      "Nothing changed in any repository.",
    ],
    tracking: [{ issue: 17, what: "the /kitchn skill and its installer" }],
  },
  {
    id: "orchestrator",
    title: "Register your house",
    lede: "Install the agents you want to cook with, then run `kitchn house init` from a checkout. It asks only what it can't infer, shows the result, and registers your house.",
    blocks: [
      {
        kind: "term",
        label: "Register the house",
        lines: [
          cmd("kitchn house init"),
          out("Registry directory [~/.kitchn]:"),
          you("House name: acme"),
          out("Repositories kitchn may work in [acme/app, from this checkout]:"),
          out("Where kitchn may post [acme/app, the repositories]:"),
          out("Found Claude Code and Codex on PATH. Who works each station?"),
          out("  Sous-chef     plans and splits work  [Claude Code]:"),
          out("  Station cook  writes the code        [Codex]:"),
          out("  Expediter     reviews at the pass    [Claude Code]:"),
          warn("No house-scoped GitHub access to read required checks (see --github-requester)."),
          you("Required checks: test, lint"),
          out("Required reviewers [expediter]:"),
          dim('{ "house": "acme", "repositories": ["acme/app"], "grants": [], … }'),
          you("Register house acme in ~/.kitchn? [y/N]: y"),
          ok("Registered house acme in ~/.kitchn and pinned the default guidance at ed7da17."),
          ok("No authority or workflows activated."),
          dim("Saved your answers as ~/.kitchn/houses/acme.json. Review it any time."),
        ],
      },
    ],
    worked: [
      "Press Enter to take a default in brackets; only the house name and the required checks needed typing.",
      "`house init` says the house is registered and nothing was activated.",
      "Scripting it? Every question has a flag, such as `--house acme --required-checks test,lint --yes`. Prefer a file you review first? `kitchn house init --registry ~/.kitchn --config house.json` still works.",
    ],
  },
  {
    id: "orca",
    title: "Start in Orca",
    lede: "Create a worktree and pick its agent. Orca opens that agent in the new worktree: type `/kitchn`. The first time in a repository, the skill binds it to your house. kitchn writes nothing into the repository.",
    blocks: [
      {
        kind: "form",
        label: "Orca: Create worktree",
        fields: [
          { name: "Project", value: "app" },
          { name: "Run on", value: "Local Mac" },
          { name: "Create from", value: "origin/main" },
          { name: "Agent", value: "Codex" },
        ],
        screenshot: {
          planned: true,
          caption: "Orca's Create worktree dialog, captured from a real session",
        },
      },
      {
        kind: "term",
        label: "Your Codex session",
        planned: true,
        lines: [
          cmd("/kitchn"),
          out("Orca worktree app-export, project github:acme/app."),
          warn("acme/app isn't bound to a house yet, and acme claims it. Bind it to acme?"),
          you("yes, interactive only for now"),
          ok("Bound acme/app to house acme in the registry; the working tree is unchanged."),
          warn("Doctor: setup incomplete. House-scoped repository access: Unobserved."),
          dim("Ready. Hand me an issue, a pull request or an idea."),
        ],
      },
      {
        kind: "term",
        label: "The same binding from a shell",
        lines: [
          cmd("kitchn house setup --registry ~/.kitchn --house acme --workflows none"),
          ok("Bound acme/app to house acme in the registry; the working tree is unchanged."),
          warn("Doctor: setup incomplete"),
          warn("House-scoped repository access: Unobserved."),
          ok("No workers, schedules, labels, or external actions were activated."),
          cmd("git status --short"),
          dim("(nothing: no kitchn file in the repository)"),
        ],
      },
    ],
    worked: [
      "The skill names the Orca worktree and the repository it resolved.",
      "Setup says the working tree is unchanged, and `git status` agrees.",
      "Doctor names each missing capability or access with its next step.",
      "Starting a new repository instead? `kitchn init app --house acme --template <name>` previews the house template before writing it.",
    ],
    tracking: [
      { issue: 17, what: "the /kitchn skill and its first-run setup" },
      { issue: 93, what: "house bindings outside the repository (PR #96)" },
    ],
  },
  {
    id: "order",
    title: "Run a real order",
    lede: "Write a short issue, let kitchn turn it into a spec, then hand it to the brigade and watch the pass.",
    blocks: [
      {
        kind: "file",
        label: "Issue #72",
        body: `Export orders as CSV

Shop owners want their orders in a spreadsheet. Add a CSV export
to the orders page, with the same filters as the list.`,
      },
      {
        kind: "term",
        label: "Refine it into a spec",
        planned: true,
        lines: [
          cmd("/kitchn issue refine #72"),
          out("Read the orders page, its filters and the last 20 merged PRs."),
          out("Proposed spec with acceptance criteria and 3 sub-issues:"),
          out("  #73 CSV writer for orders          ready"),
          out("  #74 Export endpoint with filters   blocked by #73"),
          out("  #75 Export button on orders page   blocked by #74"),
          warn("One decision needs you: should the export include cancelled orders?"),
          you("no, only active ones"),
          ok("Posted the spec, 3 sub-issues and 2 blocked-by links after your approval."),
        ],
      },
      {
        kind: "term",
        label: "Work it",
        planned: true,
        lines: [
          cmd("/kitchn work #72"),
          out("House acme. #73 is ready; #74 and #75 wait on it."),
          warn("Start a cook on #73 now, and the rest as their blockers merge?"),
          you("yes"),
          ok("Orca: worktree for #73, Codex cooking, Claude Code at the pass."),
          out("10:14 Expediter sent #73 back: no test for an empty order list."),
          out("10:31 #73 passes at exact head 9c41e0a. Waiting for your sign-off."),
          ok("10:33 You merged #73. Started #74."),
          dim("You stay the chef-owner. Merges wait for your sign-off."),
        ],
      },
      {
        kind: "term",
        label: "Later: let it run on its own",
        planned: true,
        lines: [
          cmd("/kitchn schedule pickup every 15 minutes"),
          out("Orca automation: kitchn pickup on acme/app, every 15 min, Codex cooks."),
          out("Doctor: every capability scheduled pickup needs is observed."),
          warn("Create this automation in Orca?"),
          you("create it"),
          ok("Created. It picks up issues labeled agent-ready; you still sign off merges."),
        ],
      },
    ],
    worked: [
      "The issue now has a spec, sub-issues, and blocked-by links you approved.",
      "Orca shows one worktree per ready sub-issue, and blocked ones wait.",
      "Each PR reaches you only after the expediter passes it at its exact head.",
      "A scheduled run is created only after you approve it, and only when doctor has observed every capability it needs.",
    ],
    tracking: [
      { issue: 17, what: "`/kitchn issue refine` and `/kitchn work`" },
      { issue: 8, what: "scheduled pickup" },
    ],
  },
];
