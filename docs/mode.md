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

## Running it

At home, `navette serve` runs as before; on the remote machine, `navette agent`. Then, at home:

```
navette mode start     # DP-2, DP-3 show the remote machine's NAVETTE-1, NAVETTE-2
navette mode stop      # DP-2, DP-3 are back home
```

`mode start` checks everything before it changes anything, and says what is missing: no
`mode.conf`, a monitor named twice or that does not exist at home, or no remote machine
connected. `mode stop` answers once the monitors are back. The two commands reach
`navette serve` over a Unix socket in `$XDG_RUNTIME_DIR/navette`, private to the user.

While the mode runs:

- **On the remote machine**, the outputs are called `NAVETTE-1`, `NAVETTE-2`…, from left to
  right, placed right of its own monitors, side by side. Each one lives on its own: when its
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
  everything. A drag (a button held) stays on the monitor where it began: home's compositor
  keeps the pointer's events for that surface until the button goes up. Carrying it to the
  next output would take each surface knowing where home's other work monitors stand, and the
  remote taking a press on one output's virtual pointer and the motion on another's as one
  drag, which nothing here shows it does.

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
  captures it again. Choose another chord (modifiers and one key), or
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
  hides its cursor (`cursor:invisible`, set at runtime and put back when the mode stops, when
  the link drops, when the agent stops or starts again after a crash, and set again after a
  config reload or a Hyprland restart there). The reason: a headless output has no cursor plane
  (aquamarine's headless backend), so Hyprland draws its cursor into the output's frames, and
  the capture copies it whatever it asks; the picture would show a second pointer, trailing
  behind home's. The cost is the remote cursor's shape: no text cursor over text. One switch,
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
