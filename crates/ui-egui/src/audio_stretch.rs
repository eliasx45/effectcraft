//! Play Faster's sound: time compression that keeps the pitch (WSOLA, waveform-similarity
//! overlap-add), as Premiere's shuttle does. Resampling the sound faster (FilmCraft's shuttle)
//! raises its pitch with the speed: voices sounded like chipmunks.
//!
//! The input is cut into Hann-windowed grains `WINDOW_S` long, read `speed` × further apart
//! than they are laid down in the output (half a grain apart, where the windows sum to one).
//! Each grain is taken within `SEEK_S` of where it nominally starts, where it best continues
//! the grain before it (cross-correlation with that grain's natural continuation), so the
//! waveforms join in phase instead of beating.

/// Grain length (seconds): long enough for a voice's lowest pitch period, short enough not to
/// echo.
const WINDOW_S: f64 = 0.040;
/// How far (seconds) a grain may move from its nominal place to join in phase.
const SEEK_S: f64 = 0.012;

/// Interleaved stereo in, the same sound `speed` × shorter out, at the same pitch.
pub struct Stretch {
    speed: f64,
    /// Grain length and output hop (frames).
    n: usize,
    hop: usize,
    seek: i64,
    window: Vec<f32>,
    /// Input frames not yet consumed (interleaved stereo); `input[0]` is input frame `offset`.
    input: Vec<f32>,
    offset: i64,
    /// Where the next grain nominally starts (input frame).
    nominal: f64,
    /// Where the previous grain started (input frame).
    prev: Option<i64>,
    /// Output being overlap-added: the next `n` frames (interleaved stereo).
    acc: Vec<f32>,
}

impl Stretch {
    /// `speed` × faster at `rate` Hz (`speed` ≥ 1).
    pub fn new(rate: u32, speed: f64) -> Stretch {
        let n = ((f64::from(rate.max(8000)) * WINDOW_S) as usize / 2 * 2).max(64);
        let window = (0..n).map(|i| (0.5 - 0.5 * (std::f64::consts::TAU * i as f64 / n as f64).cos()) as f32).collect();
        Stretch {
            speed: speed.max(1.0),
            n,
            hop: n / 2,
            seek: (f64::from(rate.max(8000)) * SEEK_S) as i64,
            window,
            input: Vec::new(),
            offset: 0,
            nominal: 0.0,
            prev: None,
            acc: vec![0.0; n * 2],
        }
    }

    /// How many times faster it plays.
    pub fn speed(&self) -> f64 {
        self.speed
    }

    /// Input frames held.
    fn frames(&self) -> i64 {
        (self.input.len() / 2) as i64
    }

    /// Input frame `f` as (left, right); silence outside what is held.
    fn at(&self, f: i64) -> (f32, f32) {
        let i = usize::try_from(f - self.offset).ok().and_then(|i| i.checked_mul(2));
        match i.and_then(|i| Some((*self.input.get(i)?, *self.input.get(i + 1)?))) {
            Some(lr) => lr,
            None => (0.0, 0.0),
        }
    }

    /// Feed `samples` (interleaved stereo) and take the output they complete.
    pub fn process(&mut self, samples: &[f32]) -> Vec<f32> {
        self.input.extend_from_slice(samples);
        let mut out = Vec::new();
        let n = self.n as i64;
        loop {
            let nominal = self.nominal.floor() as i64;
            // Every candidate grain and the previous grain's continuation must be held.
            let need = (nominal + self.seek + n).max(self.prev.map_or(0, |p| p + self.hop as i64 + n));
            if need > self.offset + self.frames() {
                break;
            }
            let start = match self.prev {
                None => nominal,
                Some(p) => self.best_join(p + self.hop as i64, nominal),
            };
            for i in 0..self.n {
                let (l, r) = self.at(start + i as i64);
                let w = self.window.get(i).copied().unwrap_or(0.0);
                if let Some([al, ar]) = self.acc.get_mut(2 * i..2 * i + 2) {
                    *al += w * l;
                    *ar += w * r;
                }
            }
            // The first half is complete: no later grain reaches it.
            out.extend(self.acc.drain(..2 * self.hop));
            self.acc.resize(2 * self.n, 0.0);
            self.prev = Some(start);
            self.nominal += self.hop as f64 * self.speed;
            // Drop the input no grain needs any more.
            let keep_from = (self.nominal.floor() as i64 - self.seek).min(start + self.hop as i64).max(self.offset);
            let drop = usize::try_from(keep_from - self.offset).unwrap_or(0).saturating_mul(2).min(self.input.len());
            self.input.drain(..drop);
            self.offset += (drop / 2) as i64;
        }
        out
    }

