//! Off-thread MP3 recorder for a QSO: RX in the left channel, TX in the right
//! — or, in mono mode, both time-multiplexed onto a single channel. A stereo
//! recording with no second receiver to fill the right channel is written as
//! dual mono instead of one silent ear; the mixer decides that, this end only
//! ever sees interleaved frames.
//!
//! The engine's audio loop pushes interleaved frames straight off the stereo
//! mixer into a lock-free ring; a dedicated thread drains it, resamples to
//! 48 kHz, and encodes to MP3 with the pure-Rust `shine_rs` encoder, writing
//! to the file. Encoding and file I/O never touch the real-time audio thread.
//!
//! A fixed channel count for the life of the recording (chosen at
//! [`Recorder::start`]), so WFM stereo coming and going, or the sub receiver
//! being switched on, never has to reinitialise the encoder mid-file.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use rtrb::{Consumer, Producer, RingBuffer};
use shine_rs::encoder::{
    ShineConfig, ShineMpeg, ShineWave, shine_close, shine_encode_buffer, shine_flush,
    shine_initialise, shine_samples_per_pass,
};
use tracing::{info, warn};

use sdroxide_dsp::{MonoResampler, StereoResampler};

/// MP3 encode target — a universally-valid MPEG-1 Layer III rate.
const MP3_RATE: i32 = 48_000;
/// Constant bitrate (kbps). Ample for communications audio, and joint stereo
/// spends almost none of it when L and R are identical.
const MP3_BITRATE: i32 = 192;
/// shine channel mode MPG_MD_JOINT_STEREO.
const MODE_JOINT_STEREO: i32 = 1;
/// shine channel mode MPG_MD_MONO.
const MODE_MONO: i32 = 3;

/// How many interleaved channels a recording carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordingChannels {
    /// Two channels: RX in the left, TX in the right — or the same signal in
    /// both, where the caller has nothing to separate (see `StereoMixer`).
    Stereo,
    /// RX and TX time-multiplexed onto a single channel — for RX-only
    /// listening, or anyone who doesn't want split-ear audio.
    Mono,
}

impl RecordingChannels {
    fn count(self) -> usize {
        match self {
            RecordingChannels::Stereo => 2,
            RecordingChannels::Mono => 1,
        }
    }

    fn shine_mode(self) -> i32 {
        match self {
            RecordingChannels::Stereo => MODE_JOINT_STEREO,
            RecordingChannels::Mono => MODE_MONO,
        }
    }
}

/// A running recording. Feed it through the paired [`Producer`] (held by the
/// mixer); drop-finalize by calling [`Recorder::stop`].
pub struct Recorder {
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
    /// The file being written (absolute path).
    pub path: PathBuf,
}

impl Recorder {
    /// Start recording interleaved audio arriving at `in_rate` Hz per channel
    /// to `path`, with `channels` interleaved channels per frame. Returns the
    /// recorder plus the producer the caller feeds frames into. Fails only if
    /// the file can't be created (encoder setup happens on the worker thread).
    pub fn start(
        path: PathBuf,
        in_rate: f64,
        channels: RecordingChannels,
    ) -> std::io::Result<(Recorder, Producer<f32>)> {
        let file = File::create(&path)?;
        // ~4 s of slack so a brief disk stall drops nothing.
        let cap = (in_rate as usize).max(MP3_RATE as usize) * 4 * channels.count();
        let (prod, cons) = RingBuffer::<f32>::new(cap);
        let stop = Arc::new(AtomicBool::new(false));
        let stop_worker = stop.clone();
        let path_worker = path.clone();
        let join = std::thread::Builder::new()
            .name("mp3-recorder".into())
            .spawn(move || encode_loop(cons, file, in_rate, channels, stop_worker, path_worker))
            .expect("spawn recorder thread");
        info!(path = %path.display(), "recording started");
        Ok((Recorder { stop, join: Some(join), path }, prod))
    }

    /// Stop recording: signal the worker, wait for it to flush and close the
    /// file. Blocks briefly (a final encode + flush).
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
        info!(path = %self.path.display(), "recording stopped");
    }
}

