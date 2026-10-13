# AGENTS.md — Navette

Instructions for any coding agent working in this repository. `CLAUDE.md` is a symbolic link
to this file: there is one contract, not two that drift apart.

## Project overview

Navette is the "work-from-home mode" for two Omarchy 4 (Hyprland, Wayland) machines on the same
home LAN, written in Rust. From the home desk (several monitors, keyboard, mouse, mic, speakers),
it drives a separate, employer-managed work laptop without unplugging anything, the way the old
Windows `mstsc /multimon` setup did.

- **A preset** says how many of the home monitors (1 to 3) become work monitors. The reserved
  monitors show only the laptop's output fullscreen: no bar, no home workspace, no stray window,
  the layout unchanged, so the pointer crosses between the two machines naturally.
- **Remote outputs behave as real monitors** of the laptop: one virtual (headless) output per
  reserved monitor, at its resolution, with its own workspaces on the laptop side.
- **Focus follows the pointer**: over a work monitor, the keyboard and every shortcut (Super
  included) go to the laptop; over a home monitor, to the home PC.
- **Audio (output and mic) is switched by hand, not by focus**: a simple toggle (a hotkey that
  stays live under shortcut inhibit, plus a bar indicator) says where sound and mic go.
- **The clipboard works both ways.**

Navette does not transport the pixels itself: the video, input injection and the clipboard are
gliff's job (`omacom/gliff`, MIT), one gliff client window per reserved monitor. Navette is the
orchestration on top: it applies the Hyprland preset (headless outputs, workspace and window
rules, rollback), launches and places the gliff windows, and owns the audio toggle and its
indicator. It drives Hyprland over its IPC socket and gliff over its CLI or as a library.

The audio path and multiple outputs per session are generic gaps meant to be contributed
upstream to gliff (MIT); Navette holds only what embodies the work-from-home mode.

## Commands

```
cargo build
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
tools/commit-message.sh --range origin/dev..HEAD
git config core.hooksPath .githooks     # once per clone: the commit-msg and pre-commit hooks
```

## Principles

### 1. Think before coding

**Don't assume. Don't hide confusion. Surface tradeoffs.**

- State your assumptions explicitly. If uncertain, ask.
- If multiple interpretations exist, present them; don't pick silently.
- If a simpler approach exists, say so. Push back when warranted.
- If the code and the issue disagree, the issue wins; if the issue is wrong, say so instead of
  quietly deviating.

### 2. Simplicity first

**Minimum code that solves the problem. Nothing speculative.**

- No features beyond what the task asks. No abstractions for single-use code.
- No configurability that was not requested.
- No error handling for impossible scenarios. If 200 lines could be 50, rewrite.

### 3. Surgical changes

**Touch only what you must. Clean up only your own mess.**

- Don't "improve" adjacent code, comments or formatting. Don't refactor what isn't broken.
- Match existing style. If you notice unrelated dead code, mention it, don't delete it.
- Every changed line must trace directly to the task.

### 4. The failing test first

**Define success criteria. Loop until verified.**

- Write the test that fails first, see it fail, then make it pass.
- "Fix the bug" → a test that reproduces it, then the fix.
- Every scenario of the issue gets a test named after it. A scenario without a test is a
  defect, not a TODO.

## Code style

- Rust 2024, `cargo fmt`, `cargo clippy -- -D warnings` clean.
- Code, identifiers, comments, docs, issues and commits are in English.
- The compositor (Hyprland IPC), the audio server (PipeWire) and gliff are reached through traits
  (ports), so the logic around them is tested with fakes.
- A comment says why, not what.

## Hyprland and the two machines

- Talk to the Hyprland IPC socket directly (`$XDG_RUNTIME_DIR/hypr/.../.socket.sock` for commands,
  `.socket2.sock` for events). Do not pull in a GPL Hyprland crate.
- Headless outputs are runtime-only: recreate them after a Hyprland restart. Never trust a new
  window's position (Hyprland centres it); map by output name from the IPC.
- Prefer runtime `hl.*` calls through `hyprctl eval` over writing the user's Lua config.
- A preset is reversible: it records what it changed and can roll back, and a Hyprland config
  reload must not leave the desk half-switched.
