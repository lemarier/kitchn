// Intended behavior of unattended runs (issues #8, #9, #11, #12, #40, #44,
// #49). Replace with a real night's log once scheduled runs ship;
// deploys list it while it is planned.

export interface ShiftEntry {
  time: string;
  who: string;
  text: string;
  tone?: "good" | "flag";
}

export interface ReportLine {
  label: string;
  value: string;
  tone?: "good" | "flag";
}

export const nightShift = {
  planned: true,
  house: "acme",
  log: [
    {
      time: "22:04",
      who: "Sous-chef",
      text: "Picked up 3 ready issues. #81 waits: blocked by #80.",
    },
    { time: "22:06", who: "Cooks", text: "Two worktrees on Orca with Codex, one writer each." },
    {
      time: "23:10",
      who: "Expediter",
      text: "#84 sent back: no test for empty input. Round 1 of 3.",
      tone: "flag",
    },
    { time: "23:52", who: "Expediter", text: "#84 passes at exact head e41b2c0." },
    {
      time: "23:53",
      who: "Order up",
      text: "Merged #84 under the bugfix grant this station earned.",
      tone: "good",
    },
    {
      time: "00:40",
      who: "Cook",
      text: "Question on #86: keep the old flag or remove it? Parked for you.",
      tone: "flag",
    },
    { time: "02:15", who: "Inspector", text: "Sampled merged #79. No findings." },
    {
      time: "03:00",
      who: "Dishwasher",
      text: "Cleared 3 finished worktrees. Kept 1 with untracked files.",
    },
  ] satisfies ShiftEntry[],
  report: [
    { label: "Merged", value: "#84, #85", tone: "good" },
    {
      label: "Needs your sign-off",
      value: "#87 (feature work isn't trusted to merge alone yet)",
      tone: "flag",
    },
    { label: "Question for you", value: "#86", tone: "flag" },
    { label: "Sent back and fixed", value: "1" },
    { label: "Cleaned up", value: "3 worktrees" },
    { label: "Budget", value: "11 of 40 agent-hours this week" },
  ] satisfies ReportLine[],
};
