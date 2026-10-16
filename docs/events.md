# Events: where Navette stands, for a bar or a shell

Navette runs on two machines: the **desk** (the machine with the monitors, running `navette
serve`) and the **remote machine** (the work laptop, running `navette agent`). Each one says
where it stands on a local socket, one line of JSON per change, so that a bar, a launcher or a
shell can show it and act on it without polling. The same socket path, the same line shape and
the same command on both machines: one bar module reads either side.

This page is the contract. A program written against it keeps working across Navette versions
as long as it follows [the reader's rule](#the-readers-rule).

## The socket

`$XDG_RUNTIME_DIR/navette/events.sock`, on both machines.

- `navette serve` opens it on the desk and `navette agent` on the remote machine, as they start,
  whoever starts them (a systemd user unit, a terminal, a script). When they stop, the socket
  file may stay behind, but nothing answers on it: connecting fails, which is how a reader
  knows neither runs, and the next start replaces it.
- It is a Unix socket, readable and writable by the user only (`0600`, in a `0700` directory).
  It is local: the remote machine still listens on no network interface.
- A socket left behind by a crash is replaced at the next start. A second `navette serve` on the
  same machine refuses to start; on the remote machine a second agent is refused before it gets
  there. An agent that cannot open the socket (no `XDG_RUNTIME_DIR`) says why and runs without
  it: the link matters more than a bar.

Any number of programs may read it at once, and each gets the same lines.

## The stream

On connecting, a reader gets **one line with the current state**. Then **one line each time the
state changes**, and nothing in between: no statistics, no per-frame news, never the same line
twice in a row. Each line is a whole state, not a difference from the one before: a reader that
keeps only the last line knows everything.

A line is one JSON object on one line, UTF-8, ended by `\n`. For instance, on the desk while a
mode runs (all names invented):

```json
{"v":1,"role":"desk","state":"mode","peer":"FAR","peer_version":"0.2.3","mode":"office","monitors":["DP-2"],"modes":[{"name":"office","monitors":["DP-2"]}],"mic":"remote"}
```

and on the remote machine meanwhile:

```json
{"v":1,"role":"remote","state":"mode","peer":null,"peer_version":"0.2.0","mode":null,"monitors":["NAVETTE-1"],"modes":[],"mic":null}
```

The stream ends (end of file) when `serve` or `agent` stops. It also ends for one reader:

- when the reader closes the socket, or only its writing side: keep it open for as long as you
  read;
- when the reader falls behind: more than 32 lines waiting for it, or a line it does not take
  within 5 seconds. Navette never waits for a reader, so a stalled bar holds up neither the
  link nor the other readers. The lines already queued are sent, then the socket closes;
  connect again, and the first line is the state.

`navette watch` (below) reconnects by itself.

## The fields

Every line has every field below, on both machines; a field that does not apply is `null` or
`[]`, never missing.

| Field | Desk | Remote machine |
|---|---|---|
| `v` | `1`: the contract's version ([what changes it](#what-changes-v)) | the same |
| `role` | `"desk"` | `"remote"` |
| `state` | [where it stands](#state) | the same |
| `peer` | the remote machine's label: `remote_label` of `mode.conf` (the key before any section), `"REMOTE"` without one | `null`: the desk does not say its label over the link yet; a later version will |
| `peer_version` | the remote machine's Navette version, as its hello said it: while linked (the same minor as this machine's, perhaps another patch: a shell may say "one version behind"), and while `incompatible`; `"0.1"` for any 0.1.x, which does not say its patch; `null` otherwise | the desk's, the same way |
| `mode` | the name of the mode running, starting or stopping; `null` for the unnamed mode (the one a `monitor =` before any `[section]` defines, alone or beside named modes), and with no mode | `null`: the desk does not say the mode's name over the link |
| `monitors` | the desk's monitors the mode gives, left to right; `[]` with no mode | the outputs the remote machine makes for the desk (`NAVETTE-1`, ...), left to right; `[]` with no mode |
| `modes` | every mode `mode.conf` defines, as `[{"name": ..., "monitors": [...]}]`, the unnamed one with `"name": null`; a mode the file gets wrong is left out; without the file, or with a file that cannot be read at all (a broken `[section]` line, say), `[]` | `[]` |
| `mic` | [where the desk's mic goes](#mic) | `null`: the remote machine does not know where the desk's mic goes |

`monitors` in `modes` are written as `mode.conf` writes them: an output name (`DP-2`) or a
monitor's description (`desc:...`). `modes` and `peer` are read from `mode.conf` when the desk
starts and again at each change of the link or the mode: an edit of the file shows at the next
such change.

### `state`

On the desk:

| Value | Meaning |
|---|---|
| `waiting` | no remote machine is linked |
| `linked` | the remote machine is linked, no mode runs |
| `starting` | a mode was asked for and passed its checks; the desk waits for the remote machine to make its outputs |
| `mode` | a mode runs: `mode` and `monitors` say which |
| `stopping` | the desk is giving the mode's monitors back, however the mode ends: `mode stop`, the link dropping, a failure, or a switch to another mode |
| `incompatible` | the remote machine dialed, but runs another minor version: `peer_version` says which ([what a version promises](versions.md)). It stays until a link comes up |

On the remote machine:

| Value | Meaning |
|---|---|
| `away` | not on the desk's network: the agent does not dial ([the link](link.md)) |
| `dialing` | on the desk's network, the desk not answering yet; also the agent's first state, for the moment before it has looked at the network |
| `linked` | linked to the desk, no mode runs |
| `starting` | the desk asked for a mode; the agent is making its outputs |
| `mode` | the desk drives the outputs in `monitors` |
| `incompatible` | the desk runs another minor version: `peer_version` says which. The agent dials again once a minute, on the desk's network only; off it, the state is `away` |

Some sequences, as a reader sees them:

- desk: `waiting` → `linked` → `starting` → `mode` → `stopping` → `linked`;
- a link dropping during a mode, on the desk: `mode` → `stopping` → `waiting`;
- a switch from one mode to another, on the desk: `mode` → `stopping` → `starting` → `mode`;
- a start the remote machine cannot make: `starting` → `stopping` → `linked` on the desk,
  `starting` → `linked` on the remote machine;
- remote machine: `away` → `dialing` → `linked` → `starting` → `mode` → `linked`;
- one machine updated to a new minor, the other not: `waiting` → `incompatible` on the desk,
  `dialing` → `incompatible` on the remote machine; once the other is updated too, `linked` on
  both.

A start refused by its checks (no link, an unknown monitor, a broken `mode.conf`) changes
nothing, and says nothing.

### `mic`

The desk's mic goes to one machine at a time ([the mic](mic.md)):

| Value | Meaning |
|---|---|
| `off` | no mode runs, or the desk plays no sound (`navette serve` without `--audio`): the mic stays on the desk |
| `home` | a mode runs, and the mic stays on the desk. A later version calls the desk "desk" everywhere and says `desk` here: read both as the same |
| `remote` | the mic goes to the remote machine |
| `failed` | a mode runs, but the mic cannot go where it should; `navette mic toggle` says why |

## The reader's rule

- **Ignore the fields you do not know.** A later version may add some.
- **Ignore, or show as they are, the values you do not know**, in `state`, `mic`, `role` or any
  other field whose values are listed here. A new state must not break a bar: show it as text,
  and keep the last known style if you like.
- Do not depend on the order of the fields or on spacing: parse the JSON.
- A line that does not start with `{` is an answer to a command (below), never a state.

## Commands

A reader may also write commands on the same socket, one line each. Each command gets one
answer line, `ok <text>` or `error <why>`, and the stream goes on: the connection stays open.

| Command | Does what | Answer, as the command line says it |
|---|---|---|
| `start` | `navette mode start`: the mode `mode.conf` has (the unnamed one, or the only named one) | `ok DP-2 shows the remote machine's NAVETTE-1`, or `error <why>` |
| `start <name>` | `navette mode start <name>` | the same |
| `stop` | `navette mode stop` | `ok DP-2 is back home`, `ok no mode was running` |
| `mic toggle` | `navette mic toggle` | `ok remote`, `ok home`, or `error <why>` |
| `mic remote` | `navette mic remote` | the same |
| `mic desk` | `navette mic home` (`mic home` is accepted too) | the same |

- They are carried out by the very code that serves `navette mode` and `navette mic`, and say
  the same: the texts after `ok` and `error` are for a human, not to be parsed.
- One reader's commands are carried out one after the other, and answered in that order. State
  lines keep coming meanwhile, so a state line may come between a command and its answer: a
  `start` is answered only once the remote machine has made its outputs, after the `starting`
  line.
- Anything else is answered `error unknown command: <line>`. `status` is not a command here: the
  first line is the status.
- A command line is 64 bytes at most; a longer one ends the connection.
- **On the remote machine every command is answered `error the remote does not drive the
  mode`**: the desk drives the modes and the mic.

## On the command line

### `navette watch`

```
navette watch                  # the state, a word per line: linked, mode, ...
navette watch --json           # the lines as they come
navette watch --waybar         # a Waybar custom module's object per line
navette watch --json --once    # the current line, then exit
```

It reads the socket of whichever runs on this machine, `navette serve` or `navette agent`, and
prints a line at each change, until stopped. While neither runs it prints the `off` line, once,
and looks again every 2 seconds, so a bar started before Navette picks it up on its own:

```json
{"v":1,"state":"off"}
```

`off` is only ever said by `navette watch`, never by the socket; it has no other field.

With `--once` it prints the current line, or the `off` line, and exits with 0: a one-shot check,
such as "a mode runs, do not restart now". (`navette watch | head -1` does not do it: `head`
exits after the first line, but `watch` would notice only at its next write.)

`--waybar` prints, for each line, the four fields of a [Waybar custom
module](https://github.com/Alexays/Waybar/wiki/Module:-Custom) (`text`, `alt`, `class`,
`tooltip`); `alt` and `class` are the state as it is, so a bar styles each one and an unknown
one passes through:

| `state` | `text` | `tooltip` |
|---|---|---|
| `off` | `off` | Navette is not running here |
| `waiting` | `waiting` | Waiting for FAR |
| `away` | `away` | Not on the desk's network: not dialing it |
| `dialing` | `dialing` | Dialing the desk, which does not answer yet |
| `linked` | `linked` | FAR is linked, no mode runs (on the remote machine: The desk is linked, no mode runs) |
| `starting` | `starting` | Starting mode office on DP-2 (on the remote machine: Making NAVETTE-1 for the desk) |
| `mode` | the mode's name, or `mode` | Mode office runs on DP-2; mic: remote (on the remote machine: The desk drives NAVETTE-1) |
| `stopping` | `stopping` | Giving DP-2 back |
| `incompatible` | `incompatible` | FAR runs Navette 0.3.0, another minor version: update the older machine (on the remote machine: The desk runs ...) |
| another | the state | the state |

FAR, 0.3.0, office and DP-2 stand for the line's `peer`, `peer_version`, `mode` and `monitors`. A Waybar module that
shows it on either machine, and toggles the mic on a click (refused on the remote machine):

```jsonc
"custom/navette": {
  "exec": "navette watch --waybar",
  "return-type": "json",
  "format": "{}",
  "on-click": "navette mic toggle"
}
```

A Quickshell bar does the same with a `Process` running `navette watch --json`, a `SplitParser`
that reads each line with `JSON.parse`, and the socket itself or `navette` to send commands.

### `navette mode list --json`

The desk's modes as the `modes` field says them, one JSON array, read from `mode.conf` without
`navette serve`: a launcher can read it at session start. Without `mode.conf` it prints `[]`.
A mode the file gets wrong is left out, as in the `modes` field. A file that cannot be read at
all is an error here, as with `navette mode list`: the command says why on stderr and exits with
1, while the `modes` field of the events says `[]`.

```json
[{"name":"office","monitors":["DP-2"]},{"name":"all","monitors":["DP-1","DP-2"]}]
```

### What stays as it was

`navette status` (which now also says, on the desk, that the remote machine is refused for its
version and which machine to update), `navette mic status|watch [--json]` (the mic alone, in its own Waybar shape),
and the sockets behind `navette mode` and `navette mic` (`control.sock`, `mic.sock`) work as
before: they read the same state, and a program written for them needs no change.

## What changes `v`

`v` is `1`. It changes only for what would mislead a reader that follows the rule above:

- a field removed or renamed, or its type changed;
- a field or a value that changes meaning;
- a value removed, or the line's framing changed (one JSON object per line);
- a command removed, or its answer no longer `ok ...` or `error ...` on one line.

It does not change for a new field, a new value of `state`, `mic` or any listed field, or a new
command: [the reader's rule](#the-readers-rule) covers them. Within a Navette minor version the
contract is one of [its promises](versions.md); a reader that sees a `v` it does not know should
say so rather than guess.