- The audio toggle hotkey uses the per-bind `dont_inhibit` flag so it still fires while shortcuts
  are inhibited over a work monitor.

## Security and privacy (non-negotiable)

- **The two machines stay apart.** No work account, token or credential is ever stored, logged or
  written on the home PC, and no home credential on the work laptop. Navette moves pixels, input,
  audio and clipboard over the LAN; it never copies one machine's secrets to the other.
- **Corporate security is never bypassed or weakened.** The laptop is employer-managed (endpoint
  agent, corporate VPN). Navette does nothing to its security posture, routing or DNS; it is a
  LAN peer, authenticated, nothing more.
- **LAN only, authenticated.** No connection is opened to anything but the paired machine on the
  local network. No listening socket on a non-loopback interface without authentication.
- Secrets are never printed, logged or committed. Nothing is published outside the pull request:
  no gist, no upload, no paste service.
- **This will be a public repository.** Nothing private goes into it, an issue, a pull request or
  a commit: no host name, user name, IP, MAC, serial, corporate name or private path. Examples use
  invented data.
- **A permission denial is never worked around**: do what remains, say what was refused.

## Testing

- **Tests never touch a real Hyprland, a real second machine, the real PipeWire graph, the real
  clipboard or the real network.** They talk to fakes (a fake Hyprland IPC, a fake gliff, a fake
  audio backend) and use temporary folders.
- What only real machines can show (latency, pointer crossing, shortcut inhibit, how a headless
  output feels as a monitor, audio switching mid-call) is checked by hand on the two machines and
  reported in the pull request, with the numbers.
- A GUI, compositor or benchmark check runs in a **headless nested compositor** (a nested
  Hyprland or a headless Wayland session), never on the maintainer's live desk. A loop that opens
  windows on the real screens is never acceptable, even briefly.
- A test that fails only under load is rerun alone before the failure is called real.

## How the work is organised

| Who | What |
|---|---|
| The maintainer | decides what is built, tries each tranche on their two machines, and merges what they have not delegated |
| The lead agent | turns the maintainer's decisions into issues, launches the developing agents, reviews their pull requests, keeps the tracker honest, and never hides a failure |
| Developing agents | implement one issue each: the failing test first, then the code, the verification and a pull request; they never merge |
| CI | `verify`, `commit-messages` and `shape` on every pull request |

### The tracker

- **Issues are the tickets, and a ticket stands alone.** It holds the what, the where, the "how
  to verify" and the base branch, with no local path. If it lacks one of them, ask rather than
  guess.
- The plan is cut into **tranches**, labels `tranche:1`, `tranche:2`…, beside `type:*` and
  `area:*`. A tranche ends on something the maintainer can use, and they accept it on their
  machines.
- **`dev` is the integration branch** and the default branch: a branch starts from
  `origin/dev` and its pull request targets it. `main` holds released versions.

## Git and pull requests (non-negotiable)

- **Never commit, merge, rebase, push or force-push on `main` or `dev` directly.** If you are
  on one of these branches, create a branch first (`feat/<n>-<topic>`, `fix/<n>-<topic>`). The
  `pre-commit` hook refuses it.
- Never rewrite published history. No `--no-verify`.
- One commit = one intent. No `wip` commits. Don't mix formatting and logic in one commit.
- Commits use the Git user configured on the machine.
- The subject is `<type>(<scope>): <subject>`: the **scope is required**, the subject is 72
  characters at most, starts lowercase, has no final period, and the type is one of feat, fix,
  refactor, test, docs, chore, build, ci, perf. `tools/commit-message.sh` is the judge (the
  `commit-msg` hook and CI run it).
- Scopes: `preset`, `monitor`, `hypr`, `gliff`, `audio`, `cli`, `config`, `repo`, `ci`.
- **Pull requests are merged by squash only.** The title is a plain Angular subject. The
  description is written in the first person as the maintainer and ends with `Closes #<n>`.

## GitHub gotchas

- Prefer REST (`gh api repos/…`) to GraphQL, which gets rate-limited.
- A body holding backticks or apostrophes is written to a file and passed with `--body-file`.

## When done

Run `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` and `cargo test`, plus the
manual checks the issue asks for, and report the real output. If something fails, say so;
don't claim green.
