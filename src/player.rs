//! In-process audio preview with a loop whose bounds can change while it plays.
//!
//! The audio callback reads from shared state, so moving the loop start or end, swapping the
//! source (for example when a stem is muted) or toggling looping takes effect immediately and
//! the play position keeps running: no restart, no repeated chunks.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, OutputCallbackInfo, Sample, SampleFormat, SizedSample, Stream, StreamConfig};
use std::ops::Range;
use std::sync::{Arc, Mutex};

/// Interleaved 16-bit audio. `origin` is the timeline frame of the first frame of `audio`.
#[derive(Clone, Debug)]
pub struct Source {
    pub audio: Arc<Vec<i16>>,
    pub channels: u16,
    pub rate: u32,
    pub origin: usize,
}

impl Source {
    fn frames(&self) -> usize {
        self.audio.len() / usize::from(self.channels.max(1))
    }

    fn frame(&self, index: usize) -> (f32, f32) {
        let channels = usize::from(self.channels.max(1));
        let base = index * channels;
        let left = f32::from(self.audio[base]) / 32768.0;
        let right = if channels > 1 {
            f32::from(self.audio[base + 1]) / 32768.0
        } else {
            left
        };
        (left, right)
    }
}

/// Playback state shared with the audio callback. Positions are timeline frames at the
/// source's sample rate.
pub struct PlayState {
    source: Option<Source>,
    pos: f64,
    range: Range<usize>,
    looping: bool,
    playing: bool,
}

impl PlayState {
    fn new() -> Self {
        Self {
            source: None,
            pos: 0.0,
            range: 0..0,
            looping: false,
            playing: false,
        }
    }

    /// Next stereo frame at the device rate, or `None` when stopped or finished.
    pub fn next_frame(&mut self, device_rate: f64) -> Option<(f32, f32)> {
        if !self.playing {
            return None;
        }
        let source = self.source.as_ref()?;
        let first = source.origin;
        let last = source.origin + source.frames();
        let start = self.range.start.max(first);
        let end = self.range.end.min(last);
        if start >= end {
            self.playing = false;
            return None;
        }
        if self.pos >= end as f64 {
            if self.looping {
                // The end moved behind us or we reached it: wrap to the (current) start.
                self.pos = start as f64 + (self.pos - end as f64) % (end - start) as f64;
            } else {
                self.playing = false;
                return None;
            }
        }
        if self.pos < first as f64 {
            self.pos = start as f64;
        }
        let index = (self.pos.floor() as usize - first).min(source.frames() - 1);
        let fraction = (self.pos - self.pos.floor()) as f32;
        let (l0, r0) = source.frame(index);
        let (l1, r1) = source.frame((index + 1).min(source.frames() - 1));
        let frame = (l0 + (l1 - l0) * fraction, r0 + (r1 - r0) * fraction);
        self.pos += f64::from(source.rate) / device_rate;
        Some(frame)
    }

    #[cfg(test)]
    fn render(&mut self, frames: usize) -> Vec<f32> {
        (0..frames)
            .map(|_| self.next_frame(f64::from(self.source.as_ref().unwrap().rate)).map_or(0.0, |f| f.0))
            .collect()
    }
}

pub struct Player {
    state: Arc<Mutex<PlayState>>,
    _stream: Stream,
}

fn fill<T: Sample + FromSample<f32>>(
    out: &mut [T],
    channels: usize,
    rate: f64,
    state: &Mutex<PlayState>,
) {
    let mut state = match state.lock() {
        Ok(state) => state,
        Err(_) => {
            out.fill(T::EQUILIBRIUM);
            return;
        }
    };
    for frame in out.chunks_mut(channels) {
        let (left, right) = state.next_frame(rate).unwrap_or((0.0, 0.0));
        for (i, slot) in frame.iter_mut().enumerate() {
            let value = match (channels, i) {
                (1, _) => (left + right) * 0.5,
                (_, 0) => left,
                (_, 1) => right,
                _ => 0.0,
            };
            *slot = T::from_sample(value);
        }
    }
}

fn build<T>(
    device: &cpal::Device,
    config: StreamConfig,
    state: Arc<Mutex<PlayState>>,
) -> Result<Stream, String>
where
    T: SizedSample + FromSample<f32>,
{
    let channels = usize::from(config.channels.max(1));
    let rate = f64::from(config.sample_rate);
    device
        .build_output_stream(
            config,
            move |data: &mut [T], _: &OutputCallbackInfo| fill(data, channels, rate, &state),
            |error| eprintln!("audio output error: {error}"),
            None,
        )
        .map_err(|e| format!("Could not open the audio output: {e}"))
}

impl Player {
    pub fn new() -> Result<Self, String> {
        let device = cpal::default_host()
            .default_output_device()
            .ok_or("No audio output device found")?;
        let supported = device
            .default_output_config()
            .map_err(|e| format!("Could not read the audio output format: {e}"))?;
        let format = supported.sample_format();
        let config: StreamConfig = supported.into();
        let state = Arc::new(Mutex::new(PlayState::new()));
        let stream = match format {
            SampleFormat::F32 => build::<f32>(&device, config, Arc::clone(&state))?,
            SampleFormat::I16 => build::<i16>(&device, config, Arc::clone(&state))?,
            SampleFormat::U16 => build::<u16>(&device, config, Arc::clone(&state))?,
            SampleFormat::I32 => build::<i32>(&device, config, Arc::clone(&state))?,
            other => return Err(format!("Unsupported audio output format {other}")),
        };
        stream
            .play()
            .map_err(|e| format!("Could not start the audio output: {e}"))?;
        Ok(Self {
            state,
            _stream: stream,
        })
    }

