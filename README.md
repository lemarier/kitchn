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
> kitchn is early. You can register a house, bind repositories, draft issues,
> and claim issues and pull requests from your session today. Unattended runs
> are still being validated ([#1](https://github.com/lemarier/kitchn/issues/1)). The install
> script and the crates.io release arrive with
> [#18](https://github.com/lemarier/kitchn/issues/18).

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

For supervised work, the house writer GitHub App authors worker commits and
opens the pull request. A separate GitHub identity, currently a person,
reviews and attests it. Kitchen sets the writer identity in each launched
worktree and refuses a push with a foreign author or committer.

The dishwasher recognizes a reconciled stopped launch as creation evidence.
After a retry, it checks the worker and worktree from each attempt for backend
ownership, settlement, and preserved work before an approved release.

## What stays true

- **Nothing happens because you installed it.** Installing kitchn, choosing a
  workflow or opening a role starts no worker and grants no authority.
- **Approvals cover one commit.** When a pull request head moves, earlier
  approvals no longer count.
- **Your repository stays clean.** House policies, pins and repository bindings
  live in a private registry (`~/.kitchn`), never in your working tree. Only a
  confirmed `init` or `adopt` adds template files to the target repository.
- **Houses never mix.** Your employer, a client and your open-source work each
  get their own rules, credentials and history.
- **Missing evidence counts as failure.** An unobserved check or an unsupported
  backend capability blocks the step; it is never counted as a pass.

## Quick start

**1. Install.** Build from source for now. You need rustup (it installs the
pinned toolchain) and [just](https://just.systems).

```sh
git clone https://github.com/lemarier/kitchn && cd kitchn
just install
```

**2. Set up your house.** Change into the repository you want to register, then
run this once. It takes the repository from that checkout, asks for a name,
fills the rest from your machine and defaults, and shows you everything before
saving it.

```sh
cd /path/to/your/repository
kitchn house init
```

**3. Give your agent the skill.** Copy or link `skills/kitchn` into your
agent's skills directory, for example `~/.claude/skills/kitchn` for Claude Code.

**4. Talk to your agent.** That's it. From here on, you work in your session.

The [two-minute setup](https://getkitchn.com/docs/start/quickstart/) walks
through each step with its output.

## Working with kitchn

You talk to kitchn through your agent. Open a session in any repository your
house covers and hand it work:

| You say | kitchn does |
| --- | --- |
| `/kitchn work #42` | Reads the issue and its sub-issues, shows what's ready and what's blocked, and proposes a plan. With Orca, it offers to start a cook per ready sub-issue and asks before each one. |
| `/kitchn pr #57` | Reviews, follows up, repairs or gates the pull request at its exact head. If the head moves, it starts over. |
| `/kitchn issue new` | Drafts an issue with you (outcome, acceptance criteria, dependencies) and shows the full preview before anything is posted. |
| `/kitchn issue refine #61` | Does the same for an existing issue. |

The first time you use it in a repository, it asks which house the repository
belongs to and which workflows to turn on, then shows you what's still missing.
It never guesses a house.

Every comment, label, push or worker launch needs your yes, for that exact
action, in that session. A yes never carries over to the next action.

## The CLI

`kitchn` is the engine behind the skill. Your agent runs it and reads its
JSON; you rarely type anything beyond `house init`. The commands are precise
for external effects. In a clean checkout at a pull request's live head, an
expediter can run `kitchn gate attest --review-id <id>`; Kitchen infers the
sole open pull request for that branch, or at that commit when no branch
matches. `kitchn gate review` also infers
`--head` from the clean checkout's `HEAD`. Explicit flags override these
defaults.

If you want to look under the hood, `kitchn --help` lists every command and
the [CLI reference](https://getkitchn.com/docs/reference/cli/) documents
their flags and exit codes. `house init` also creates the house state store
where claims live, and commands find it through the registry.

## Unattended merges

The scheduled gate merges a pull request without a person only when the house
grants `merge` for the repository and an independent reviewer has attested the
exact head and base. Checks must pass, threads must be resolved, and the
attestation must say the review was clean and read-only, acceptance and
hardware are complete, and no risk class applies. Any risk class needs a
separate human approval, so the gate reports the pull request instead of
merging it. The gate also never merges a branch a person wrote.

The reviewer who attests must not be the pull request's author, any author or
committer of a commit on the branch, or a worker that a launch on the branch
created. Kitchen reads these from the forge and the house records when the
attestation is recorded, and the gate checks them again before merging.

To attest, the reviewer posts an approved review at the pull request's head
whose body contains exactly one `kitchen-attestation` block:

```kitchen-attestation
head=4f2c9a1e7b3d5f608192a3b4c5d6e7f809a1b2c3
base=9e8d7c6b5a4f3e2d1c0b9a8f7e6d5c4b3a291807
semantic=clean
read_only=true
acceptance=complete
hardware=complete
risk=none
```

| Field | Meaning |
| --- | --- |
| `head` | The full SHA of the pull request head the review read. |
| `base` | The full SHA of the base branch's live tip. |
| `semantic` | `clean`, `findings`, `partial`, or `unavailable`. |
| `read_only` | `true` when the review read committed content without running the pull request's code with credentials. |
| `acceptance` | `complete` or `incomplete`: the linked issue's acceptance evidence. |
| `hardware` | `complete` or `incomplete`; `complete` when no hardware work is required. |
| `risk` | `none`, or distinct classes separated by commas, such as `workflow-rules,dependencies`. |

Each field appears once, with no others. Then, in a clean checkout at that
head, the reviewer records the attestation:

```sh
kitchn gate attest --review-id <forge-review-id>
```

A new head or base needs a new review and attestation. The
[CLI reference](https://getkitchn.com/docs/reference/cli/) lists every risk
class.

## Repository layout

| Path | Contents |
| --- | --- |
| `crates/kitchen` | Domain contracts, workflow policy, house registry, state store, and the Orca and GitHub adapters. |
| `crates/kitchen-cli` | The `kitchn` package and executable, a thin layer over the library. |
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
