//! Turns an uploaded audio file into the 16 kHz mono `f32` PCM the models take.
//!
//! Decoding is done in-process (symphonia through rodio): WAV, FLAC, MP3 and
//! Ogg Vorbis. Nothing is written to disk and no external tool is launched.

use rodio::{Decoder, Source};
use rubato::audioadapter_buffers::owned::InterleavedOwned;
use rubato::{Fft, FixedSync, Resampler};
use std::io::Cursor;

pub const TARGET_SAMPLE_RATE: u32 = 16_000;

/// Longest recording accepted for one request. Bounds decoded memory
/// (30 min of 16 kHz f32 is about 115 MB) and worst-case inference time.
pub const MAX_AUDIO_SECS: u64 = 30 * 60;

#[derive(Debug, PartialEq)]
pub enum AudioError {
    /// The bytes are not in a supported container/codec.
    Unsupported(String),
    /// Decoded fine but there is no audio in it.
    Empty,
    TooLong,
    Resample(String),
}

impl std::fmt::Display for AudioError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsupported(e) => write!(
                f,
                "Unsupported or corrupt audio file ({e}). Supported formats: wav, flac, mp3, ogg (vorbis)"
            ),
            Self::Empty => f.write_str("The audio file contains no audio"),
            Self::TooLong => write!(
                f,
                "The audio is longer than the {} minute limit",
                MAX_AUDIO_SECS / 60
            ),
            Self::Resample(e) => write!(f, "Could not resample the audio: {e}"),
        }
    }
}

impl std::error::Error for AudioError {}

/// Decode `bytes` and convert to 16 kHz mono f32 samples in [-1, 1].
pub fn decode_to_pcm16k(bytes: Vec<u8>) -> Result<Vec<f32>, AudioError> {
    let decoder =
        Decoder::new(Cursor::new(bytes)).map_err(|e| AudioError::Unsupported(e.to_string()))?;
    let channels = decoder.channels().get() as usize;
    let rate = decoder.sample_rate().get();
    let max_frames = MAX_AUDIO_SECS.saturating_mul(rate as u64) as usize;

    let mut mono: Vec<f32> = Vec::new();
    let mut frame_sum = 0.0f32;
    let mut in_frame = 0usize;
    for sample in decoder {
        frame_sum += sample;
        in_frame += 1;
        if in_frame == channels {
            mono.push((frame_sum / channels as f32).clamp(-1.0, 1.0));
            frame_sum = 0.0;
            in_frame = 0;
            if mono.len() > max_frames {
                return Err(AudioError::TooLong);
            }
        }
    }
    if mono.is_empty() {
        return Err(AudioError::Empty);
    }
    resample_mono(mono, rate)
}

fn resample_mono(mono: Vec<f32>, rate: u32) -> Result<Vec<f32>, AudioError> {
    if rate == TARGET_SAMPLE_RATE {
        return Ok(mono);
    }
    let frames = mono.len();
    let input = InterleavedOwned::new_from(mono, 1, frames)
        .map_err(|e| AudioError::Resample(e.to_string()))?;
    let mut resampler = Fft::<f32>::new(
        rate as usize,
        TARGET_SAMPLE_RATE as usize,
        1024,
        1,
        FixedSync::Input,
    )
    .map_err(|e| AudioError::Resample(e.to_string()))?;
    let needed = resampler.process_all_needed_output_len(frames);
    let mut output = InterleavedOwned::<f32>::new(0.0, 1, needed);
    let (_, produced) = resampler
        .process_all_into_buffer(&input, &mut output, frames, None)
        .map_err(|e| AudioError::Resample(e.to_string()))?;
    let mut pcm = output.take_data();
    pcm.truncate(produced);
    if pcm.is_empty() {
        return Err(AudioError::Empty);
    }
    Ok(pcm)
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// A WAV file holding `secs` of a 440 Hz sine, 16-bit PCM.
    pub fn sine_wav(rate: u32, channels: u16, secs: f32) -> Vec<u8> {
        let spec = hound::WavSpec {
            channels,
            sample_rate: rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut cursor = Cursor::new(Vec::new());
        {
            let mut writer = hound::WavWriter::new(&mut cursor, spec).unwrap();
            let frames = (rate as f32 * secs) as usize;
            for n in 0..frames {
                let t = n as f32 / rate as f32;
                let v = ((t * 440.0 * std::f32::consts::TAU).sin() * 0.5 * i16::MAX as f32) as i16;
                for _ in 0..channels {
                    writer.write_sample(v).unwrap();
                }
            }
            writer.finalize().unwrap();
        }
        cursor.into_inner()
    }

    #[test]
    fn wav_at_16k_passes_through() {
        let pcm = decode_to_pcm16k(sine_wav(16_000, 1, 1.0)).unwrap();
        assert_eq!(pcm.len(), 16_000);
        assert!(pcm.iter().any(|s| s.abs() > 0.4));
    }

    #[test]
    fn stereo_44k_is_downmixed_and_resampled() {
        let pcm = decode_to_pcm16k(sine_wav(44_100, 2, 1.0)).unwrap();
        let expected = 16_000i64;
        assert!(
            (pcm.len() as i64 - expected).abs() < 1_500,
            "got {} samples",
            pcm.len()
        );
        assert!(pcm.iter().any(|s| s.abs() > 0.3));
    }

    #[test]
    fn garbage_is_rejected() {
        assert!(matches!(
            decode_to_pcm16k(b"definitely not audio".to_vec()),
            Err(AudioError::Unsupported(_))
        ));
        assert!(matches!(
            decode_to_pcm16k(Vec::new()),
            Err(AudioError::Unsupported(_))
        ));
    }
}