    /// The grain start within `seek` of `nominal` whose first half best matches the input
    /// that naturally follows the previous grain (from `cont`): normalised cross-correlation
    /// of the mono mix, every other frame and every other lag (enough for a smooth join, a
    /// tenth of the work).
    fn best_join(&self, cont: i64, nominal: i64) -> i64 {
        let len = self.hop as i64;
        let mono = |f: i64| {
            let (l, r) = self.at(f);
            l + r
        };
        let mut best = (f32::MIN, nominal);
        let mut lag = -self.seek;
        while lag <= self.seek {
            let c = nominal + lag;
            let (mut xy, mut yy) = (0.0f32, 0.0f32);
            let mut i = 0;
            while i < len {
                let (x, y) = (mono(cont + i), mono(c + i));
                xy += x * y;
                yy += y * y;
                i += 2;
            }
            let score = if yy > 1e-9 { xy / yy.sqrt() } else { 0.0 };
            if score > best.0 {
                best = (score, c);
            }
            lag += 2;
        }
        best.1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(rate: u32, hz: f64, secs: f64) -> Vec<f32> {
        let n = (f64::from(rate) * secs) as usize;
        (0..n)
            .flat_map(|i| {
                let v = (std::f64::consts::TAU * hz * i as f64 / f64::from(rate)).sin() as f32 * 0.5;
                [v, v]
            })
            .collect()
    }

    /// Rising zero crossings per second of the left channel.
    fn pitch(rate: u32, s: &[f32]) -> f64 {
        let left: Vec<f32> = s.chunks(2).map(|c| c[0]).collect();
        let ups = left.windows(2).filter(|w| w[0] < 0.0 && w[1] >= 0.0).count();
        ups as f64 / (left.len() as f64 / f64::from(rate))
    }

    /// Elias, 23 Oct 2026: fast playback's sound at 2×, 4× and 8× keeps its pitch (no chipmunk
    /// voice) and is that many times shorter.
    #[test]
    fn faster_sound_keeps_its_pitch_and_gets_shorter() {
        let rate = 48_000;
        let input = sine(rate, 220.0, 4.0);
        for speed in [2.0, 4.0, 8.0] {
            let mut st = Stretch::new(rate, speed);
            // Fed in blocks, as the feeder does.
            let out: Vec<f32> = input.chunks(2048).flat_map(|b| st.process(b)).collect();
            let secs = out.len() as f64 / 2.0 / f64::from(rate);
            assert!((secs - 4.0 / speed).abs() < 0.1, "{speed}×: {secs:.3} s out of 4 s");
            // Skip the first grain's fade-in.
            let hz = pitch(rate, &out[out.len().min(4800)..]);
            assert!((hz - 220.0).abs() < 6.0, "{speed}×: {hz:.1} Hz, not 220");
        }
    }

    /// Grains joined in phase: a steady tone comes out steady (no beating or dropouts), at the
    /// input's level.
    #[test]
    fn a_steady_tone_stays_steady() {
        let rate = 48_000;
        let mut st = Stretch::new(rate, 2.0);
        let out = st.process(&sine(rate, 440.0, 2.0));
        let body = &out[out.len().min(9600)..];
        for block in body.chunks(2 * 960) {
            let peak = block.iter().fold(0.0f32, |m, v| m.max(v.abs()));
            assert!((0.4..0.6).contains(&peak), "peak {peak} in a 20 ms block");
        }
    }
}
