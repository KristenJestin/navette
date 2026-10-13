# Navette

The "work-from-home mode" for two Omarchy 4 (Hyprland, Wayland) machines on the same home LAN,
written in Rust. From the home desk (several monitors, keyboard, mouse, mic, speakers), it drives
a separate, employer-managed work laptop without unplugging anything: a preset reserves some of
the home monitors, shows the laptop's output on them fullscreen as if they were its own monitors,
lets focus follow the pointer between the two machines, switches audio by hand, and shares the
clipboard both ways.

Navette does not move the pixels itself: the video, input injection and the clipboard are
[gliff](https://github.com/omacom/gliff)'s job (one gliff client window per reserved monitor).
Navette is the orchestration on top — it applies the Hyprland preset, places the gliff windows,
and owns the audio toggle and its indicator. It drives Hyprland over its IPC socket and gliff over
its CLI or as a library.

Agents read `AGENTS.md`. Work in progress: see the issues.

## Build

```
cargo build
cargo test
```

## Licence

Apache-2.0. Code reused from gliff (MIT) keeps its MIT notice.
