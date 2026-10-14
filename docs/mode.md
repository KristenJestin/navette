# The mode: the work monitors

A mode gives one or more home monitors to the paired remote machine. For each one, the remote
makes a headless output that copies that monitor (its mode, scale and orientation), captures and
encodes it, and streams it over the link; home covers the monitor with a fullscreen overlay
surface and shows the stream. The remote's outputs stand in the home monitors' left-to-right
order, so the pointer crosses from one work monitor to the next as on one machine. Home monitors
are never moved or reconfigured.

## Setting it up

Pair the two machines first (see [the link](link.md)). Then, at home, name the monitor the
remote machine takes, by its output name (`hyprctl monitors` lists them), in `mode.conf` in
Navette's config directory:

```
monitor = DP-2
```

or several, separated by commas, up to eight, side by side or not:

```
monitor = DP-2, DP-3
```

Their order in `mode.conf` does not matter: the remote's outputs follow where the monitors
stand at home, from left to right (by their position in `hyprctl monitors`).

A monitor can also be named by its description, the make and model `hyprctl monitors` shows
(Hyprland's own `desc:` rule), so that a change of connector order breaks nothing:

```
monitor = desc:Dell Inc. DELL U2723QE, DP-3
```

`desc:` is followed by the start of the description (no comma in it); give more of it, the
serial included, when two monitors share a model: a description that fits two monitors, or none,
stops the start with a message that names them, and nothing is changed.

## Named modes

A `[name]` section in `mode.conf` defines a mode of its own. The keys before the first section
are the default of every mode: `release`, `closed_lid` and `mic_hotkey` (a section may set its
own), and a `monitor` among them is the one unnamed mode, so a file with no sections keeps
working, and `navette mode start` with no name starts it.

```
release = ctrl+alt+q

[office]
monitor = desc:Dell Inc. DELL U2723QE, desc:LG Electronics LG ULTRAWIDE

[call]
monitor = DP-2
closed_lid = on
```

A name is letters, digits, `-`, `_` and `.`, 32 characters at most. With no name,
`navette mode start` starts the unnamed mode, or the only named one, and with several asks for a
name.

```
navette mode list           # each mode and its monitors, as written
navette mode start office
navette mode start call     # office stops first: its monitors are back home, then call starts
navette status              # mode `call` runs on DP-2; the remote machine is connected
```

`mode start <other>` while a mode runs checks the other one first: when it fails (a name that is
not defined, a monitor that is not there, a description that fits two), the running mode is left
as it is. Otherwise the running mode stops as `mode stop` does, everything is given back, and the
answer says so before it says what the new mode shows. A switch is refused, with nothing
changed, while the running mode is itself still starting (wait for its answer, or `mode stop`).
If the new mode then cannot start (its cover cannot go up, the remote cannot make its outputs),
the answer says both: the first mode was stopped and its monitors are back home, and why the
second did not start; no mode runs. Home numbers each start and the remote says the number back
in its answers, so that an answer of a mode already stopped never ends or confirms the next one
(an agent too old to say it back is taken at its word). `navette status` says which mode runs
(`no mode is running` otherwise), on which monitors, and whether the remote machine is connected.
`mode list` reads the file and does not need `navette serve`.

## Running it

At home, `navette serve` runs as before; on the remote machine, `navette agent`. Then, at home:

```
navette mode start     # DP-2, DP-3 show the remote machine's NAVETTE-1, NAVETTE-2
navette mode stop      # DP-2, DP-3 are back home
```

`mode start` checks everything before it changes anything, and says what is missing: no
`mode.conf`, a monitor named twice or that does not exist at home, or no remote machine
connected. If the link to the remote machine drops before it has answered for every output,
`mode start` gives back what it had changed and ends with `navette: the link to the remote
machine dropped while the mode was starting; nothing was changed` (a non-zero exit); if the home
desk could not be given back, it says so instead, and it is restored at the next start. A drop
once the mode is up only gives the monitors back, as below. `mode stop` answers once the
monitors are back. The two commands reach `navette serve` over a Unix socket in
`$XDG_RUNTIME_DIR/navette`, private to the user.

While the mode runs:

- **On the remote machine**, the outputs are called `NAVETTE-1`, `NAVETTE-2`…, from left to
  right, placed right of its own monitors, side by side. The name follows the place and
  nothing else: the n-th reserved monitor from the left is `NAVETTE-n` on every start, after a
  Hyprland restart there, and whatever the last mode was (the names are
  [listed for your config](#the-remotes-workspaces)). Each one lives on its own: when its
  picture stops (a Hyprland restart there), the picture starts again on it, or on a new one in
  the same place if it is gone, after a wait that doubles from one second up to thirty; after
  six failures in a row of any one of them, every output is removed and the mode ends at home
  with the reason. Their monitor rules come back after a config reload, and they are removed
  when the mode stops or the link drops. Those left by an agent that crashed are removed when
  the agent starts again.
- **Each output is a stream of its own**, with its own encoder and its own rate controller,
  over the one link. Some GPUs encode fewer streams at once than there are outputs: when the
  remote machine cannot start one more, the mode does not start and `mode start` says which
  output could not stream.
- **At home**, the changes to the desk are the reserved monitors' workspaces, a window rule
  per reserved monitor and, with `navette serve --audio`, the mic's hotkey. Before the surfaces
  open, every workspace with windows on a reserved monitor moves whole to the leftmost home
  monitor kept home: each stays a workspace of its own, out of sight, and that monitor still
  shows what it showed. Windows that would open on any workspace of a reserved monitor open
  there too instead of under a surface. These changes are undone when the mode stops, when the
  link drops, when `navette serve` stops, and at the next start after a crash: each workspace
  goes back to its monitor, and each reserved monitor shows the workspace it showed, even with
  workspace rules that bind workspaces to monitors. A workspace whose windows were all closed
  meanwhile is not made again, except the one a monitor showed, which it shows again, empty.
  After a config reload they are made again. The mode also ends if any one surface goes away
  (its monitor unplugged, Hyprland restarted at home) or home cannot decode a picture.
- **No home window stays under a surface.** A home window that lands on a reserved monitor
  anyway (moved to one of its workspaces, dragged onto it, opened with an explicit
  `[workspace N silent]`) goes to that same home monitor, on
  the workspace it shows; it takes the focus only if it had it, and then brings the pointer
  home too. A workspace moved whole onto a reserved
  monitor goes back to the home monitor whole, and shows there; a workspace of a reserved
  monitor that it does not show goes too, and stays out of sight. Special workspaces stay
  where they are. These moves are not undone when the mode stops: the windows are left home,
  and the monitor takes windows again as usual.
- **The remote's sound**, with `navette serve --audio`, plays at home while the mode runs, and
  on the remote machine's own output again once it stops ([the sound](audio.md)).
- **The home mic**, with `navette serve --audio`, goes to the remote machine while the mode runs,
  and the hotkey switches it either way ([the mic](mic.md)).

## The remote's workspaces

The outputs have fixed names so that the remote machine's Hyprland config can name them. With
no rule, Hyprland gives a new output the next free workspace number, the bar shows no current
workspace for it, and a bind that sends a window to a monitor by name fails with
`Invalid monitor`. On a config that keeps one range of workspaces per monitor, add the outputs
to it. Here a laptop screen `eDP-1` has workspaces 1 to 10, and a mode with two reserved
monitors adds 11 to 20 on `NAVETTE-1` and 21 to 30 on `NAVETTE-2`, in Hyprland's Lua config
(0.56):

```lua
for i = 1, 10 do
  hl.workspace_rule({ workspace = tostring(i), monitor = "eDP-1" })
end
for i = 11, 20 do
  hl.workspace_rule({ workspace = tostring(i), monitor = "NAVETTE-1", default = (i == 11) })
end
for i = 21, 30 do
  hl.workspace_rule({ workspace = tostring(i), monitor = "NAVETTE-2", default = (i == 21) })
end

-- Send the focused window to a monitor by name.
hl.bind("SUPER + SHIFT + F1", hl.dsp.window.move({ monitor = "NAVETTE-1" }))
hl.bind("SUPER + SHIFT + F2", hl.dsp.window.move({ monitor = "NAVETTE-2" }))
```

`NAVETTE-1` is the leftmost reserved monitor at home, `NAVETTE-2` the next one, and so on:
list as many as the most monitors you will reserve. Hyprland creates each output with
the exact name Navette asks for (measured on 0.56.2: removing an output and creating it again,
in any order, never gives it a suffix), so the rules find it as soon as it appears, and its
first workspace is the `default` one.

When no mode runs, these outputs do not exist, and nothing in the config fails:

- A rule for a missing monitor is ignored, and the workspace it names opens on a monitor that
  is there, such as the laptop screen. A window left on workspace 11 goes to `NAVETTE-1` with
  its workspace when the output appears, and comes back to the laptop screen when the mode
  stops.
- A bind that names a missing monitor does nothing: Hyprland answers
  `Invalid monitor / monitor doesn't exist`, as it does for any bind to a monitor that is not
  plugged in. Navette does not try to run it for you.

Navette never writes this config: it declares each output's mode, place and scale at run time
(`hl.monitor`), and those rules are not workspace rules.

## Closing the remote laptop

The remote laptop can be closed while a mode runs, and opened again, if home's `mode.conf`
says so (it is off by default):

```
closed_lid = on     # or off
```

- **Closed**, the laptop's own screen (the output named `eDP-…`, `LVDS-…` or `DSI-…`; both,
  on a laptop with two) is disabled within a second, and Hyprland moves its workspaces to
  Navette's outputs. The laptop does not suspend: the agent holds logind's
  `handle-lid-switch` inhibitor lock, which needs no root, for as long as the mode runs. The
  agent reads logind's `LidClosed` once a second, on a thread of its own, so neither logind
  nor Hyprland ever holds home's keyboard and pointer; the mode starts at once, and the lid
  is watched once logind has given the lock.
- **Opened**, the screen comes back with the mode, place, scale and orientation it had, beside
  Navette's outputs, and Hyprland gives it back the workspaces it had.
- **A config reload** or a script on the lid that turns the screen on while the lid is closed
  is undone at once; after a Hyprland restart, the screen is disabled again once Navette's
  outputs are back.
- **Undone** when the mode stops, when the link drops and when the agent stops: the screen
  comes back first, then the lock is released, and logind handles the lid as it always does.
  The rule that brings the screen back is kept in `$XDG_RUNTIME_DIR/navette/lid` until it is
  back: when Hyprland could not be reached to take it, the agent tries again at Hyprland's
  next event, at the next mode, and when it starts again after a crash (its lock went with
  it). If the agent's loop fails, the agent ends and systemd starts it again.
- **When the lid cannot be watched**, the mode runs without the option, the lid does what it
  always does, and `navette serve` says why at home, as the agent does in its journal
  (`journalctl --user -u navette-agent`). Nothing else is tried. That is when logind or the
  laptop's policy refuses the lock, when `LidClosed` cannot be read (then no lock is taken;
  if it stops being readable for five seconds, the screen comes back and the lock is let
  go), and without `XDG_RUNTIME_DIR`. An agent too old to know `closed_lid` is named in
  `navette mode start`'s answer.

Nothing else on the laptop changes: its idle lock works as usual, and so does a lock on
closing the lid (Omarchy's lid bind locks the session when no physical monitor is plugged in;
Navette's outputs do not count as one). It is unlocked from the home monitors.

## Keyboard and pointer

Focus follows the pointer, with nothing to click. Over a reserved monitor, the keyboard,
the pointer, the wheel and every shortcut, Super included, go to the remote machine; over any
other monitor they stay home.

- **From one work monitor to the next**, the pointer moves from one remote output to the next,
  and the keyboard stays with the remote machine. It crosses where home's compositor puts it:
  the remote's outputs stand side by side, tops aligned, but the position on each comes from
  home. With two monitors of different heights, or not level at home, the pointer crosses at
  the point home puts it on the next monitor, and below the bottom of the shorter one it does
  not cross, as at home. Each reserved monitor has its own surface, and the remote machine its
  own virtual keyboard and pointer on each output. A key held as the pointer crosses (Shift,
  Ctrl, Super) stays down: it goes up on the output the pointer leaves and down again on the
  next one, as a virtual keyboard's modifiers are its own; leaving for a home monitor releases
  everything. A drag (a button held, a window moved by its title) goes from one work monitor
  to the next as on a single desk: home's compositor keeps the pointer's events for the
  surface where the button went down until it goes up, and that surface sends the motion past
  its edge to the work monitor under it. A drag never leaves the work monitors: past their
  outer edge, onto a home monitor, it stops at that edge, and when the button goes up there
  the pointer is home's again.

- **Shortcuts.** While the pointer is over the reserved monitor, home's surface holds the
  keyboard and a shortcuts inhibitor, so home's binds do not fire. A bind flagged
  `dont_inhibit` (the mic toggle, say) still fires at home, everywhere; its key does not reach
  the remote machine.
- **The release hotkey** gives the keyboard back to home without moving the pointer:
  `Shift+Escape` by default (Escape alone still goes to the remote). Home's shortcuts work
  at once, and typing goes to the home window that had the keyboard last, among those shown
  on the other monitors (with none, to no window until the pointer is over one). The pointer
  stays where it is: the focus is given with `cursor:no_warps` set for that moment, then put
  back as it was. A modifier counts as the key pressed, even
  when a keyboard option makes it something else (Shift while Caps Lock is on, with
  Omarchy's `shift:both_capslock_cancel`). Moving the pointer off the monitor and back
  captures it again. Both moves are said by a short desktop notification, `Keyboard → home`
  when the hotkey is typed and `Keyboard → remote` the next time a reserved monitor takes the
  keyboard (once, whichever reserved monitor it is); moving between reserved monitors with no
  release says nothing. These notifications replace one another, apart from the mic's. Choose another chord (modifiers and one key), or
  none (`release = none`, or `release =` left empty), in `mode.conf`, where `#` starts a
  comment:

  ```
  release = ctrl+alt+q     # or: release = none
  ```

  With `RUST_LOG=navette=info`, `navette serve` says each time the hotkey gives the keyboard
  back; with `RUST_LOG=navette=debug`, it also says when the hotkey's key came with other
  modifiers, and which (no other key typed is ever logged).

- **Keys held** when the pointer leaves the monitor, when the hotkey is typed, when the mode
  stops or when the link drops are all released on the remote machine: none stays down.
- **Typing** uses home's keymap, sent to the remote machine (and sent again when home's layout
  changes), so every key types there what it types at home. Caps Lock and Num Lock follow
  home's: as they are at home when the keyboard crosses, so they are on the remote machine.
  A keymap that includes files is refused there, and the one before it stays.
- **The remote machine's virtual keyboard and pointer** are seat-wide there: on the remote
  machine, Hyprland sends what they type to the window its own focus rules pick, as for its
  own keyboard. With `input:follow_mouse = 1` (Hyprland's default) on the remote machine, keys
  go to the window under the pointer on `NAVETTE-1`; with another setting, to the window that
  had the focus there.
- **The pointer** lands at the same place on each output whatever scale either machine uses.
  Over the monitor it is home's own default arrow, drawn by home's compositor at home's
  pointer, so it never lags, and there is only one: while the mode runs, the remote machine
  hides its cursor whenever it is over one of the `NAVETTE-<n>` outputs, and shows it over its
  own screens, so the laptop's trackpad still shows it there (`cursor:invisible`, set at
  runtime each time the pointer crosses onto another monitor, put back when the mode stops,
  when the link drops, when the agent stops or starts again after a crash, and set again after
  a config reload or a Hyprland restart there). The reason: a headless output has no cursor plane
  (aquamarine's headless backend), so Hyprland draws its cursor into the output's frames, and
  the capture copies it whatever it asks; the picture would show a second pointer, trailing
  behind home's. Hyprland has no cursor setting per monitor, hence the switch at each crossing:
  for the few milliseconds it takes, the picture may still show the remote pointer, and a
  pointer parked on the laptop's screen right at its edge shows its tip on the first output.
  The cost is the remote cursor's shape: no text cursor over text. One switch,
  `REMOTE_CURSOR` in `src/display/mod.rs`, brings it back (and the second pointer with it).
- **On the link**, input has a queue of its own that never blocks: a key goes ahead of a
  picture waiting to be sent, and if the link stalls long enough to fill it, everything held
  is released rather than a release being lost.

Hyprland hands the keyboard over at the pointer's first movement once it has crossed onto the
monitor, and takes it back home as soon as the pointer leaves, even onto a home monitor with no
window.

## Decoding at home

Home decodes on its GPU (Vulkan Video) and hands the picture to the compositor as a dmabuf, with
no copy. When the compositor does not take the decoder's linear dmabufs (Hyprland on an NVIDIA
GPU lists only tiled ones), the GPU still decodes and the picture is read back and shown through
shared memory: one copy; so it is too when the decoder's GPU is not the one the compositor
draws with. With several outputs, each one decodes on a device of its own, opened on the
same GPU, never shared between two decoders; when one cannot be opened, `navette mode start`
fails and names its monitor: the CPU does not decode the streams offered for a GPU.
Without a usable GPU, or with `GLIFF_VIDEO=cpu`, home decodes on the CPU (OpenH264).
A decoder that fails three times in a row, or on a keyframe (a GPU device lost does that), is
given up: a GPU decoder for a new one on the GPU opened again, once, then for the CPU, each time
with a keyframe asked of the remote; when the CPU decoder fails too, the mode ends with the
reason and the monitor is given back. The report counts the decoders lost.
`navette serve` says which path it took, and every ten seconds prints, for each remote output,
the frames, the frame rate, the decode time and the time from capture to decoded picture, on
stderr.

## Checking home's side without a remote machine

`examples/synthetic_agent.rs` is `navette agent` with a moving pattern encoded on the CPU instead
of the captured output: the real link and the real headless output, for a machine whose capture
cannot be read (an NVIDIA GPU's headless Hyprland gives only tiled buffers).

```
cargo run --release --example synthetic_agent -- <remote config dir>
```

## Measuring how sharp still text gets

A still picture is first sent at the rate the link controller has reached, then re-encoded up to
eight times, every 200 ms, at the controller's ceiling, so small text sharpens within about a
second once the screen stops changing. `still_text_measure` measures it with no compositor and
no network: the real stream session encodes a page of invented text on the CPU, over a simulated
LAN to a home that decodes it, and prints the PSNR of each decoded frame against the page, then
the frame rate and latency while the page scrolls.

```
tools/still-text-page.sh /tmp/page.ppm 2560 1440
NAVETTE_PAGE=/tmp/page.ppm cargo test --release --lib still_text_measure -- --ignored --nocapture
```

The CPU encoder (OpenH264) stands in for the remote's VA-API encoder here, and it is a poor
stand-in for refinement: its rate control is not the same, and every change of rate or frame
rate rebuilds it and sends a keyframe, which throws the refined picture away. The remote
machine's own numbers are checked on it.
