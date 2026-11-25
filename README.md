# SP-404SX/A Wave Converter

A Rust desktop sample manager for an SP-404SX/A-formatted card or card copy.
This independent project is not affiliated with, endorsed by, or supported by
Roland Corporation.

**Back up the entire card before making changes.** Writing has been tested on
isolated fixtures, not verified on the sampler; compatibility is not guaranteed.
Do not modify the card concurrently with another program or remove it during an
operation.

![SP-404SX Wave Converter screenshot](assets/screenshot.png)

```sh
cargo run -- gui /path/to/card
```

On Linux, build and install the application and desktop launcher with:

```sh
make
make install
```

The default install prefix is `~/.local`; use `make install PREFIX=/usr/local`
to choose another prefix, and `DESTDIR` for staged packaging. The GUI embeds
`assets/icon-256.png`, a resized copy of `icon.png`, which is also installed
for the desktop launcher. To regenerate it after changing `icon.png`, run
`magick icon.png -resize 256x256 assets/icon-256.png`. The GUI also embeds
`bg.png` as its background skin; rebuild after changing the image.

Use **...** to select the mounted card's root folder, or enter the path and
press Enter or refresh. The last successfully opened folder is remembered in
`$XDG_CONFIG_HOME/sp404sx-wave-converter/last-card-folder` (or
`~/.config/sp404sx-wave-converter/last-card-folder`) and reopened on startup.
A card path supplied on the command line takes precedence. Choose a bank, then
click an empty pad to import audio. To replace a sample, select its pad and use
**Import**; **Delete** removes the selected sample. Both replacement and
deletion require confirmation. Double-click an assigned pad to preview it;
click the playing pad or **Stop** to stop.

To copy or move a sample, select its source pad, choose a destination bank and
pad, and click **Copy** or **Move**. The destination must be empty. **Swap**
exchanges two occupied pads. These operations retain the samples and their pad
settings, and move and swap require confirmation. The controls may be below
the fold in a small window.

Select an assigned pad and click **Edit** to open its waveform in a separate
window. Main-window controls are disabled until the editor is closed; the main
window is not resized. On platforms without separate viewport support the editor
appears inside the main window. Drag to select a region, or enter frame
endpoints; use zoom and scroll for detail, and double-click a waveform to select
all of it. Drag the `[` and `]` tabs to refine the selection. **Preview** plays the
selection, or everything when nothing is selected; enable **Loop preview** to repeat
it. A looping preview follows the selection as you drag its edges, without
restarting. Preview plays through your sound card and does not reproduce sampler
effects.

Trim to the selection, delete it, or normalize the entire sample. The pad panel
shows channel and length and edits playback start/end in frames, gate, pad loop,
forward/reverse, and level (0–127). **Use selection for start/end** sets the
playback bounds without deleting audio. Pad loop is separate from loop preview;
preview does not simulate these pad settings. Balance is unavailable in the
known SX/A metadata. Unrecognized stored setting values are shown as unknown
and preserved.

Undo and redo apply to audio, playback bounds, and settings. **Save to pad**
requires confirmation; a settings-only save leaves the original sample file
and its name untouched and preserves unrelated metadata. Closing without saving
discards edits after confirmation. Editing requires `ffmpeg` and `ffprobe` and
supports 8- or 16-bit mono/stereo PCM at up to 44.1 kHz.

**Detect beats** estimates a constant-tempo grid between 70 and 180 BPM for
rhythmic audio of at least three seconds. It is not guaranteed accurate:
half/double-tempo ambiguity, weak transients, and changing tempo can make the
grid wrong. Check the waveform and adjust BPM and offset manually before
using beat snapping. Snapping applies to waveform selections and is turned
off after edits or undo/redo until the grid is checked again. Grid changes
do not modify the pad's stored tempo.

Editing keeps the decoded sample and audio-edit undo/redo copies in memory.
Long samples or many edits can use substantial memory; loading, analysis, preview
creation, and saving run synchronously and may temporarily freeze the window.

Import requires `ffmpeg` and `ffprobe` on `PATH` and accepts audio formats
supported by the installed FFmpeg. Audio is converted to WAV, preserving the
sample rate up to 44.1 kHz and mono or stereo channels; higher rates are
resampled to 44.1 kHz and multichannel audio is downmixed to stereo.
8-bit PCM is kept at 8-bit; other audio is converted to 16-bit PCM.
Lossy formats do not have a directly comparable PCM bit depth.
Compatibility with the sampler at rates below 44.1 kHz or at 8-bit has not
been verified. Imports reset the selected pad's
settings and set its tempo to 120 BPM; automatic tempo detection is not
implemented. Preview requires `ffplay` and applies the recorded start/end
offsets, but does not reproduce other sampler effects or pad settings.
Conversion runs synchronously and the window may be unresponsive during a
large import.

The command line also supports read-only listing and full-file playback:

```sh
cargo run -- list /path/to/card A
cargo run -- play /path/to/card A 1
```

For a CLI-only build, use `cargo build --no-default-features`. Linux GUI builds
need the system libraries required by `eframe`/`winit` and a native file dialog
backend.

## Recovery and limitations

Writes are staged in `ROLAND/SP-404SX/SMPL/.sp404sx-converter-transaction`.
The program attempts to roll back failed renames and refuses further writes
if this directory is present. Multi-file updates are **not crash-atomic**:
power loss, disconnection, or cleanup failure can leave the card inconsistent
or a partially cleaned transaction. Stop using the card, preserve a copy of
the entire card and transaction directory, and restore the independent backup
if necessary. When present, `operation.txt`, `PAD_INFO.original`, and
`original.sample` may help inspect an interrupted operation. Pad transfers use
`source.original`, `target.original`, and sample snapshots where applicable.
Do not assume all of these files survived. Do not remove the transaction
directory until the card has been checked or restored.

SP-404 CompactFlash conversion is not implemented: its source format and
destructive conversion behavior have not been verified. The card's tempo
detection and full hardware behavior likewise remain unverified.

## Format references

Sample layout follows the [SP-404SX sample format notes](https://gist.github.com/threedaymonk/701ca30e5d363caa288986ad972ab3e0)
and observations from a card. Pad metadata fields are informed by the
[PAD_INFO parser](https://raw.githubusercontent.com/uttori/uttori-data-tools/master/src/audio/sp404-padinfo.ts).
