# The remote machine's sound

While home asks for it, the remote machine's sound plays on the home speakers or headset,
mixed with home sound. There is no output toggle: both machines are heard at once.

- **On the remote machine**, the agent creates a virtual sink, *Home speakers (Navette)*
  (`navette-home-speakers`), and makes it the default output: what the apps play goes into it.
  It is Opus-encoded (48 kHz stereo, 10 ms frames, in-band FEC) and sent home over the link.
  The sound is read from the sink's monitor, at the scale the apps played it. The sink itself
  holds a volume and mute, which the remote's mixer moves: it stands in for home's output, so
  the volume keys, `wpctl` and the sliders show the level heard at home (see below).
  When home stops asking, when the link drops or when the agent stops, the previous default
  output is put back, then the sink is removed. If PipeWire restarts on the remote machine
  meanwhile, the agent sees its sink gone within a quarter of a second, opens it again and
  makes it the default again; what it puts back at the end is still the default from before.
- **At home**, a playback stream, *Remote machine (Navette)* (`navette-remote-sound`), plays it
  through a small jitter buffer, at 100 %. PipeWire mixes it like any application. Home's
  default output is never changed, only its volume, by the remote's volume keys.

## The volume

There is one volume while a mode runs: home's default output's, where the person listens. The
remote's sound plays at 100 % at home, and home's output volume applies to it like to
everything else.

The remote's sink stands in for home's output on the remote machine. As a mode starts, home
tells it its output's level, and the sink takes it: the remote's mixer shows the volume heard.
The remote's volume keys keep moving the sink, so the remote desktop shows its own volume
animation, but the sink does not scale the sound it sends (its monitor carries the sound
before its volume, `monitor.channel-volumes=false`): its new level is told home and set on
home's default output, mute included. The other way, when home's own keys or its mixer move
its default output, the level is told to the remote and set on the sink. The default output
followed is the one the apps play to; if it changes during the mode, the new one is followed.
When the mode stops, the sink goes away with the remote's own volume as it was, and home's
output keeps the level it was last set to, which the user chose. The level is told as a
linear gain, so a slider at 50 % (0.125) is set as 0.125 on the other side.

A level applied from the other side is not sent back: each side remembers the level the other
side holds, what its own mixer said when that level was applied, and the levels applied since its
mixer last said the other side's, which a held volume key sends faster than a mixer may say
them, until its mixer says something else. Until the sink has said home's level, nothing it says is told, so the level
the session manager restored it at is never taken for the user moving it. A remote one beta
older, which tells its sink's level as the session starts, has it set on home's output the
same way. If both machines move the level within the same few milliseconds, each may end at
the other's; moving it once more sets both.

Home's output is set as the session manager's own mixer sets it: on the card's active route
when it is a card's, so that the card keeps it as the user's, or on the node otherwise.

The sink and home's output set the level in one change of their controls (the overall volume
at unity, the gain of each channel, the mute), and PipeWire's loop tells it at most every
50 ms, so a level is never told half set. Each side looks for a change 20 times a second while
the sound plays, so a held volume key is followed within about 100 ms. A level that cannot be
sent because the link is full is sent as soon as there is room, never dropped.

