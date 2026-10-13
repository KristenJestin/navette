# The remote machine's sound

While home asks for it, the remote machine's sound plays on the home speakers or headset,
mixed with home sound. There is no output toggle: both machines are heard at once.

- **On the remote machine**, the agent creates a virtual sink, *Home speakers (Navette)*
  (`navette-home-speakers`), and makes it the default output: what the apps play goes into it.
  It is Opus-encoded (48 kHz stereo, 10 ms frames, in-band FEC) and sent home over the link.
  When home stops asking, when the link drops or when the agent stops, the previous default
  output is put back, then the sink is removed. If PipeWire restarts on the remote machine
  meanwhile, the agent sees its sink gone within a quarter of a second, opens it again and
  makes it the default again; what it puts back at the end is still the default from before.
- **At home**, a playback stream, *Remote machine (Navette)* (`navette-remote-sound`), plays it
  through a small jitter buffer (30 ms before playing, 120 ms at most). PipeWire mixes it like
  any application, so it has its own volume in the mixer. Home's own default output is never
  changed.

## Trying it

The modes will start and stop the sound (#6). Until then, `--audio` makes home ask for it as
long as the link is up:

```
navette serve --audio          # at home
```

The agent on the remote machine needs nothing more: it follows home.

The default output is the one `wpctl set-default` writes (`default.configured.audio.sink` in
PipeWire's `default` metadata), so WirePlumber moves the playing apps to the Navette sink and
back. Before switching, the agent saves the default it replaces in
`$XDG_STATE_HOME/navette/previous-default-sink` (0600), and removes the file once it is put
back. If the agent is killed without a chance to clean up (`SIGKILL`, a crash), the Navette sink
disappears with it and WirePlumber falls back to another output on its own; the next start
finds the default still on the Navette sink, and puts back the saved one when it stops.

## The messages

Three top-level variants of the link's `LinkMsg` (`src/wire.rs`), numbered apart from the
other services:

| Variant | Direction | Fields |
|---|---|---|
| `48` `AudioStart` | home → remote | the session number |
| `49` `AudioStop` | home → remote | none |
| `50` `AudioFrame` | remote → home | the session and a sequence number, and `data_len`: the Opus packet follows as the message's payload (4000 bytes at most, or the link ends) |

Home numbers each start: a frame of an earlier session, still on its way when home stopped,
is dropped rather than taken for a late frame of the new one.

The sound has its own queues between the link and its audio thread, 64 frames each way
(640 ms). Neither side waits on a full one: a frame is dropped rather than delay the ones
after it, the link's reader and its heartbeats never wait on the sound, and home fills a
missing frame from the next packet's FEC, or by concealment. The orders (start, stop) are
never dropped: they wait in order with the frames, however many frames are dropped around them,
so a stop always reaches an audio thread that is slow for a while.

## Checking it on the two machines

1. Install the build on both machines, then on the laptop
   `systemctl --user restart navette-agent`.
2. At home: `navette serve --audio`.
3. On the laptop, `wpctl status` shows *Home speakers (Navette)* as the default sink (`*`).
4. Play a video on the laptop and music at home: both are heard on the home speakers. In the
   home mixer, the volume of *Remote machine (Navette)* changes only the laptop's sound.
5. Stop `navette serve` (Ctrl-C): on the laptop, `wpctl status` shows its previous default sink
   again. Start it again, then `systemctl --user stop navette-agent` on the laptop: the previous
   default comes back too.
6. With `navette serve --audio` running and a video playing on the laptop, restart its sound
   server: `systemctl --user restart pipewire wireplumber`. Within a few seconds, the video is
   heard at home again and `wpctl status` shows *Home speakers (Navette)* as the default; stop
   `navette serve`, and the previous default sink comes back.
7. Latency, a click test: put the laptop next to a home microphone. On the laptop, make a click
   (`sox -n -r 48000 -c 2 click.wav synth 0.005 sine 2000 pad 0.5 1.5`) and play it to the
   laptop's own speakers and to Navette at once:
   `pw-play --target <laptop-speakers-node-name> click.wav & pw-play click.wav`.
   At home, record the microphone (`pw-record mic.wav`) during the click: the gap between the
   laptop's click and the one from the home speakers, read in Audacity, is Navette's latency.
   Repeat five times and report the values.

On loopback, between two private PipeWire instances with a 1024-sample quantum, the gap was
26 to 41 ms.
