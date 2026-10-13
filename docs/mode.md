# The mode: one work monitor

A mode gives one home monitor to the paired remote machine. The remote makes a headless output
that copies that monitor (its mode, scale and orientation), captures and encodes it, and streams
it over the link; home covers the monitor with a fullscreen overlay surface and shows the stream.
Home monitors are never moved or reconfigured.

## Setting it up

Pair the two machines first (see [the link](link.md)). Then, at home, name the monitor the
remote machine takes, by its output name (`hyprctl monitors` lists them), in `mode.conf` in
Navette's config directory:

```
monitor = DP-2
```

## Running it

At home, `navette serve` runs as before; on the remote machine, `navette agent`. Then, at home:

```
navette mode start     # DP-2 shows the remote machine's NAVETTE-1
navette mode stop      # DP-2 is back home
```

`mode start` checks everything before it changes anything, and says what is missing: no
`mode.conf`, a monitor that does not exist at home, or no remote machine connected. `mode stop`
answers once the monitor is back. The two commands reach `navette serve` over a Unix socket in
`$XDG_RUNTIME_DIR/navette`, private to the user.

While the mode runs:

- **On the remote machine**, the output is called `NAVETTE-1`, placed right of its own monitors.
  When its picture stops (a Hyprland restart there), the picture starts again on it, or on a new
  one if it is gone, after a wait that doubles from one second up to thirty; after six failures
  in a row the output is removed and the mode ends at home with the reason. Its monitor rule
  comes back after a config reload, and it is removed when the mode stops or the link drops. One
  left by an agent that crashed is removed when the agent starts again.
- **At home**, the one change to the desk is a window rule: windows that would open on any
  workspace of the reserved monitor open on another home monitor instead of under the surface.
  It is undone when the mode stops, when the link drops, and at the next start after a crash,
  and made again after a config reload. The mode also ends if the surface goes away (the monitor
  unplugged, Hyprland restarted at home) or home cannot decode the picture.

## Keyboard and pointer

Focus follows the pointer, with nothing to click. Over the reserved monitor, the keyboard,
the pointer, the wheel and every shortcut, Super included, go to the remote machine; over any
other monitor they stay home.

- **Shortcuts.** While the pointer is over the reserved monitor, home's surface holds the
  keyboard and a shortcuts inhibitor, so home's binds do not fire. A bind flagged
  `dont_inhibit` (the mic toggle, say) still fires at home, everywhere; its key does not reach
  the remote machine.
- **The release hotkey** gives the keyboard back to home without moving the pointer:
  `Shift+Escape` by default (Escape alone still goes to the remote). Moving the pointer off
  the monitor and back captures it again. Choose another chord (modifiers and one key), or
  none (`release = none`, or `release =` left empty), in `mode.conf`, where `#` starts a
  comment:

  ```
  release = ctrl+alt+q     # or: release = none
  ```

- **Keys held** when the pointer leaves the monitor, when the hotkey is typed, when the mode
  stops or when the link drops are all released on the remote machine: none stays down.
- **Typing** uses home's keymap, sent to the remote machine (and sent again when home's layout
  changes), so every key types there what it types at home.
- **The pointer** lands at the same place on `NAVETTE-1` whatever scale either machine uses. Its
  image over the monitor is the remote machine's cursor (a text cursor over text, say), drawn by
  home's compositor at home's pointer, so it never lags; until the remote has one to show, it is
  the default arrow.
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
draws with. Without a usable GPU, or with `GLIFF_VIDEO=cpu`, home decodes on the
CPU (OpenH264). `navette serve` says which path it took, and every ten seconds prints the frames,
the frame rate, the decode time and the time from capture to decoded picture, on stderr.

## Checking home's side without a remote machine

`examples/synthetic_agent.rs` is `navette agent` with a moving pattern encoded on the CPU instead
of the captured output: the real link and the real headless output, for a machine whose capture
cannot be read (an NVIDIA GPU's headless Hyprland gives only tiled buffers).

```
cargo run --release --example synthetic_agent -- <remote config dir>
```
