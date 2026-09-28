// Shared by the landing page and the docs glossary.
export interface Term {
  term: string;
  kitchen?: string;
  meaning: string;
  landing?: true;
}

export const glossary: Term[] = [
  {
    term: "House",
    meaning:
      "The organization kitchn works for: a company, a client or an open-source project. It owns the rules, allowed repositories, reviewers and credentials. Houses never share keys, context or history.",
    landing: true,
  },
  {
    term: "Station",
    kitchen: "role",
    meaning:
      "One job with one owner, such as the cook who implements or the expediter who checks. Each station owes evidence and has a line it can't cross.",
    landing: true,
  },
  { term: "Brigade", meaning: "All eight stations together." },
  {
    term: "Order",
    kitchen: "ticket",
    meaning: "A task, usually a GitHub issue, moving from station to station.",
    landing: true,
  },
  {
    term: "The pass",
    meaning:
      "The independent check before anything ships: the exact revision, the required checks and reviewers, and no open findings.",
    landing: true,
  },
  {
    term: "Pinned guidance",
    meaning:
      "The house's written rules, imported as a verified snapshot and locked to a revision. A task keeps the rules it started with.",
    landing: true,
  },
  {
    term: "Orchestrator",
    kitchen: "backend",
    meaning:
      "The tool that launches and supervises agents, such as Orca. kitchn sits on top of it.",
    landing: true,
  },
  {
    term: "Agent",
    meaning: "The coding model doing the work at a station, such as Claude Code or Codex.",
  },
  {
    term: "Evidence",
    meaning:
      "What a station hands over: tests run, the exact revision, findings and what's still unknown. Missing evidence is never success.",
    landing: true,
  },
  {
    term: "Grant",
    meaning:
      "Explicit permission for one kind of external action, such as posting or merging, scoped to a repository. Installing kitchn grants nothing.",
    landing: true,
  },
  {
    term: "Registry",
    meaning:
      "The private directory, outside any checkout, where kitchn keeps house policies and pinned snapshots.",
  },
  {
    term: "Binding",
    kitchen: ".kitchen.json",
    meaning:
      "The file that ties a repository to its house and the workflows it opts into. It can add reviewers or checks, never remove them.",
  },
  {
    term: "Workflow",
    meaning:
      "An automated loop a repository can opt into, such as pickup or gate. Selecting one starts nothing.",
  },
  {
    term: "Doctor",
    meaning:
      "The command that reports what's still missing. Anything it can't observe is unknown, never fine.",
  },
];