    fn with<R>(&self, f: impl FnOnce(&mut PlayState) -> R) -> R {
        match self.state.lock() {
            Ok(mut state) => f(&mut state),
            Err(poisoned) => f(&mut poisoned.into_inner()),
        }
    }

    /// Start playing `range` (timeline frames) from its beginning.
    pub fn start(&self, source: Source, range: Range<usize>, looping: bool) {
        self.with(|s| {
            s.pos = range.start as f64;
            s.source = Some(source);
            s.range = range;
            s.looping = looping;
            s.playing = true;
        });
    }

    /// Replace the audio without moving the play position.
    pub fn set_source(&self, source: Source) {
        self.with(|s| s.source = Some(source));
    }

    /// Move the loop bounds while playing; the position keeps running.
    pub fn set_range(&self, range: Range<usize>) {
        self.with(|s| s.range = range);
    }

    pub fn set_looping(&self, looping: bool) {
        self.with(|s| s.looping = looping);
    }

    pub fn stop(&self) {
        self.with(|s| s.playing = false);
    }

    pub fn is_playing(&self) -> bool {
        self.with(|s| s.playing)
    }

    /// Current play position in timeline frames, while playing.
    pub fn position(&self) -> Option<usize> {
        self.with(|s| s.playing.then_some(s.pos as usize))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mono ramp: frame n holds the value n, so the output shows which frame was played.
    fn ramp(frames: usize, origin: usize) -> Source {
        Source {
            audio: Arc::new((0..frames).map(|i| (i as i16) * 16).collect()),
            channels: 1,
            rate: 44100,
            origin,
        }
    }

    fn state(source: Source, range: Range<usize>, looping: bool) -> PlayState {
        PlayState {
            pos: range.start as f64,
            source: Some(source),
            range,
            looping,
            playing: true,
        }
    }

    fn frame_of(value: f32) -> i32 {
        (value * 32768.0 / 16.0).round() as i32
    }

    #[test]
    fn loop_wraps_from_the_end_back_to_the_start() {
        let mut s = state(ramp(100, 0), 10..14, true);
        let played: Vec<i32> = s.render(10).into_iter().map(frame_of).collect();
        assert_eq!(played, vec![10, 11, 12, 13, 10, 11, 12, 13, 10, 11]);
    }

    #[test]
    fn moving_the_end_keeps_playing_and_wraps_at_the_new_end() {
        let mut s = state(ramp(100, 0), 10..20, true);
        let _ = s.render(5); // now at frame 15
        s.range = 10..30; // end moved later: no restart, keeps going past the old end
        let played: Vec<i32> = s.render(18).into_iter().map(frame_of).collect();
        assert_eq!(played[0], 15);
        assert_eq!(played[14], 29);
        assert_eq!(played[15], 10, "wraps to the start at the new end");
        s.range = 10..12; // end moved behind the play position (13): wrap straight away
        let after: Vec<i32> = s.render(3).into_iter().map(frame_of).collect();
        assert!(after.iter().all(|f| (10..12).contains(f)), "{after:?}");
    }

    #[test]
    fn moving_the_start_finishes_the_current_pass_then_uses_the_new_start() {
        let mut s = state(ramp(100, 0), 10..20, true);
        let _ = s.render(4); // at frame 14
        s.range = 18..20; // start moved past the position
        let played: Vec<i32> = s.render(8).into_iter().map(frame_of).collect();
        assert_eq!(played, vec![14, 15, 16, 17, 18, 19, 18, 19]);
    }

    #[test]
    fn one_shot_stops_at_the_end_and_loop_can_be_toggled_live() {
        let mut s = state(ramp(100, 0), 10..13, false);
        let played = s.render(6);
        assert_eq!(frame_of(played[2]), 12);
        assert_eq!(played[3], 0.0);
        assert!(!s.playing);
        let mut s = state(ramp(100, 0), 10..13, false);
        let _ = s.render(2);
        s.looping = true;
        let more: Vec<i32> = s.render(4).into_iter().map(frame_of).collect();
        assert_eq!(more, vec![12, 10, 11, 12]);
    }

    #[test]
    fn source_swap_keeps_the_position_and_origin_offsets_the_timeline() {
        // Audio covers timeline frames 50..150.
        let mut s = state(ramp(100, 50), 60..70, true);
        assert_eq!(frame_of(s.render(1)[0]), 10); // timeline frame 60 = audio index 10
        let _ = s.render(2);
        s.source = Some(Source {
            audio: Arc::new(vec![32; 100]),
            channels: 1,
            rate: 44100,
            origin: 50,
        });
        assert!(s.pos >= 62.0 && s.playing);
        assert_eq!(frame_of(s.render(1)[0]), 2); // the new audio is heard from the same spot
        // A range outside the audio ends playback instead of panicking.
        s.range = 500..600;
        assert!(s.next_frame(44100.0).is_none());
    }

    #[test]
    fn device_rate_changes_the_step() {
        let mut s = state(ramp(100, 0), 0..100, false);
        let first = s.next_frame(88200.0).unwrap().0;
        let second = s.next_frame(88200.0).unwrap().0;
        assert_eq!(frame_of(first), 0);
        assert!((frame_of(second) as f32 - 0.5).abs() <= 0.5, "half a source frame per device frame");
    }
}
