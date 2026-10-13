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
  captures the default mic, and cancels what the default output plays, read from its monitor
  (`monitor.mode`): home's own default output and default mic are never changed, and the
  canceller's source is no default. What it hears is Opus-encoded (48 kHz stereo, 10 ms
  frames, in-band FEC) and sent while the mic goes to the remote machine. Home apps still have
  the mic: Navette only decides whether the remote machine hears it.

## Switching it

```
navette mic toggle       # to the other machine
navette mic remote       # to the remote machine
navette mic home         # home
navette mic status       # remote, home, or off when no mode runs
```

Each prints where the mic goes once it is done. With no mode running there is nothing to
switch: the command says so and the mic stays home.

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
the next reload. A bind of the user's on the same keys still fires as well: pick free keys.

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
  "format-icons": { "remote": "mic → work", "home": "mic · home", "off": "" },
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
| `51` `MicStart` | the session number: open the source and make it the default mic |
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
   `home`, and the call hears silence. Press it again over a home monitor: they hear you again.
   Do the same with the bar item, mid-call.
5. `navette mode stop`: on the laptop, `wpctl status` shows the previous default source again.

Between two private PipeWire instances, with the speakers' monitor looped back into a virtual
mic as the echo, the canceller took the echo down by 6 to 20 dB once it had adapted, after some
six seconds, unevenly on that synthetic echo; how it does with real speakers in a real room is
the check above.
