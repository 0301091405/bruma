//! The one sound Bruma makes: a soft shutter, for a capture taken while a
//! game fills the screen and nothing else can be shown over it.

/// Samples a second.
const RATE: u32 = 44_100;

/// A short shutter sound as a complete WAV file (16-bit mono PCM): two
/// clicks of filtered noise, the second softer, each dying away in a few
/// milliseconds. Made in code so that no sound file has to be shipped.
pub fn shutter() -> Vec<u8> {
    let length = RATE as usize * 140 / 1000;
    let mut seed: u32 = 0x2545_F491;
    let mut noise = move || {
        // xorshift: the same sound every time.
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        seed as f32 / u32::MAX as f32 * 2.0 - 1.0
    };
    let click = |t: f32, start: f32, loudness: f32| {
        if t < start {
            0.0
        } else {
            loudness * (-(t - start) / 0.006).exp()
        }
    };
    let mut low = 0.0f32;
    let samples: Vec<i16> = (0..length)
        .map(|i| {
            let t = i as f32 / RATE as f32;
            // Noise through a gentle low-pass, so it clicks without hissing.
            low += 0.35 * (noise() - low);
            let envelope = click(t, 0.0, 0.55) + click(t, 0.055, 0.3);
            (low * envelope * i16::MAX as f32) as i16
        })
        .collect();
    let data = samples.len() as u32 * 2;
    let mut wav = Vec::with_capacity(44 + data as usize);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&1u16.to_le_bytes()); // mono
    wav.extend_from_slice(&RATE.to_le_bytes());
    wav.extend_from_slice(&(RATE * 2).to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data.to_le_bytes());
    for s in samples {
        wav.extend_from_slice(&s.to_le_bytes());
    }
    wav
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shutter_is_a_short_quiet_wav_that_ends_in_silence() {
        let wav = shutter();
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..16], b"WAVEfmt ");
        assert_eq!(
            u32::from_le_bytes(wav[4..8].try_into().unwrap()) as usize,
            wav.len() - 8
        );
        let samples: Vec<i16> = wav[44..]
            .chunks_exact(2)
            .map(|b| i16::from_le_bytes([b[0], b[1]]))
            .collect();
        let seconds = samples.len() as f32 / RATE as f32;
        assert!((0.1..0.2).contains(&seconds), "{seconds} s");
        let loudest = samples.iter().map(|s| s.unsigned_abs()).max().unwrap();
        assert!(loudest > 2_000 && loudest < 20_000, "{loudest}");
        let tail = samples[samples.len() - 200..]
            .iter()
            .map(|s| s.unsigned_abs())
            .max()
            .unwrap();
        assert!(tail < 50, "it fades out: {tail}");
        assert_eq!(shutter(), wav, "the same every time");
    }
}