fn encode_loop(
    mut cons: Consumer<f32>,
    file: File,
    in_rate: f64,
    channels: RecordingChannels,
    stop: Arc<AtomicBool>,
    path: PathBuf,
) {
    let ch = channels.count();
    let cfg = ShineConfig {
        wave: ShineWave { channels: ch as i32, samplerate: MP3_RATE },
        mpeg: ShineMpeg {
            mode: channels.shine_mode(),
            bitr: MP3_BITRATE,
            emph: 0,
            copyright: 0,
            original: 1,
        },
    };
    let mut enc = match shine_initialise(&cfg) {
        Ok(e) => e,
        Err(e) => {
            warn!(path = %path.display(), "MP3 encoder init failed: {e}; recording aborted");
            return;
        }
    };
    let spp = shine_samples_per_pass(&enc) as usize; // samples per channel per frame

    let mut file = BufWriter::new(file);
    // Exactly one of these is live, matching `ch`; `None` when the rates
    // already match (see `StereoResampler`/`MonoResampler::new`).
    let mut stereo_rs = (ch == 2).then(|| StereoResampler::new(in_rate, MP3_RATE as f64)).flatten();
    let mut mono_rs = (ch == 1).then(|| MonoResampler::new(in_rate, MP3_RATE as f64)).flatten();
    let mut drained: Vec<f32> = Vec::new();
    let mut resampled: Vec<f32> = Vec::new();
    let mut pending: Vec<f32> = Vec::new(); // 48 kHz interleaved, awaiting a frame
    // shine takes one pointer per channel, so the frame is de-interleaved here.
    let mut pcm_l = vec![0i16; spp];
    let mut pcm_r = vec![0i16; spp];

    loop {
        drained.clear();
        while let Ok(s) = cons.pop() {
            drained.push(s);
        }
        if drained.is_empty() {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
            continue;
        }
        match (stereo_rs.as_mut(), mono_rs.as_mut()) {
            (Some(r), _) => {
                resampled.clear();
                r.push(&drained, &mut resampled);
                pending.extend_from_slice(&resampled);
            }
            (None, Some(r)) => {
                resampled.clear();
                r.push(&drained, &mut resampled);
                pending.extend_from_slice(&resampled);
            }
            (None, None) => pending.extend_from_slice(&drained),
        }
        while pending.len() >= spp * ch {
            encode_frame(
                &mut file,
                &mut enc,
                &pending[..spp * ch],
                ch,
                &mut pcm_l,
                &mut pcm_r,
                &path,
            );
            pending.drain(..spp * ch);
        }
    }

    // Final partial frame (zero-padded) so no tail is dropped, then flush.
    if !pending.is_empty() {
        pending.resize(spp * ch, 0.0);
        encode_frame(&mut file, &mut enc, &pending[..spp * ch], ch, &mut pcm_l, &mut pcm_r, &path);
    }
    let (tail, n) = shine_flush(&mut enc);
    if n > 0 {
        let _ = file.write_all(tail);
    }
    let _ = file.flush();
    shine_close(enc);
}

/// De-interleave one frame of f32 samples into per-channel i16 and encode it,
/// writing the MP3 bytes. `pcm_r` is unused (but still sized) when `channels == 1`.
fn encode_frame(
    file: &mut BufWriter<File>,
    enc: &mut shine_rs::ShineGlobalConfig,
    frame: &[f32],
    channels: usize,
    pcm_l: &mut [i16],
    pcm_r: &mut [i16],
    path: &std::path::Path,
) {
    let result = if channels == 2 {
        for (i, lr) in frame.chunks_exact(2).enumerate() {
            pcm_l[i] = (lr[0].clamp(-1.0, 1.0) * 32767.0) as i16;
            pcm_r[i] = (lr[1].clamp(-1.0, 1.0) * 32767.0) as i16;
        }
        shine_encode_buffer(enc, &[pcm_l.as_ptr(), pcm_r.as_ptr()])
    } else {
        for (i, &s) in frame.iter().enumerate() {
            pcm_l[i] = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
        }
        shine_encode_buffer(enc, &[pcm_l.as_ptr()])
    };
    match result {
        Ok((mp3, n)) => {
            if n > 0 {
                if let Err(e) = file.write_all(mp3) {
                    warn!(path = %path.display(), "recording write failed: {e}");
                }
            }
        }
        Err(e) => warn!(path = %path.display(), "MP3 encode failed: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn records_a_valid_mp3() {
        let path =
            std::env::temp_dir().join(format!("sdroxide-rec-test-{}.mp3", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let (rec, mut prod) = Recorder::start(path.clone(), 48_000.0, RecordingChannels::Stereo)
            .expect("start recorder");

        // ~1 s at 48 kHz: 1 kHz in the left ear, 400 Hz in the right, so the
        // two channels are genuinely different, fed as it drains.
        for i in 0..48_000 {
            let t = i as f32 / 48_000.0;
            let l = 0.5 * (std::f32::consts::TAU * 1000.0 * t).sin();
            let r = 0.5 * (std::f32::consts::TAU * 400.0 * t).sin();
            for s in [l, r] {
                while prod.push(s).is_err() {
                    std::thread::sleep(Duration::from_millis(1)); // ring full: let it drain
                }
            }
        }
        std::thread::sleep(Duration::from_millis(100));
        rec.stop(); // flushes + closes the file

        let bytes = std::fs::read(&path).expect("read mp3");
        let _ = std::fs::remove_file(&path);
        assert!(bytes.len() > 2_000, "mp3 suspiciously small: {} bytes", bytes.len());
        // First frame must start with an 11-bit MPEG sync word (0xFFE..).
        assert_eq!(bytes[0], 0xFF, "no MP3 frame sync");
        assert_eq!(bytes[1] & 0xE0, 0xE0, "no MP3 frame sync in byte 1");
    }

    #[test]
    fn records_a_valid_mono_mp3() {
        let path =
            std::env::temp_dir().join(format!("sdroxide-rec-test-mono-{}.mp3", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let (rec, mut prod) = Recorder::start(path.clone(), 48_000.0, RecordingChannels::Mono)
            .expect("start recorder");

        for i in 0..48_000 {
            let t = i as f32 / 48_000.0;
            let s = 0.5 * (std::f32::consts::TAU * 1000.0 * t).sin();
            while prod.push(s).is_err() {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        std::thread::sleep(Duration::from_millis(100));
        rec.stop();

        let bytes = std::fs::read(&path).expect("read mp3");
        let _ = std::fs::remove_file(&path);
        assert!(bytes.len() > 1_000, "mp3 suspiciously small: {} bytes", bytes.len());
        assert_eq!(bytes[0], 0xFF, "no MP3 frame sync");
        assert_eq!(bytes[1] & 0xE0, 0xE0, "no MP3 frame sync in byte 1");
    }
}
