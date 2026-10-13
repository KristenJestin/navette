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
- **At home**, the changes to the desk are a window rule and, with `navette serve --audio`,
  the mic's hotkey: windows that would open on any workspace of the reserved monitor open on
  another home monitor instead of under the surface. Both are undone when the mode stops, when
  the link drops, and at the next start after a crash, and made again after a config reload.
  The mode also ends if the surface goes away (the monitor unplugged, Hyprland restarted at
  home) or home cannot decode the picture.
- **The home mic**, with `navette serve --audio`, goes to the remote machine while the mode runs,
  and the hotkey switches it either way ([the mic](mic.md)).

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
