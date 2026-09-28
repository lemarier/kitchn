// Entries without `planned` are output captured from the kitchn CLI against a
// scratch registry and the repository's example template. Paths are shortened
// to ~/.kitchn and long doctor reports are trimmed; the wording is otherwise
// the CLI's own.
//
// Entries marked `planned: true` show the intended behavior of features that
// are still being built (see issue #1). Replace each with captured output when
// its feature lands; `just website-deploy` refuses to publish while any remain.

export type Tone = "cmd" | "out" | "dim" | "ok" | "warn";
export interface TermLine {
  tone: Tone;
  text: string;
}
export type Group = "In your session" | "On the line" | "Setup";
export interface TermCommand {
  id: string;
  group: Group;
  label: string;
  planned?: true;
  summary: string;
  docs: string;
  lines: TermLine[];
}

const cmd = (text: string): TermLine => ({ tone: "cmd", text });
const out = (text: string): TermLine => ({ tone: "out", text });
const dim = (text: string): TermLine => ({ tone: "dim", text });
const ok = (text: string): TermLine => ({ tone: "ok", text });
const warn = (text: string): TermLine => ({ tone: "warn", text });

export const commands: TermCommand[] = [
  {
    id: "work",
    group: "In your session",
    label: "/kitchn work #58",
    planned: true,
    summary: "Hand it an issue from your worktree",
    docs: "/docs/start/introduction/",
    lines: [
      cmd("/kitchn work #58"),
      out("House acme, repository acme/app, guidance pinned at 4f2a9c1."),
      out("#58 Add a retry budget to pickup. Blocked by #57: done."),
      out("Two independent parts found. Proposed sub-issues:"),
      out("  #58.1 Budget type and validation      owns crates/kitchen/src/budget.rs"),
      out("  #58.2 Enforce the budget in pickup    blocked by #58.1"),
      warn("Create these sub-issues and their blocked-by link? [y/N] y"),
      ok("Created #75 and #76. Fanning out on Orca: 1 cook now, 1 waiting on #75."),
      dim("You stay the owner. Nothing merges without your sign-off."),
    ],
  },
  {
    id: "pr",
    group: "In your session",
    label: "/kitchn pr #61",
    planned: true,
    summary: "Review, repair or judge one pull request",
    docs: "/docs/concepts/authority/",
    lines: [
      cmd("/kitchn pr #61"),
      out("Exact head 3b04d17 on base main@9e1c0aa (fresh)."),
      ok("Required checks  tauri-check, specta-types     passed"),
      ok("Reviewer         desktop-reviewer             approved at 3b04d17"),
      ok("Findings         1 raised, 1 resolved         none open"),
      warn("Merge grant      not held by this session"),
      out("Verdict: ready. Merge needs the chef-owner's sign-off for 3b04d17."),
      dim("If the head moves, this verdict is void."),
    ],
  },
  {
    id: "refine",
    group: "In your session",
    label: "/kitchn issue refine #72",
    planned: true,
    summary: "Turn a rough idea into a ready issue",
    docs: "/docs/start/introduction/",
    lines: [
      cmd("/kitchn issue refine #72"),
      out("Read #72, 3 linked issues and 14 commits touching crates/kitchen/src/trust."),
      out("Sharpened acceptance criteria: 2 added, 1 made testable."),
      out("Dependency found: blocked by #12 (trust records)."),
      warn("One product decision needs you:"),
      warn("  Should work type be derived from labels or from changed paths?"),
      dim("Nothing is posted until you approve the final text."),
    ],
  },
  {
    id: "pickup",
    group: "On the line",
    label: "kitchn schedule pickup",
    planned: true,
    summary: "Let the sous-chef pick up ready issues",
    docs: "/docs/concepts/houses/",
    lines: [
      cmd("kitchn schedule add pickup --repository acme/app --every 30m"),
      ok("Backend orca supports every capability pickup needs."),
      out("House budget: 30m minimum interval, 40 agent-hours per week. Within limits."),
      out("One consumer per repository: no other pickup schedule found."),
      ok("Scheduled pickup for acme/app. First run 14:30."),
      dim("Pause with kitchn schedule pause pickup. Interactive work is unaffected."),
    ],
  },
  {
    id: "dishwasher",
    group: "On the line",
    label: "kitchn dishwasher",
    planned: true,
    summary: "Clean up only what's provably done",
    docs: "/docs/concepts/authority/",
    lines: [
      cmd("kitchn dishwasher --preview"),
      out("7 worktrees and 5 sessions inspected."),
      ok("  eligible  kitchn-54    merged, settled, clean, commits preserved"),
      ok("  eligible  kitchn-53    merged, settled, clean, commits preserved"),
      warn("  keep      kitchn-58    active worker"),
      warn("  keep      kitchn-49    untracked files in tests/"),
      warn("  keep      spike-auth   ownership unknown"),
      out("2 eligible, 3 kept. Nothing removed; rerun with --apply after review."),
    ],
  },
  {
    id: "trust",
    group: "On the line",
    label: "kitchn trust",
    planned: true,
    summary: "See what each station has earned",
    docs: "/docs/concepts/authority/",
    lines: [
      cmd("kitchn trust --repository acme/app"),
      out("Station  Work type  Model        Runs  First pass  Findings  Reverts"),
      out("cook     bugfix     codex        24    92%         2         0"),
      out("cook     feature    codex        9     67%         5         1"),
      out("review   any        claude-code  33    n/a         7 raised  -"),
      ok("cook/bugfix meets the house threshold to run unattended."),
      warn("cook/feature does not: 9 of 20 runs, 1 attributed revert."),
      dim("Graduation needs your decision. Evidence never grants it on its own."),
    ],
  },
  {
    id: "setup",
    group: "Setup",
    label: "kitchn house setup",
    summary: "Adopt a repository",
    docs: "/docs/start/quickstart/",
    lines: [
      cmd(
        "kitchn house setup --registry ~/.kitchn --repository acme/app --house acme --workflows pickup,gate",
      ),
      ok("Adopted .kitchen.json for acme/app."),
      out("House: acme"),
      out("Repository: acme/app"),
      warn("Doctor: setup incomplete"),
      dim("…"),
      ok("No workers, schedules, labels, or external actions were activated."),
    ],
  },
  {
    id: "doctor",
    group: "Setup",
    label: "kitchn house doctor",
    summary: "See exactly what's missing",
    docs: "/docs/reference/cli/",
    lines: [
      cmd("kitchn house doctor --registry ~/.kitchn"),
      out("House: acme"),
      out("Repository: acme/app"),
      warn("Doctor: setup incomplete"),
      out(""),
      warn("Label agent-ready: Unobserved (workflows: pickup)."),
      dim(
        "Next: Use the house-scoped GitHub integration to read this repository's labels, then rerun doctor with that observation.",
      ),
      dim("… 4 more labels (trimmed)"),
      out(""),
      warn(
        "Scheduled gate requires: schedule.precheck, schedule.single_consumer, schedule.run_timeout, worker.launch_readiness, house.credentials.",
      ),
      dim(
        "Next: Configure a backend that positively supports each named capability and rerun doctor with its scoped observation; keep scheduling disabled until then.",
      ),
      out(""),
      warn("House-scoped repository access: Unobserved."),
      dim(
        "Next: Configure acme access in the external credential provider, then probe acme/app through the house-scoped integration and rerun doctor; never put credential values in .kitchen.json.",
      ),
      out(""),
      ok("No workers, schedules, labels, or external actions were activated."),
    ],
  },
  {
    id: "adopt",
    group: "Setup",
    label: "kitchn adopt",
    summary: "Bring an existing repository in, safely",
    docs: "/docs/guides/templates/",
    lines: [
      cmd(
        "kitchn adopt . --registry ~/.kitchn --house acme --repository acme/app --template example --set project_name=app",
      ),
      out("Template acme/example revision 1, guidance cccccccccccccccccccccccccccccccccccccccc"),
      out("Target . (existing directory)"),
      warn("  conflict   README.md: existing file differs; left untouched"),
      ok("  add        AGENTS.md"),
      ok("  add        CLAUDE.md"),
      ok("  add        .gitignore"),
      ok("  add        .kitchen.json"),
      out(
        "4 to add, 0 unchanged, 1 conflicts. Nothing is written until the plan is applied; existing files are never overwritten or deleted.",
      ),
      dim("Preview only; no files changed."),
    ],
  },
];
