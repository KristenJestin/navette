# The link between the two machines

The remote machine (the managed laptop) dials the home machine over the LAN; home listens on
its LAN address only. They trust each other once paired with a PIN, then talk over mutual
TLS 1.3, each side pinning the other's certificate.

## Commands

At home, on the LAN address and a free port of your choice (the examples use invented
addresses: home `192.0.2.10`, the laptop `192.0.2.20`):

```
navette pair --listen 192.0.2.10:7420     # shows a six-digit PIN, waits 120 s
```

On the remote machine:

```
navette pair 192.0.2.10:7420              # asks for the PIN
```

Then `navette serve` at home (it listens on the address used to pair; `--listen` changes it)
and `navette agent` on the remote machine, as a systemd user service
(`dist/navette-agent.service`, how to install it is written at its top).

Both commands keep their files in `$XDG_CONFIG_HOME/navette` (`--config-dir` or
`NAVETTE_CONFIG_DIR` to move it): `identity.key` and `identity.crt` (this machine's Navette
identity) and `paired.conf` (the peer's certificate fingerprint, its role and the address),
all 0600 in a 0700 directory. Pairing again replaces `paired.conf`; to change a machine's
identity, remove its `identity.*` files and pair both machines again.

## The clipboard

While a mode runs, `navette serve` and `navette agent` each run one clipboard bridge on the
link, whatever the monitors given: copy on one machine, paste on the other, text and files,
both ways. It rides the link after the hello, in gliff's framing; the transfer is gliff's
engine, which is lazy (nothing moves until an application pastes), typed, chunked, and
refuses a request against an offer that a newer copy has replaced.

- **Nothing crosses outside a mode.** The link stays up between two modes, so that a start is
  quick, but each machine's display says on it whether a mode runs: home once the remote
  answered its start, the remote while it runs home's mode. Until then and after the stop,
  each machine keeps its copies to itself, does not put the other's offer on its clipboard,
  and refuses its requests, whatever the other machine asks. A copy made before `mode start`
  is not offered when the mode starts: the next one is. At the stop, the other machine's copy
  leaves the clipboard (a copy made here since stays), and the received files go. The
  remote's sound and home's mic are not sent either, and the input goes only to a mode's
  outputs.

- **One bridge at a time on a link, never per monitor.** Two bridges on one machine's clipboard replace
  each other's offers, and every paste then fails with "the peer aborted the transfer"; the
  link has exactly one. When a new connection from the remote has said hello, home ends the
  older one, and its bridge, before it starts the new one's; a connection whose hello fails
  leaves the link that is up alone.
- **Both machines use `ext-data-control`** (Hyprland has it), through gliff's `hypr-input`.
  The display is `$WAYLAND_DISPLAY`, else the running Hyprland session. If it is not there
  yet (the agent starting before the session) the link stays up, and the bridge looks again
  every 5 s; meanwhile a paste from the other machine is refused at once. The calls to the
  compositor that can block run beside the link, never in its way: a slow compositor does not
  stop the heartbeats.
- **Limits.** 32 MiB for a pasted item that is not a file (gliff's), 10,000 entries for a set
  of files, and no limit on their size: the free space is checked before a byte is written. An
  offer of the other machine beyond them, with a path that is absolute or holds `..`, a
  top-level name starting with `.navette-incoming-`, sizes whose sum does not fit in 64 bits,
  or more than 64 mime types (or one longer than 255 bytes) is refused: it withdraws that
  machine's previous copy, and nothing is offered.
- **What a copy offers**: only the files and folders copied, folders walked whole. Links are
  never followed and, with sockets, pipes and devices, are left out; the machine that copied
  names them in a notification when the other one pastes. A copied file is read only when the
  other machine pastes it, and only if it is still a regular file (not a link put in its
  place). Of the types the copy is on the clipboard as, only the standard ones cross: text
  (`text/plain` and its charsets, `UTF8_STRING`, `STRING`, `TEXT`), `text/html`, `image/*`
  and the file lists. The types an application keeps for its own instances (a token, a
  process id) stay on their machine, where they mean something.
- **Pasted files** go in a private temporary folder, not in the Downloads: `navette/clipboard`
  in `$XDG_RUNTIME_DIR` (per user, mode 0700, cleared at logout), else `navette-clipboard-<uid>`
  in the temporary directory, mode 0700 as well, and refused if it is a link or belongs to
  another user. A file manager reads every new copy at once, so files come in with no paste;
  here they pile up nowhere the user keeps things, and a paste copies them from there to where
  the user pastes. Only the files of the copy being offered are kept: a newer copy removes the
  older one's, and the folder is emptied when the mode stops, when the link ends (the lid
  closes) or when `navette` starts. That folder may live in memory (`$XDG_RUNTIME_DIR` is a tmpfs): a very large
  copy takes that much memory until it is replaced or the link ends. A paste first fills a
  hidden 0700 folder in it (`.navette-incoming-<pid>-<n>`), then moves each pasted file or
  folder in under its own name, or `name (1).ext`, `name (2).ext`… when one is already there:
  nothing is ever overwritten, and a name from the other machine never reaches out of the
  folder. They move in all together or not at all. On a filesystem that cannot rename without
  replacing, a file is linked in then unlinked, a folder replaces an empty one made for it. The
  application that pasted gets their paths once they are all complete (the text types of the
  copy name the same paths). A second paste of the same copy points at the same files.
- **The same copy offered again** points at the same files too. The machine that copied offers
  files unchanged since its last copy of files (the same files, sizes and modification times)
  under the same offer number; the machine that fetched that copy answers with the files it
  received, while they are all still in the folder, and keeps them. A file changed since is
  fetched again. A new link starts with an empty folder, so a copy offered again on it is
  fetched again.
- **A file manager that stops waiting.** A file manager that reads every new selection at
  once gives up when the answer is slow: one waits two seconds for each of the two file-list
  types, so about four seconds in all, which a large paste over the LAN outlasts (with a mode
  running, the picture leaves the clipboard a few megabytes a second). It then holds a copy
  with no file in it, and its paste does nothing. When the files are in and the application
  that asked for them had stopped waiting, the copy is put on the clipboard again, once: the
  file manager reads it again and gets them at once. Never over a copy made on this machine
  since.
- **On the link**, a file's chunks go after the picture, the sound and the input, and gliff's
  window (four chunks of 256 KiB in flight per transfer) keeps a transfer from piling up more:
  a large file never holds a frame back for longer than the chunk already on the line. When
  the picture keeps the link busy, the clipboard still gets one message in every eight frames,
  so a paste never stalls into gliff's 30 s idle timeout.
- **Progress and cancel.** While files cross, either way, home's corners show a bar per
  transfer with what is left ([the corner](mode.md#the-corner)); a click on one stops that
  transfer, the others going on: the half
  of a paste already written is removed, nothing is left behind, and the copy whose paste was
  stopped serves no more files (copy again to paste it). The bar loses a paste served from
  here when its last byte goes, when a file is not the size the copy said, or when the other
  machine asks for no next file within 30 s. The machine that pasted says in a desktop
  notification when a paste did not end well: `Paste failed` with why (no room, the other
  machine stopped it…), or `Paste cancelled`. Files that arrive say nothing: a file manager
  reads the files of a copy as soon as it is offered, before any paste, and a read of the
  clipboard does not say why it is made (the same types, at any time, the first read being
  that fetch and not the paste), so a notification could not tell a paste from it. The bar
  shows them crossing.
- **When the link drops**, every paste and transfer in flight ends at once, and what it had
  written is removed; the files already pasted stay.
- **When a newer copy replaces the one being pasted**, the paste of the older one stops, and
  what it had written is removed: a large file of a copy nobody holds any more never competes
  with the new one for the link. The application that pasted it gets nothing.
- **The journal** follows each copy by its offer number, one line per step and on both
  machines: the offer sent and received with its file count, each paste asked and as what
  type, the files fetched, pasted (with how long it took) or failed with why, the files
  served, and each request refused with why ("this machine copied again"). Each copy made on
  a machine gets a line there: its offer sent, or why it is not (a newer copy replaced it
  first), and a file list that names no local file says so. A copy that leaves no line never
  reached the clipboard Navette watches. It never holds
  what was copied: a paste of text or a picture that fails says only "a paste failed: ...",
  a paste of files names the file it failed on.

## The home firewall

Pairing and the link use the same port, so home needs one rule. With Omarchy's `ufw`:

```
sudo ufw allow in proto tcp from 192.0.2.20 to 192.0.2.10 port 7420 comment navette
```

Nothing changes on the remote machine: it only dials out, and with its corporate VPN up its
LAN traffic to home still goes directly over the local network.

## Pairing protocol

Written at the top of [`src/pair.rs`](../src/pair.rs): SPAKE2 on the PIN, then each side sends
its certificate fingerprint under an HMAC of the shared key and the transcript, and records its
peer only after checking the other's.

## Threat model

- **A machine on the LAN without the PIN** cannot pair: home accepts one connection per
  pairing session and SPAKE2 gives it one guess out of a million, with nothing to test offline.
  It cannot open the link either: home ends the TLS handshake for any certificate but the
  paired one, before any Navette message is read, and the remote refuses any home but the
  paired one.
- **Someone listening on the LAN** sees TLS 1.3 traffic, and during pairing only SPAKE2
  messages and certificate fingerprints, none of which is secret.
- **Denial of service** stays possible from the LAN: a host can take the one pairing attempt
  (pair again). Home shakes hands with up to 8 connections at once and drops each one that
  stays silent after 10 s, so a few silent connections do not keep the paired remote out; a
  host that keeps all 8 slots busy permanently still can, and only a firewall rule limiting
  the LAN sources to the remote's address stops that.
- **What is not covered**: a compromised machine. Whoever can read a machine's
  `identity.key` can pose as it; the answer is a new identity and a new pairing. The address
  home listens on is the one it is given: Navette refuses a wildcard (`0.0.0.0`, `::`) but does
  not check that the address is on the LAN.
- **What never crosses**: no work credential goes to the home machine and no home credential
  to the laptop. The Navette identity is neither, and is used for nothing but this link.
