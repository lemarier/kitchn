// Entries without `planned` are output captured from the kitchn CLI against a
// scratch registry and the repository's example template. Paths are shortened
// to ~/.kitchn and long reports are trimmed or condensed onto fewer lines; the
// wording is otherwise the CLI's own.
//
// Entries marked `planned: true` show the intended behavior of features that
// are still being built (see issue #1). Replace each with captured output when
// its feature lands; `just website-deploy` refuses to publish while any remain.

export type Tone = "cmd" | "out" | "dim" | "ok" | "warn";
export interface TermLine {
  tone: Tone;
  text: string;
}
export type Group = "In your session" | "Set up";
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
    label: "/kitchn work #14",
    planned: true,
    summary: "Run a whole issue tree from one Orca workspace",
    docs: "/docs/start/introduction/",
    lines: [
      cmd("/kitchn let's work on #14"),
      out("House acme, guidance pinned at 4f2a9c1. #14 has 4 sub-issues:"),
      out("  #15 Task contracts      ready"),
      out("  #16 Pickup workflow     blocked by #15"),
      out("  #17 Merge gate          blocked by #15"),
      out("  #19 Docs                ready"),
      warn("Start cooks on #15 and #19 now, then #16 and #17 when #15 merges? [y/N] y"),
      ok("Orca: 2 worktrees, Codex cooking, Claude Code at the pass."),
      out("11:40 #15 merged. Started #16 and #17."),
      warn("11:52 Cook on #17 asks: keep the legacy flag? Waiting for your answer."),
      dim("You stay the chef-owner. Merges wait for your sign-off."),
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
      ok("Checks passed, reviewer approved, no open findings at 3b04d17."),
      out("Verdict: ready. Merge needs the chef-owner's sign-off."),
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
      out("Sharpened acceptance criteria and found a dependency: blocked by #12."),
      warn("One decision needs you: derive work type from labels or changed paths?"),
      dim("Nothing is posted until you approve the final text."),
    ],
  },
  {
    id: "setup",
    group: "Set up",
    label: "kitchn house setup",
    summary: "Adopt a repository",
    docs: "/docs/start/quickstart/",
    lines: [
      cmd("kitchn house setup --registry ~/.kitchn --repository acme/app"),
      ok("Adopted .kitchen.json for acme/app."),
      warn("Doctor: setup incomplete"),
      ok("No workers, schedules, labels, or external actions were activated."),
    ],
  },
  {
    id: "doctor",
    group: "Set up",
    label: "kitchn house doctor",
    summary: "See exactly what's missing",
    docs: "/docs/reference/cli/",
    lines: [
      cmd("kitchn house doctor --registry ~/.kitchn"),
      warn("House-scoped repository access: Unobserved."),
      warn(
        "Scheduled gate requires: schedule.precheck, worker.launch_readiness, house.credentials, … (trimmed)",
      ),
      dim("Next: configure acme access, then rerun doctor with that observation."),
      ok("No workers, schedules, labels, or external actions were activated."),
    ],
  },
  {
    id: "adopt",
    group: "Set up",
    label: "kitchn adopt",
    summary: "Bring an existing repository in, safely",
    docs: "/docs/guides/templates/",
    lines: [
      cmd("kitchn adopt . --registry ~/.kitchn --template example --set project_name=app"),
      warn("  conflict   README.md: existing file differs; left untouched"),
      ok("  add        AGENTS.md, CLAUDE.md, .gitignore, .kitchen.json"),
      out("4 to add, 0 unchanged, 1 conflicts."),
      dim("Preview only; no files changed."),
    ],
  },
];
