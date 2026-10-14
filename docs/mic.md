# The home mic

The home mic serves one machine at a time. When a mode starts it goes to the remote machine;
when the mode stops it comes back home. While the mode runs, a hotkey, `navette mic` or a bar
switches it either way. It needs `navette serve --audio`, the switch that plays the remote
machine's sound at home ([the sound](audio.md)).

- **On the remote machine**, the agent opens a virtual source, *Home mic (Navette)*
  (`navette-home-mic`), and makes it the default mic for the whole mode: a call there picks it
  up like any mic. It plays what home sends, through the same small jitter buffer as the sound
  (30 ms before playing, 120 ms at most), and silence while the mic is home: switching never
  changes the remote's devices, so a call keeps its mic. When the mode stops, the link drops or
  the agent stops, the previous default mic is put back, then the source is removed. It is saved
  meanwhile in `$XDG_STATE_HOME/navette/previous-default-source` (0600), and put back after a
  crash as soon as the agent is connected again, as the default output is, and a source gone with a PipeWire restart is opened again within a quarter of a second.
- **At home**, Navette loads PipeWire's echo canceller (`libpipewire-module-echo-cancel`,
  WebRTC) into its own process for the mode, and unloads it after; a crash takes it away too. It
  captures the mic home's apps have when the mode starts, by its name, and cancels what the
  default output plays, read from its monitor (`monitor.mode`): home's default output is never
  changed, and the canceller's source is no default. What it hears is Opus-encoded (48 kHz
  stereo, 10 ms frames, in-band FEC) and sent while the mic goes to the remote machine. The
  capture stays open for the whole mode, the mic home included: the canceller takes some six
  seconds to adapt, and would start over at each switch. When home's PipeWire restarts, the
  capture, its canceller and the silent source below are opened again within a quarter of a
  second; while they cannot be, the mic says `failed`, and `navette mic toggle` says why.
- **Home apps do not hear the mic while it is remote.** Home's default mic is then a silent
  source of Navette's, *Mic away (Navette)* (`navette-mic-away`): the apps that follow the
  default (most do, a call included) move to it and hear silence, and move back when the mic
  comes home. Muting the mic itself would silence the echo canceller too, which captures it by
  name. The default it replaces is saved in `$XDG_STATE_HOME/navette/previous-default-source`
  (0600) and put back when the mic comes home, when the mode stops, when the link drops, and
  after a crash as soon as `navette serve` starts again. An app set to record from the mic
  itself, by its name rather than the default, still hears it.

## Switching it

```
navette mic toggle       # to the other machine
navette mic remote       # to the remote machine
navette mic home         # home
navette mic status       # remote, home, failed, or off when no mode runs
```

Each prints where the mic goes once it is done. With no mode running there is nothing to
switch: the command says so and the mic stays home. While the mic says `failed`, switching it
says why instead.

The mode binds a hotkey, `SUPER + CTRL + M` unless `mode.conf` names other keys:

```
monitor = DP-2
mic_hotkey = SUPER + ALT + F9
```

It is bound with Hyprland's per-bind `dont_inhibit` flag, so it fires over the remote's monitor
too, where every other shortcut goes to the remote machine. It runs `navette mic toggle` with the
binary `navette serve` was started from. Like the window rule, it goes through the desk recorder:
undone when the mode stops, when the link drops and at the next start after a crash, and bound
again after a config reload. Hyprland 0.56 removes a bind only together with every other bind on
the same keys, so Navette disables its own instead; `hyprctl binds` lists the disabled ones until
the next reload. A bind of the user's on the same keys still fires as well: pick free keys. Keys
Hyprland refuses do not stop the mode: it runs without the hotkey, `navette serve` says why, and
the bar and `navette mic` still switch the mic.

## Saying where it went

At each switch, `navette serve` shows a short notification through the desktop's notification
service (it runs `notify-send`, from libnotify 0.8 or later): `Mic → remote` or `Mic → home`.
That covers the hotkey, the bar, `navette mic` and a mode starting or stopping. A mode that
stops with the mic already home moves nothing, and says nothing. A mic that cannot go where it
should shows `Mic failed` with why, and a switch that does not happen (no mode running, the
mic failed, PipeWire refusing) shows `Mic not switched` with why, since a hotkey has no
terminal to print it to.

Each notification replaces the previous one of the mic, so they do not pile up. Without a
notification service, or without `notify-send`, the mic switches all the same, and `navette
serve` says once that nothing will be shown.

## Showing it in a bar

`navette mic watch` prints where the mic goes, then a line at each change, until it is
stopped. While `navette serve` is not running it prints `off`, and follows it again once it
starts. With `--json`, each line has the fields a Waybar custom module reads; any bar can read
them:

```
{"text":"remote","alt":"remote","class":"remote","tooltip":"The mic goes to the remote machine"}
```

A bar runs `navette mic watch --json` for as long as it lives, shows the state, and calls
`navette mic toggle` on a click. In Waybar, for instance:

```jsonc
"custom/navette-mic": {
  "exec": "navette mic watch --json",
  "return-type": "json",
  "format": "{icon}",
  "format-icons": { "remote": "mic → work", "home": "mic · home", "failed": "mic ✗", "off": "" },
  "on-click": "navette mic toggle"
}
```

A Quickshell bar does the same with a `Process` running `navette mic watch --json`, a
`SplitParser` that reads each line with `JSON.parse`, and `Quickshell.execDetached(["navette",
"mic", "toggle"])` on a click. `navette mic status --json` gives one line, for a bar that polls.

## The messages

Three top-level variants of the link's `LinkMsg` (`src/wire.rs`), a block of their own, all
home → remote:

| Variant | Fields |
|---|---|
| `51` `MicStart` | the session number: open the source and make it the default mic; over a running session, only the number changes |
| `52` `MicStop` | none: put the previous default mic back and remove the source |
| `53` `MicFrame` | the session and a sequence number, and `data_len`: the Opus packet follows (4000 bytes at most, or the link ends) |

Home numbers each mode, and the remote drops a frame of another session. They share the
sound's queues (64 frames each way): a frame is dropped rather than delay the next, an order
never is.

## Checking it on the two machines

1. Install the build on both machines, restart the agent on the laptop, and at home run
   `navette serve --audio`, then `navette mode start`.
2. On the laptop, `wpctl status` shows *Home mic (Navette)* as the default source (`*`); at
   home, `navette mic status` says `remote`.
3. Start a call on the laptop, with the home speakers playing it: speak, and the other side
   hears you, without its own voice coming back (ask them).
4. Press `SUPER + CTRL + M` with the pointer over the work monitor: `navette mic status` says
   `home`, the call hears silence, and a notification says `Mic → home`. Press it again over a
   home monitor: they hear you again, and the same notification, replaced, says `Mic → remote`.
   Do the same with the bar item, mid-call.
5. Meanwhile, at home, `wpctl status` shows *Mic away (Navette)* as the default source while
   the mic is remote, and the home mic again while it is home; a home app recording from the
   default hears silence, then the mic.
6. `navette mode stop`: on the laptop, `wpctl status` shows the previous default source again.

Between two private PipeWire instances, with the speakers' monitor looped back into a virtual
mic as the echo, the canceller took the echo down by 6 to 20 dB once it had adapted, after some
six seconds, unevenly on that synthetic echo; how it does with real speakers in a real room is
the check above.
