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
and `navette agent` on the remote machine, both through `navette run` in the one systemd user
service `navette.service` (how to install it is written in [the install guide](install.md);
it replaces the former `navette-agent.service`).

Both commands keep their files in `$XDG_CONFIG_HOME/navette` (`--config-dir` or
`NAVETTE_CONFIG_DIR` to move it): `identity.key` and `identity.crt` (this machine's Navette
identity) and `paired.conf` (the peer's certificate fingerprint, its role and the address,
and on the remote machine the hardware address of the desk's router, below), all 0600 in a
0700 directory. Pairing again replaces `paired.conf`; to change a machine's
identity, remove its `identity.*` files and pair both machines again.

## The clipboard

While a mode runs, `navette serve` and `navette agent` each run one clipboard bridge on the
link, whatever the monitors given: copy on one machine, paste on the other, text and files,
both ways. It rides the link after the hello, in gliff's framing; the transfer is gliff's
engine, which is lazy (nothing moves until an application pastes), typed, chunked, and
refuses a request against an offer that a newer copy has replaced. Files go further: they
cross only when an application reads them, through a virtual folder (below).

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
- **The virtual folder.** The machine that receives a copy of files shows them in a folder it
  mounts itself with FUSE, `navette/files` in `$XDG_RUNTIME_DIR`, the way remote desktop
  clients do it. A paste of files (and the text of a copy of files) gets their paths there at
  once, `<offer number>/<name>`, folders as their tree: nothing crosses, so a file manager
  that reads every new copy to set its Paste button moves no byte, and never waits. A file's
  bytes are fetched from the other machine only when an application reads it: the first read
  fetches it whole, in order, into a cache file, and each read waits for its part. Reading the
  same file again fetches nothing. The folder is read-only and the user's alone; a read waits
  on a thread of its own, never on the link's.
  - The bytes read go to disk, not memory: `navette/clipboard` in `$XDG_CACHE_HOME` (else
    `~/.cache`), a folder made private to the user (mode 0700; a link put in its place is
    refused), since a runtime folder is a tmpfs of a few GB that a large file would fill.
    Each cache file is removed from the folder as soon as it is made, so nothing there has a
    name and nothing stays: its bytes go when their copy goes, or when `navette` ends, however
    it ends; the folder is emptied again at each start. A file read takes its size there
    while its copy is shown, and the room for it and for the files being fetched beside it is
    checked before it is fetched.
  - A newer copy does not stop a transfer: an older copy stays while an application holds one
    of its files or folders open, and while its transfer is under way: from the first read of
    one of its files until it is cancelled, or until every file read is whole and nothing of
    it was opened for 30 s (a file manager copying a folder opens its files one after the
    other, with nothing open between two). The machine that copied serves the files of its
    last 8 copies of files, for that. Several transfers can run at once.
  - A file closed by its last reader before all its bytes came stops its fetch, and what came
    of it goes: a file manager that looks at the first bytes of each file of the folder costs
    a few chunks, not the files. A fetch that fails (the other machine stopped it, or sent
    less than it said) keeps nothing either. In both cases a later read fetches the file
    again. A copy whose reads fail says `Paste failed` once, not at each read.
  - It is mounted when a mode starts and unmounted when it stops, when the link ends and when
    `navette` exits; a read in progress then fails. A mount left by a `navette` that died is
    unmounted at the next start.
  - It needs nothing at build time (no libfuse: the `fuser` crate mounts through
    `fusermount3`), and at run time `/dev/fuse` and `fusermount3` (setuid, from `fuse3`), as
    the user, with no root and no change to the system; a setuid helper is what
    `NoNewPrivileges=` forbids, so the agent's unit does not set it. Where it cannot be
    mounted (no FUSE, or no `$XDG_RUNTIME_DIR`), the journal says so once, and a paste of
    files fetches them all first, as below.
- **Pasted files**, without the virtual folder, go in a private temporary folder, not in the
  Downloads: `navette/clipboard` in `$XDG_RUNTIME_DIR` (per user, mode 0700, cleared at
  logout), else `navette-clipboard-<uid>`
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
- **The same copy offered again** points at the same files too (in the virtual folder: the
  same paths, and what was read of them stays). The machine that copied offers
  files unchanged since its last copy of files (the same files, sizes and modification times)
  under the same offer number; the machine that fetched that copy answers with the files it
  received, while they are all still in the folder, and keeps them. A file changed since is
  fetched again. A new link starts with an empty folder, so a copy offered again on it is
  fetched again.
- **A file manager that stops waiting** (without the virtual folder). A file manager that
  reads every new selection at once gives up when the answer is slow: one waits two seconds for each of the two file-list
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
  stopped serves no more files (copy again to paste it). Through the virtual folder, a
  transfer is the reads of one copy's files: its bar counts the bytes read of all of them
  over their total, and a cancel fails the reads of that copy, those waiting and those to
  come. The bar loses a paste served from
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
- **When a newer copy replaces the one being pasted** (without the virtual folder), the paste of the older one stops, and
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

## Only from the desk's network

The agent dials only when the laptop is on the desk's network. Away from it (an office LAN, a
hotel's Wi-Fi), the desk's private address may be anybody's, and a managed laptop has no
business knocking at it every few seconds. A subnet says nothing, since the same private
subnet exists on many networks: the desk's network is known by its router, the laptop's
default gateway there.

At pairing, the remote machine records that router's hardware address in `paired.conf`
(`gateway = 02:00:00:00:00:01`). Before each dial, the agent reads its routing and ARP tables
(`/proc/net/route`, `/proc/net/arp`), which sends nothing, and follows the way to the desk,
not the default route: the interface whose directly connected subnet holds the desk's address
(the narrowest one), then the routers of the routes through that interface (the default
route's, or under a full-tunnel VPN the one its server's host route goes through), as the ARP
table knows them there. It dials only when one of them has the recorded address. So a
tethered phone or an LTE dock with a preferred default route is not taken for the desk's
router, and a corporate VPN, whether it adds a tunnel beside the LAN's default route or
replaces it, does not hide the router below it. No connected subnet holding the desk's address
is away, whatever else the laptop sees. Otherwise it does not dial, says once in its journal
`not on the desk's network`, and looks again every 10 s; back on the desk's network, it links
within that.

- **A pairing made before this check** has no `gateway`: the agent dials as before, and learns
  the router at its next link, then keeps it. That is safe: the link only comes up with the
  paired desk, authenticated, so the laptop was on the desk's network when it learned.
- **No router known yet** (pairing, or that first link, came before any traffic through the
  router, so the ARP table did not have it): the same, the next link learns it.
- **A desk paired on an IPv6 address**: the check reads only IPv4 routes, so no router is
  recorded and the agent dials as it did before.
- **A new router at the desk** has another hardware address: the agent never dials again,
  and its journal says so, until `navette pair` is run again on both machines and the agent
  restarted (`systemctl --user restart navette`), since a running agent read the
  previous pairing when it started.

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
