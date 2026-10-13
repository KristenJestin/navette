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
  through a small jitter buffer. PipeWire mixes it like any application, so it has its own
  volume in the mixer. Home's own default output is never changed.

## The jitter buffer

Navette's nodes ask PipeWire for cycles of 512 samples (11 ms) at most; a graph whose other
apps ask for less runs shorter ones. (Asking for one 10 ms frame, 480 samples, ran both graphs
in cycles of 256, as PipeWire rounds down to a power of two: 5 ms, short enough for a busy desk
to miss some, heard as crackles that no buffer can absorb.)

What the buffer keeps after each cycle of the playback (its *target*) is at least a cycle's
worth plus 10 ms: in cycles of 512, about 21 ms are left after each cycle, 32 ms before it; in
PipeWire's default cycles of 1024 (21 ms), 31 ms and 52 ms. It starts 20 ms above that, as the
link has not shown how it behaves yet, and comes down once it is calm. The extra cycle is
there because the laptop sends a cycle of its own at a time: as the two clocks drift, one of
them now and then reaches home just after the cycle it used to be in time for, and that cycle
finds a cycle's worth less. Without it, the buffer ran dry every few minutes even on a cable.

- **The link falls behind** (Wi-Fi): each time the buffer runs dry (an *underrun*, a gap of
  silence), the target grows by 10 ms, up to 100 ms. A silence of more than half a second is
  the laptop's sound pausing (music paused, the end of a video): no underrun, and the target
  stays. Two minutes in which the buffer always
  had 30 ms to spare lower it by 10 ms, down to the start.
- **The clocks drift**: the two sound cards never run at quite the same rate. Rather than drop
  sound once the buffer is too full, or run dry once it is empty, home asks the playback to take
  the sound up to 0.5 % faster or slower (0.1 % for each 10 ms off the target), which
  PipeWire's own resampler does on the stream (`SPA_PROP_rate`). A drift of 200 ppm costs
  0.02 %, which nobody hears.
- **A burst** after a long stall, more than 100 ms beyond the start, is dropped (a *trim*): the
  sound would otherwise lag that much behind until the rate caught up.

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
disappears with it and WirePlumber falls back to another output on its own; once the agent is
back and connected, it puts the saved default back at once, unless the user chose another one
meanwhile, and removes the file.

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

## The counters

Every 10 seconds while it plays, and once more when the link drops, home prints how the
remote's sound fares; each kind of event is heard as a crackle, and is counted since the sound
started:

```
navette: audio: buffer 52 ms, target 31 ms, rate +0.004 %; underruns 0, trims 0, concealed 0, lost 0, late 0, dropped 0
```

- *buffer*: what the jitter buffer holds now; *target*, what it aims to keep after each
  cycle of the playback; *rate*, how much faster (or slower) than the graph the playback takes
  the sound, to stay on that target while the two clocks drift.
- *underruns*: the playback found the buffer empty: a gap of silence, then a wait for the
  buffer to fill again; counted once the sound is back, unless it stayed away for more than
  half a second (a pause).
- *trims*: the buffer was past its maximum and its oldest sound was dropped: a jump.
- *concealed*: frames that never came, filled from the next packet's FEC or by concealment.
- *lost*: frames that never came, in a gap too long to fill: the sound jumps over them.
- *late*: frames that came after a later one, too late to play.
- *dropped*: frames dropped at home because the audio thread had 64 waiting.

On the remote machine, the agent prints, at most every 10 seconds, the frames it dropped
because the link had no room for them (`N frames dropped, the link had no room`); home sees
them as concealed or lost.

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

8. Crackles: with `navette serve --audio` in a terminal and the laptop on Wi-Fi, play music on
   the laptop for ten minutes or more. Listen for crackles, and note the last
   `navette: audio:` line home printed: each counter says which kind of event a crackle was.
   Also run `journalctl --user -u navette-agent | grep 'frames dropped'` on the laptop.
   Report the last line, the laptop's lines, and how many crackles were heard.

On loopback, between two private PipeWire instances with a 1024-sample quantum, the gap was
26 to 41 ms.
