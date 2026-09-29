# kitchn

**Close the drive-thru. Open a kitchen.**

Right now you're the whole kitchen. Six agents, one of you: you take every
order, read every diff, decide what's good enough, merge at midnight and clean
up worktrees in the morning.

kitchn turns your coding agents into a brigade. Every issue goes to a station
with one owner. An independent expediter checks the exact commit before
anything leaves the pass. Stations earn autonomy from their recorded results,
never because you installed something. It runs on the orchestrator you already
use; [Orca](https://github.com/stablyai/orca) is the first.

[getkitchn.com](https://getkitchn.com) · [Two-minute setup](https://getkitchn.com/docs/start/quickstart/) · [Docs](https://getkitchn.com/docs/start/introduction/)

## Why I built this

I co-founded Popcorn Time, ran VPN.ht for 50,000 users on 150+ servers, and
built Tauri's updater. I was a founding member of Tauri's board and now build
with Tauri at CrabNebula.

In the ten months to September 2026, my agents merged 797 pull requests and
3,500+ commits across 28 repositories.

I watched every one of them. The agents could write the code. What broke was
everything around it:

- two agents pushing to the same branch,
- an approval that still counted after the head moved,
- a green pull request nobody independent had actually checked,
- a cleanup script one command away from deleting unpushed work,
- and me, at midnight, being the only process there was.

Then I heard Lauren Tan in [an interview about working with agents](https://x.com/0xShoopy/status/2104537178571153664):
*"I like to call it the Michelin kitchen because I think it's not a factory…
how do we get quality at scale?"* And on trust: watch the agent work, correct
it, turn what you corrected into a skill, and only then let it run on its own.
That was the missing piece. Speed was never the problem. Trust that's earned,
step by step, with evidence, was.

kitchn is that Michelin kitchen, built as a product: one owner per task, an
independent check at the exact commit, cleanup only with proof, and autonomy
earned, never assumed.

— [David Lemarier](https://lemarier.ca)

> [!NOTE]
> kitchn is early. You can register a house, bind repositories and work issues
> and pull requests from your session today. Unattended runs are still being
> validated ([#1](https://github.com/lemarier/kitchen/issues/1)). The binary is
> still named `kitchen` until
> [#18](https://github.com/lemarier/kitchen/issues/18) ships the rename and the
> install script.

## The brigade

| Station | Job |
| --- | --- |
| Owner | You. Sets house policy and makes the calls evidence can't. |
| Sous-chef | Picks eligible issues, respects dependencies, supervises the cooks. |
| Station cook | Implements one owned task, inside its paths and authority. |
| Commis | Bounded prep and research for a station. |
| Expediter | Checks the exact head against checks, reviewers and findings. A green build is not enough. |
| Inspector | Samples delivered work independently of its author. |
| Gardener | Keeps issues specified, deduplicated and ready. |
| Dishwasher | Reclaims worktrees only with positive proof they're finished. Keeps anything uncertain. |

Each [role card](roles/) lists what the station owes as evidence and the line it
can't cross.

## What stays true

- **Nothing happens because you installed it.** Installing kitchn, choosing a
  workflow or opening a role starts no worker and grants no authority.
- **Approvals cover one commit.** When a pull request head moves, earlier
  approvals no longer count.
- **Your repository stays clean.** Houses, pins and bindings live in a private
  registry (`~/.kitchn`), never in your working tree.
- **Houses never mix.** Your employer, a client and your open-source work each
  get their own rules, credentials and history.
- **Missing evidence counts as failure.** An unobserved check or an unsupported
  backend capability blocks the step; it is never counted as a pass.

## Quick start

Build from source. You need rustup (it installs the pinned toolchain) and
[just](https://just.systems).

```sh
git clone https://github.com/lemarier/kitchen
cd kitchen
just install
```

`just install` records the built commit so `house init` can pin kitchn's
default guidance. It does so only from a clean tree whose commit is on a
remote-tracking branch; otherwise it says why, and `house init` then needs
`--bundle <path>`.

Register a house and bind a repository. Run these inside a checkout of that
repository:

```sh
kitchen house init      # asks for the house name, infers the rest, pins guidance
kitchen house setup --registry ~/.kitchn   # binds this repository; choose workflows or `none`
kitchen house doctor --registry ~/.kitchn  # lists what is still missing
```

`house init` prints the configuration before registering it. Every prompt has
a flag, and `--config <file>` registers a policy you wrote and reviewed
instead. The [quickstart](https://getkitchn.com/docs/start/quickstart/) walks
through each step with its output.

## Working with agents

The [`/kitchn` skill](skills/kitchn/SKILL.md) lets an agent session act as you
in a bound repository. Copy or link `skills/kitchn` into your agent's skills
directory (for Claude Code, `~/.claude/skills/kitchn`). Then hand it:

- an issue: `/kitchn work #42`
- a pull request to review, follow up, repair, or gate: `/kitchn pr #57`
- a rough idea or issue: `/kitchn issue new`, `/kitchn issue refine #61`

The skill resolves the house from the repository's remotes, follows that
house's pinned rules, and asks for your approval before each external action.
It drives these CLI entrypoints:

| Command | What it does |
| --- | --- |
| `kitchen work <issue>` | Plan one issue: coordinate sub-issues, propose a split, or implement it. Takes a durable claim. |
| `kitchen pr <number>` | Plan a review, follow-up, repair, or merge-gate pass at one exact head. Follow-up and repair take a durable claim. |
| `kitchen issue new` / `refine` | Preview an issue draft for approval. Posts nothing. |
| `kitchen hand-back <task>` | Release your claim so a scheduled run or another session can adopt it. |

Interactive and scheduled work share the same claims, so they never work the
same item at once. Claims live in the house's state store (`--store`), and no
command creates that store yet. The
[sessions guide](https://getkitchn.com/docs/guides/sessions/) covers each
entrypoint.

## Other commands

Run `kitchen <command> --help` for flags and exit codes.

| Command | What it does |
| --- | --- |
| `house init`, `setup`, `sync`, `update`, `import`, `doctor` | Register houses, bind repositories, install or update guidance pins, and diagnose readiness. |
| `forge bind`, `forge show` | Bind a house to the GitHub account it writes as. The token stays in a file you place; kitchn stores no credential. |
| `init`, `adopt` | Preview a new or existing repository from a house template. Only missing files are installed; conflicts are left alone. See [templates](templates/README.md). |
| `decompose preview` | Preview a project split into dependency-linked issues. Writes nothing. |
| `cleanup preview`, `approve` | Show what the dishwasher would release and why everything else is kept, and record your approval by digest. Releases nothing. |
| `gardener precheck` | Scheduled read-only check for issue hygiene. |
| `budget precheck`, `run`, `install` | Pause schedules that exhausted their usage budget and report them. `install` adds the tick paused. |
| `pickup task-id`, `check-branch` | Offline pickup diagnostics. |

## Repository layout

| Path | Contents |
| --- | --- |
| `crates/kitchen` | Domain contracts, workflow policy, house registry, state store, and the Orca and GitHub adapters. |
| `crates/kitchen-cli` | The `kitchen` executable, a thin layer over the library. |
| `roles/` | The eight role cards, embedded in the binary and pinned by digest. |
| `skills/kitchn/` | The `/kitchn` agent skill. |
| `templates/` | The example house template and the template guide. |
| `apps/website/` | getkitchn.com (Astro Starlight), deployed from CI on merge to `main`. |
| `.origin89/` | Temporary engineering bootstrap and its notices. |

## Development

Read [CONTRIBUTING.md](CONTRIBUTING.md) and [AGENTS.md](AGENTS.md) first. You
need rustup, Python 3.11+, just, and actionlint; the website also needs Node
22.12+ and pnpm.

```sh
just skills-sync       # install current engineering guidance into an ignored local cache
cargo fetch --locked   # checks run offline after this
just check             # fmt, clippy, tests, build, docs, MSRV, workflow lint
just website-check     # website only; kept out of `just check`
```

`just --list` shows every recipe. `skills-sync` needs network access on first
run; `just skills-offline` reuses the verified cache.

## Security

Report vulnerabilities privately as described in [SECURITY.md](SECURITY.md).

`just check` is offline and does not audit dependencies. The security workflow
runs cargo-deny and zizmor on pushes to `main`, on pull requests, and weekly.
To run them locally, install both and run `just security` with a repository-scoped
`GH_TOKEN`.

Never commit credentials or private operational records.

## Acknowledgements

The kitchen is Lauren Tan's ([@poteto](https://x.com/poteto)) idea: a Michelin
kitchen, not a software factory, where quality holds at scale and agents earn
trust gradually. kitchn builds that idea into a tool.

The engineering skills kitchn is developed with adapt
[pstack](https://github.com/cursor/plugins/tree/main/pstack), also by Lauren
Tan: its `interrogate`, `arena` and `swarm` skills and its overnight-run
guidance, under pstack's MIT license. Those notices move with the skills when
they come into this repository.

## License

Kitchen-authored material is copyright lemarier, licensed under
[MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option. The
development bootstrap keeps its [upstream notices](.origin89/NOTICE.md).
Downloaded skills keep their own licenses.
