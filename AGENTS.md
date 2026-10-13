# AGENTS.md — Navette

Instructions for any coding agent working in this repository. `CLAUDE.md` is a symbolic link
to this file: there is one contract, not two that drift apart.

## Project overview

Navette is the "work-from-home mode" for two Omarchy 4 (Hyprland, Wayland) machines on the same
home LAN, written in Rust. From the home desk (several monitors, keyboard, mouse, mic, speakers),
it drives a separate, employer-managed work laptop without unplugging anything, the way the old
Windows `mstsc /multimon` setup did.

- **One program, two roles.** `navette` is the single binary on both machines. On the machine
  with the monitors ("home"), it takes the reserved monitors itself and draws the remote
  machine's output on them. On the remote machine (the managed work laptop), `navette agent`
  runs as a systemd user service, without root.
- **The link.** The remote machine opens the connection to the home machine over the LAN: no
  listening port, no sshd and no firewall change on the managed machine. Home listens on its LAN
  interface only and refuses any unpaired peer. Pairing is done once with a PIN; mutual TLS
  follows. gliff's framing runs over that stream.
- **The split of work.** The remote does only what only it can do: headless outputs, capture,
  encode, input injection, mic playback. Everything else runs at home: modes and monitor mapping,
  decode (on the home GPU; an NVIDIA GPU is the first test machine), audio mixing and echo
  cancellation, the mic toggle and its indicator, rollback.
- **A mode** names the paired remote machine and which home monitors go to it (any subset).
  Home monitors are never moved or reconfigured. Each remote output is a virtual (headless)
  output that copies the monitor it replaces (resolution, orientation), with its own workspaces
  on the remote side, and the physical left-to-right order is kept, so the pointer crosses
  between the two machines naturally.
- **Focus follows the pointer**: over a reserved monitor, the keyboard and every shortcut (Super
  included) go to the remote machine; over any other monitor, to the home PC.
- **Audio.** Remote sound plays on the home speakers, mixed with home sound: there is no output
  toggle. The mic goes to one side at a time: to the remote machine when a mode starts, back home
  when it stops, and switchable both ways while connected, by a hotkey that stays live under
  shortcut inhibit and by a clickable bar indicator.
- **The clipboard works both ways.**

### What comes from gliff and what Navette owns

Navette uses gliff (`omacom/gliff`, MIT) as a code base, not as an application. It depends on
gliff's library crates (`hypr-capture`, `gliff-va`, `gliff-vk`, `gliff-sw`, `hypr-input`,
`gliff-proto`, `gliff-transport`, `hypr-ipc`, `hypr-wl`) through a git dependency on the fork
`KristenJestin/navette-gliff`, branch `navette`: upstream `main` plus Navette's changes, kept in
sync with upstream, pinned by commit in `Cargo.lock`. These give the capture, the encode and
decode, the input injection, the wire protocol and framing, and the Hyprland and Wayland
plumbing.

gliff's two programs (`gliff`, `gliff-server`) are not libraries. The session and rate-control
logic Navette needs from them is adapted inside Navette, keeping gliff's MIT notice.

Navette owns what embodies the work-from-home mode: the pairing and the link, the modes and the
monitor mapping, the mic routing and its indicator, and the rollback.

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
- The compositor (Hyprland IPC), the audio server (PipeWire) and the link between the two machines
  are reached through traits (ports), so the logic around them is tested with fakes.
- A comment says why, not what.

## Hyprland, gliff and the two machines

- Talk to the Hyprland IPC socket directly (`$XDG_RUNTIME_DIR/hypr/.../.socket.sock` for commands,
  `.socket2.sock` for events), or through gliff's `hypr-ipc` crate. Do not pull in a GPL Hyprland
  crate.
- Headless outputs live on the remote machine and are runtime-only: recreate them after a Hyprland
  restart there. Map monitors by output name from the IPC, never by position or order of arrival.
- Prefer runtime `hl.*` calls through `hyprctl eval` over writing the user's Lua config.
- A mode is reversible: it records what it changed and can roll back, and a Hyprland config reload
  must not leave the desk half-switched.
- The mic toggle hotkey uses the per-bind `dont_inhibit` flag so it still fires while shortcuts
  are inhibited over a reserved monitor.
- **gliff comes from the fork, pinned by commit.** The gliff library crates come from
  `KristenJestin/navette-gliff`, branch `navette` (upstream `main` plus Navette's changes, kept in
  sync with upstream), pinned by commit in `Cargo.lock`. A change inside a gliff crate is made
  in the fork: one pull request into `navette`, tracked by an issue in Navette. The rest of gliff
  stays as upstream has it. Keep gliff's MIT notice on everything adapted or copied from it.
- **Upstream proposals are validated by the maintainer first.** The generic gaps (a mic path,
  several outputs per session) are meant to go to gliff (MIT); never open an upstream issue or
  pull request without the maintainer's approval. Navette holds only what embodies the
  work-from-home mode.

## Security and privacy (non-negotiable)

- **The two machines stay apart.** No work account, token or credential is ever stored, logged or
  written on the home PC, and no home credential on the work laptop. Navette moves pixels, input,
  audio and clipboard over the LAN; it never copies one machine's secrets to the other.
- **Corporate security is never bypassed or weakened.** The laptop is employer-managed (endpoint
  agent, corporate VPN). Navette does nothing to its security posture, routing or DNS; it is a
  LAN peer, authenticated, nothing more.
- **LAN only, authenticated.** No connection is opened to anything but the paired machine on the
  local network. No listening socket on a non-loopback interface without authentication.
- **The remote machine never listens.** `navette agent` only dials out to the home machine: no
  listening port, no sshd, no firewall change on the managed laptop. Home listens on its LAN
  address only (never `0.0.0.0`), and refuses any peer that is not paired.
- **Navette's pairing identity is its own.** The certificate and key made at pairing are neither
  a work nor a home credential: they open the link between the two Navette instances and nothing
  else, and are never reused for another purpose.
- Secrets are never printed, logged or committed. Nothing is published outside the pull request:
  no gist, no upload, no paste service.
- **This will be a public repository.** Nothing private goes into it, an issue, a pull request or
  a commit: no host name, user name, IP, MAC, serial, corporate name or private path. Examples use
  invented data.
- **A permission denial is never worked around**: do what remains, say what was refused.

## Testing

- **Tests never touch a real Hyprland, a real second machine, the real PipeWire graph, the real
  clipboard or the real network.** They talk to fakes (a fake Hyprland IPC, a fake audio backend,
  an in-memory or loopback link) and use temporary folders.
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
- Scopes: `mode`, `display`, `link`, `agent`, `input`, `clipboard`, `audio`, `hypr`, `cli`,
  `config`, `repo`, `ci`.
- **Pull requests are merged by squash only.** The title is a plain Angular subject. The
  description is written in the first person as the maintainer and ends with `Closes #<n>`.

## GitHub gotchas

- Prefer REST (`gh api repos/…`) to GraphQL, which gets rate-limited.
- A body holding backticks or apostrophes is written to a file and passed with `--body-file`.

## When done

Run `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` and `cargo test`, plus the
manual checks the issue asks for, and report the real output. If something fails, say so;
don't claim green.
