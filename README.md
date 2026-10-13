# Navette

The "work-from-home mode" for two Omarchy 4 (Hyprland, Wayland) machines on the same home LAN,
written in Rust. From the home desk (several monitors, keyboard, mouse, mic, speakers), it drives
a separate, employer-managed work laptop without unplugging anything: some of the home monitors
show the laptop's output fullscreen, as if they were its own monitors, focus follows the pointer
between the two machines, and the clipboard is shared both ways.

It is one program, `navette`, with two roles:

- **At home**, `navette` takes the reserved monitors itself and draws the laptop's output on
  them. Remote sound plays on the home speakers; the mic goes to one machine at a time, switched
  by a hotkey or a bar indicator.
- **On the laptop**, `navette agent` runs as a systemd user service, without root. It dials out to
  the home machine: nothing listens on the laptop. Home listens on its LAN address only and
  refuses any peer it has not been paired with (a one-time PIN, then mutual TLS).

Navette is built on the library crates of [gliff](https://github.com/omacom/gliff) (capture,
encode and decode, input, protocol, transport), taken from the fork
[navette-gliff](https://github.com/KristenJestin/navette-gliff) (branch `navette`: upstream plus
Navette's changes, kept in sync with upstream) and pinned by commit. What
is specific to the work-from-home mode (pairing, modes, monitor mapping, mic routing) is Navette's
own.

Agents read `AGENTS.md`. Work in progress: see the issues.

## Build

```
cargo build
cargo test
```

## Licence

Apache-2.0. Code reused from gliff (MIT) keeps its MIT notice.