Each change is logged, so the two journals say where a level stopped: on the remote,
`navette: audio: the sink is at 40 %, told home` and `navette: audio: home set the sink to
40 %`; at home, `navette: audio: the remote set the volume to 40 %, on home's output` and
`navette: audio: home's output is at 40 %, told the remote`. The percentage is the one `wpctl`
shows. Where PipeWire takes it, each side also logs `navette: audio: <sink name> set to 40 %`
(home's output, or `navette-home-speakers` on the remote), or `could not set`; home logs
`navette: audio: following the volume of <sink name>` each time it follows a default output.
A stream that has just opened does not have its volume controls yet, and PipeWire drops a
level set before then: the level is kept and set as soon as the stream has them (`has no
volume yet, 40 % kept for when it has`); a card that has not said its active route yet keeps
home's level the same way.
A level home receives for a session it does not play is logged as `ignored`.

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

The sound starts and stops with the mode, as the mic does: `navette mode start` brings the
remote's sound home, `navette mode stop` sends it back to the remote's own output, and so does
the link dropping. `--audio` only says that home may play it (and reach PipeWire at all):

```
navette serve --audio          # at home
navette mode start             # the remote's sound comes home
navette mode stop              # it plays on the remote machine again
```

Without a mode, nothing of the remote machine plays at home. The agent on the remote machine
needs nothing more: it follows home.

The default output is the one `wpctl set-default` writes (`default.configured.audio.sink` in
PipeWire's `default` metadata), so WirePlumber moves the playing apps to the Navette sink and
back. Before switching, the agent saves the default it replaces in
`$XDG_STATE_HOME/navette/previous-default-sink` (0600), and removes the file once it is put
back. If the agent is killed without a chance to clean up (`SIGKILL`, a crash), the Navette sink
disappears with it and WirePlumber falls back to another output on its own; once the agent is
back and connected, it puts the saved default back at once, unless the user chose another one
meanwhile, and removes the file.

## The messages

Four top-level variants of the link's `LinkMsg` (`src/wire.rs`), numbered apart from the
other services:

| Variant | Direction | Fields |
|---|---|---|
| `48` `AudioStart` | home → remote | the session number |
| `49` `AudioStop` | home → remote | none |
| `50` `AudioFrame` | remote → home | the session and a sequence number, and `data_len`: the Opus packet follows as the message's payload (4000 bytes at most, or the link ends) |
| `54` `AudioVolume` | both ways | the session, the linear gain of the left and of the right channel, and whether it is muted: sent when the remote sink's (or home's default output's) volume or mute changes, by the side whose mixer moved it, and by home as a mode starts |

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
2. At home: `navette serve --audio`. Before any mode, music on the laptop plays on the
   laptop's own speakers, and `wpctl status` there shows its own default sink.
3. `navette mode start`. On the laptop, `wpctl status` shows *Home speakers (Navette)* as the
   default sink (`*`).
4. Play a video on the laptop and music at home: both are heard on the home speakers.
5. `navette mode stop` with music playing on the laptop: silence at home, and the music goes
   back to the laptop's own speakers, which `wpctl status` shows as the default again. Start
   the mode again, then stop `navette serve` (Ctrl-C): the previous default comes back too.
   Start both again, then `systemctl --user stop navette-agent` on the laptop: the same.
6. With a mode running and a video playing on the laptop, restart its sound
   server: `systemctl --user restart pipewire wireplumber`. Within a few seconds, the video is
   heard at home again and `wpctl status` shows *Home speakers (Navette)* as the default; stop
   the mode, and the previous default sink comes back.
7. Latency, a click test: put the laptop next to a home microphone. On the laptop, make a click
   (`sox -n -r 48000 -c 2 click.wav synth 0.005 sine 2000 pad 0.5 1.5`) and play it to the
   laptop's own speakers and to Navette at once:
   `pw-play --target <laptop-speakers-node-name> click.wav & pw-play click.wav`.
   At home, record the microphone (`pw-record mic.wav`) during the click: the gap between the
   laptop's click and the one from the home speakers, read in Audacity, is Navette's latency.
   Repeat five times and report the values.

8. Crackles: with `navette serve --audio` in a terminal, a mode running and the laptop on
   Wi-Fi, play music on
   the laptop for ten minutes or more. Listen for crackles, and note the last
   `navette: audio:` line home printed: each counter says which kind of event a crackle was.
   Also run `journalctl --user -u navette-agent | grep 'frames dropped'` on the laptop.
   Report the last line, the laptop's lines, and how many crackles were heard.

9. The volume. Note home's volume (say 70 %) and start a mode: the laptop's mixer
   (`wpctl status`, the bar) shows 70 % on *Home speakers (Navette)*. With a video playing on
   the laptop and the pointer over a reserved monitor, use the volume keys: the laptop's
   desktop shows its own volume animation, what is heard at home goes down once, and home's
   own mixer shows the same level as the laptop's. At home, *Remote machine (Navette)* stays
   at 100 %. Mute on the laptop (the mute key): home's output is muted; unmute, and it comes
   back. Then the other way: at home, use home's own volume keys (pointer over a home monitor)
   or its mixer: the laptop's mixer and bar follow; mute at home, and the laptop shows muted.
   Stop the mode: home keeps the level last set, the laptop's own output is at its own volume
   from before. If a level does not follow, `journalctl --user -u navette-agent | grep
   'navette: audio: \(the sink\|home set\)'` on the laptop and the `navette: audio:` lines of
   `navette serve` say on which side it stopped.

On loopback, between two private PipeWire instances with a 1024-sample quantum, the gap was
26 to 41 ms.
