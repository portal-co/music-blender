use clap::Parser;
use std::{
    collections::BTreeMap,
    f32::consts::PI,
    fs::OpenOptions,
    io::{Read, Write},
    iter::once,
    mem::replace,
    panic::{catch_unwind, AssertUnwindSafe},
    sync::{Mutex, OnceLock},
};

use fundsp::{
    hacker::{An, Lowpole},
    prelude::{U1, U2, resynth},
    wave::Wave,
};
use rayon::iter::{IntoParallelIterator, ParallelIterator};
use sha3::{
    Sha3_256,
    digest::{FixedOutput, Update},
};

// ── Modes ─────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug)]
enum Mode {
    Standard,
    Div,
    Atan,
    FreqMult,
    FreqDivNorm,
}

const MODES: [Mode; 5] = [
    Mode::Standard,
    Mode::Atan,
    Mode::FreqMult,
    Mode::Div,
    Mode::FreqDivNorm,
];

// ── Parameter types ───────────────────────────────────────────────────────────

/// Parameters for one audio input in a merge operation.
struct InputSpec<'a> {
    wave: &'a (Wave, OnceLock<[u8; 32]>),
    /// Each sample is repeated `x` times before striding.
    x: usize,
    /// Keep every `s`-th sample after repeating.
    s: usize,
    /// "One-minus" amplitude inversion.
    om: bool,
    /// Reverse sample playback order (play the audio backwards).
    rev: bool,
}

struct MergeParams<'a> {
    inputs: Vec<InputSpec<'a>>,
    /// Result repeat factor.
    rx: usize,
    /// Result stride.
    rs: usize,
    mode: Mode,
}

// ── Hashing ───────────────────────────────────────────────────────────────────

impl MergeParams<'_> {
    fn update_hash(&self, h: &mut dyn Update) {
        for (i, inp) in self.inputs.iter().enumerate() {
            let (w, cache) = inp.wave;
            h.update(cache.get_or_init(|| {
                catch_unwind(AssertUnwindSafe(|| {
                    let mut bytes = Vec::new();
                    let _ = w.write_wav16(&mut bytes);
                    let mut sha = Sha3_256::default();
                    sha.update(&bytes);
                    sha.finalize_fixed().into()
                }))
                .unwrap_or([0u8; 32])
            }));
            // Encode (x, s) with a position tag so different input orderings produce
            // different hashes.
            for (j, &v) in [inp.x, inp.s].iter().enumerate() {
                if v != 1 {
                    h.update(&usize::to_ne_bytes(i * 2 + j));
                    h.update(&usize::to_ne_bytes(v));
                }
            }
            if inp.om {
                h.update(&[i as u8, b'o', b'm']);
            }
            if inp.rev {
                h.update(&[i as u8, b'r', b'e', b'v']);
            }
        }
        // Result (rx, rs), tagged to distinguish from per-input params.
        for (j, &v) in [self.rx, self.rs].iter().enumerate() {
            if v != 1 {
                h.update(b"r");
                h.update(&usize::to_ne_bytes(j));
                h.update(&usize::to_ne_bytes(v));
            }
        }
        h.update(match self.mode {
            Mode::Standard => b"std" as &[u8],
            Mode::Atan => b"atan",
            Mode::Div => b"div",
            Mode::FreqMult => b"freqmult",
            Mode::FreqDivNorm => b"freqdivnorm",
        });
    }

    fn compute_hash(&self) -> String {
        let mut s = Sha3_256::default();
        self.update_hash(&mut s);
        hex::encode(sha3::Digest::finalize(s))
    }
}

// ── Sample helpers ────────────────────────────────────────────────────────────

/// Extract amplitude-normalised, om-applied samples for one channel of one input.
fn input_channel_samples(inp: &InputSpec, amp: f32, channel: usize, mode: Mode) -> Vec<f32> {
    let (wave, _) = inp.wave;
    (0..wave.len())
        .map(|p| wave.at(channel, if inp.rev { wave.len() - p - 1 } else { p }) / amp)
        .flat_map(|s| once(s).cycle().take(inp.x))
        .enumerate()
        .filter_map(|(i, s)| if i % inp.s == 0 { Some(s) } else { None })
        .map(|s| {
            if inp.om {
                match mode {
                    Mode::Atan => -s,
                    _ => (1.0 - s.abs()) * s.signum(),
                }
            } else {
                s
            }
        })
        .collect()
}

/// Combine two sample sequences in the frequency domain (FreqMult / FreqDivNorm).
/// For N > 2 inputs this is the binary operation in a left-fold across all inputs.
fn freq_combine_pair(a: Vec<f32>, b: Vec<f32>, mode: Mode, sample_rate: f64) -> Vec<f32> {
    let min_len = a.len().min(b.len());
    let mut tmp = Wave::new(2, sample_rate);
    for i in 0..min_len {
        tmp.push((a[i], b[i]));
    }
    tmp = tmp.filter_latency(
        tmp.duration(),
        &mut resynth::<U2, U1, _>(256, |w| {
            for i in 0..w.bins() {
                w.set(
                    0,
                    i,
                    match mode {
                        Mode::FreqMult => w.at(0, i) * w.at(1, i),
                        Mode::FreqDivNorm => {
                            let x = w.at(1, i);
                            if x.norm() == 0.0 {
                                x
                            } else {
                                let a = w.at(0, i) / x;
                                if a.norm() > 1.0 { a.inv() } else { a }
                            }
                        }
                        _ => unreachable!(),
                    },
                );
            }
        }),
    );
    tmp.normalize();
    (0..tmp.len()).map(|i| tmp.at(0, i)).collect()
}

// ── Core merge ────────────────────────────────────────────────────────────────

fn merge(params: MergeParams) -> Option<Wave> {
    if params.inputs.len() < 2 {
        return None;
    }
    let MergeParams { inputs, rx, rs, mode } = params;

    // All inputs must share channel count and sample rate; reject degenerate 2-sample waves.
    let channels = inputs[0].wave.0.channels();
    let sample_rate = inputs[0].wave.0.sample_rate();
    for inp in &inputs {
        if inp.wave.0.channels() != channels {
            return None;
        }
        if inp.wave.0.sample_rate() != sample_rate {
            return None;
        }
        if inp.wave.0.len() == 2 {
            return None;
        }
    }

    // Pre-check that every input wave has non-zero amplitude.
    let amplitudes: Vec<f32> = inputs.iter().map(|inp| inp.wave.0.amplitude()).collect();
    if amplitudes.iter().any(|&a| a == 0.0) {
        return None;
    }

    // Reject combinations where all (x, s) pairs — including the result's (rx, rs) —
    // are uniformly contracting or uniformly expanding.
    let all_xs: Vec<(usize, usize)> = inputs
        .iter()
        .map(|i| (i.x, i.s))
        .chain(once((rx, rs)))
        .collect();
    if all_xs.iter().all(|&(vx, vs)| vs * 2 > vx) {
        return None;
    }
    if all_xs.iter().all(|&(vx, vs)| vx * 2 > vs) {
        return None;
    }

    // Maximum samples to generate: shortest input length × a bounded scale factor.
    let min_input_len = inputs.iter().map(|i| i.wave.0.len()).min().unwrap_or(0);
    let max_param = inputs
        .iter()
        .flat_map(|i| [i.x, i.s])
        .chain([rx, rs])
        .max()
        .unwrap_or(1)
        .min(3);
    let take_len = min_input_len * max_param;

    let mut new_wave = Wave::new(0, sample_rate);

    'channel: for ch in 0..channels {
        // Sample sequences for every input on this channel.
        let input_seqs: Vec<Vec<f32>> = inputs
            .iter()
            .zip(&amplitudes)
            .map(|(inp, &amp)| input_channel_samples(inp, amp, ch, mode))
            .collect();

        // Combine all input sequences according to the current mode.
        let combined: Vec<f32> = match mode {
            Mode::Standard | Mode::Atan | Mode::Div => {
                let min_len = input_seqs.iter().map(|s| s.len()).min().unwrap_or(0);
                (0..min_len)
                    .map(|i| {
                        let vals: Vec<f32> = input_seqs.iter().map(|s| s[i]).collect();
                        match mode {
                            Mode::Standard => vals.iter().copied().product(),
                            Mode::Atan => {
                                let c: f32 =
                                    vals.iter().map(|&v| (v * PI / 2.0).tan()).sum();
                                if c.is_infinite() || c.is_nan() {
                                    0.0
                                } else {
                                    c.atan() * 2.0 / PI
                                }
                            }
                            Mode::Div => vals
                                .iter()
                                .copied()
                                .fold(None::<f32>, |acc, b| {
                                    Some(match acc {
                                        None => b,
                                        Some(a) => {
                                            if b == 0.0 {
                                                0.0
                                            } else {
                                                let c = a / b;
                                                if c > 1.0 { 1.0 / c } else { c }
                                            }
                                        }
                                    })
                                })
                                .unwrap_or(0.0),
                            _ => unreachable!(),
                        }
                    })
                    .cycle()
                    .flat_map(|s| once(s).cycle().take(rx))
                    .enumerate()
                    .filter_map(|(i, s)| if i % rs == 0 { Some(s) } else { None })
                    .take(take_len)
                    .collect()
            }

            Mode::FreqMult | Mode::FreqDivNorm => {
                // Left-fold all inputs pairwise in the frequency domain.
                let folded = input_seqs
                    .into_iter()
                    .reduce(|a, b| freq_combine_pair(a, b, mode, sample_rate))?;
                folded
                    .into_iter()
                    .cycle()
                    .flat_map(|s| once(s).cycle().take(rx))
                    .enumerate()
                    .filter_map(|(i, s)| if i % rs == 0 { Some(s) } else { None })
                    .take(take_len)
                    .collect()
            }
        };

        // ── Post-processing (identical to the original per-channel pipeline) ──

        let mut samples = combined;
        let mut tmp = Wave::new(0, sample_rate);
        tmp.push_channel(&samples);
        let threshold = 0.05;
        tmp = tmp.filter_latency(tmp.duration(), &mut An(Lowpole::<f32, U1>::new(8000.0f32)));
        for i in 0..tmp.len() {
            let mut v = tmp.at(0, i);
            if v.abs() <= threshold {
                v = 0.0;
            }
            tmp.set(0, i, v);
        }
        if tmp.amplitude() == 0.0 {
            continue 'channel;
        }
        tmp.normalize();
        tmp = tmp.filter_latency(tmp.duration(), &mut An(Lowpole::<f32, U1>::new(8000.0f32)));
        samples = (0..tmp.len()).map(|i| tmp.at(0, i)).collect();

        for _ in 0..(samples.len() / 3) {
            let Some(p) = samples.pop() else { break };
            if p.abs() > threshold {
                samples.push(p);
                break;
            }
        }

        let bytes: Vec<u8> = samples
            .iter()
            .cloned()
            .scan(0.0, |a, b| {
                let c = replace(a, b);
                Some(c - b)
            })
            .scan(0.0, |a, b| {
                let c = replace(a, b);
                Some(c - b)
            })
            .map(|a| (a * 128.0).ceil() as i8 as u8)
            .zip(samples.iter().cloned().map(|a| (a * 128.0).ceil() as i8 as u8))
            .map(|(a, b)| a.wrapping_sub(b))
            .scan(0u8, |a, b| {
                let c = replace(a, b);
                Some(c.wrapping_sub(b))
            })
            .scan(0u8, |a, b| {
                let c = replace(a, b);
                Some(c.wrapping_sub(b))
            })
            .collect();

        if entropy::shannon_entropy(&bytes) > 7.0 {
            continue 'channel;
        }

        loop {
            let Some(p) = samples.pop() else { break };
            if p.abs() > threshold {
                samples.push(p);
                break;
            }
        }

        // Align channel lengths in the accumulating output wave.
        if new_wave.channels() != 0 {
            while new_wave.len() > samples.len() {
                let l = samples.len();
                samples.push(new_wave.at(0, l));
            }
            while samples.len() > new_wave.len() {
                let mut c = new_wave.remove_channel(0);
                let l = c.len();
                c.extend_from_slice(&samples[l..]);
                new_wave.insert_channel(0, &c);
            }
        }
        new_wave.push_channel(&samples);
    }

    if new_wave.channels() == 0 {
        return None;
    }
    new_wave.normalize();
    Some(new_wave)
}

// ── Zip loading ───────────────────────────────────────────────────────────────

/// Recursively load audio files from zip bytes (handles nested zips too).
/// `virtual_base` is used as the key prefix in the `waves` map so each entry
/// gets a unique, human-readable path even though the file never lives on disk.
fn load_from_zip_bytes(
    virtual_base: &std::path::Path,
    bytes: Vec<u8>,
    waves: &mut BTreeMap<std::path::PathBuf, (Wave, OnceLock<[u8; 32]>)>,
) -> std::io::Result<()> {
    let cursor = std::io::Cursor::new(bytes);
    let mut archive = zip::ZipArchive::new(cursor)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        if entry.is_dir() {
            continue;
        }
        let entry_name = entry.name().to_string();
        let virtual_path = virtual_base.join(&entry_name);
        let ext = std::path::Path::new(&entry_name)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        let mut entry_bytes = Vec::new();
        entry.read_to_end(&mut entry_bytes)?;
        drop(entry); // release borrow on archive before potential recursion
        if ext == "zip" {
            let _ = load_from_zip_bytes(&virtual_path, entry_bytes, waves);
        } else {
            let suffix = format!(".{ext}");
            if let Ok(mut tmp) = tempfile::Builder::new().suffix(&suffix).tempfile() {
                if tmp.write_all(&entry_bytes).is_ok() && tmp.flush().is_ok() {
                    if let Ok(w) = Wave::load(tmp.path()) {
                        waves.insert(virtual_path, (w, OnceLock::new()));
                    }
                }
                // tmp dropped here → temp file deleted
            }
        }
    }
    Ok(())
}

// ── Entry point ───────────────────────────────────────────────────────────────

fn main() -> Result<(), std::io::Error> {
    let mut waves: BTreeMap<std::path::PathBuf, (Wave, OnceLock<[u8; 32]>)> = BTreeMap::new();

    #[derive(Parser)]
    struct Opt {
        /// Output directory
        #[arg(short, long)]
        out: String,

        /// Maximum output size in MB
        #[arg(short, long)]
        max_size: Option<usize>,

        /// Hash prefix filter
        #[arg(long, default_value = "")]
        pow: String,

        /// Minimum number of audio inputs per merge (≥ 2)
        #[arg(long, default_value_t = 2)]
        min_inputs: usize,

        /// Maximum number of audio inputs per merge
        #[arg(long, default_value_t = 2)]
        max_inputs: usize,

        /// Input paths (files or directories)
        #[arg(value_name = "INPUT")]
        inputs: Vec<std::path::PathBuf>,
    }

    let opts = Opt::parse();
    let out = opts.out;
    let pow = opts.pow;
    let min_inputs = opts.min_inputs.max(2);
    let max_inputs = opts.max_inputs.max(min_inputs);

    // ── Load waves ────────────────────────────────────────────────────────────

    for input in opts.inputs.iter() {
        for entry in walkdir::WalkDir::new(input) {
            let entry = entry?;
            if entry.file_type().is_file() {
                let path = entry.into_path();
                let ext = path
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("")
                    .to_ascii_lowercase();
                if ext == "zip" {
                    if let Ok(bytes) = std::fs::read(&path) {
                        let _ = load_from_zip_bytes(&path, bytes, &mut waves);
                    }
                } else if let Ok(w) = Wave::load(&path) {
                    waves.insert(path, (w, OnceLock::new()));
                }
            }
        }
    }

    // ── Build search space ────────────────────────────────────────────────────

    let size = opts.max_size.map(|a| Mutex::new(a * 1024 * 1024));

    // (x, s) combinations: repeat × stride, excluding identical non-unity pairs.
    let xsi: Vec<(usize, usize)> = [1usize, 2, 3, 5]
        .into_iter()
        .flat_map(|a| {
            [1, 2, 3, 5]
                .into_iter()
                .filter(move |b| *b != a || *b == 1)
                .map(move |b| (a, b))
        })
        .collect();

    // All possible per-wave-slot parameter combinations: (wave_ref, x, s, om, rev).
    // This flattens the wave × xsi × {om,rev} product into a single indexed list so
    // we can address any combination with a single u64 index below.
    let per_wave: Vec<(&(Wave, OnceLock<[u8; 32]>), usize, usize, bool, bool)> = waves
        .values()
        .flat_map(|w| {
            xsi.iter().flat_map(move |&(x, s)| {
                [(false, false), (false, true), (true, false), (true, true)]
                    .into_iter()
                    .map(move |(om, rev)| (w, x, s, om, rev))
            })
        })
        .collect();

    // All possible shared parameter combinations: (rx, rs, mode).
    let shared: Vec<(usize, usize, Mode)> = xsi
        .iter()
        .flat_map(|&(rx, rs)| MODES.iter().map(move |&mode| (rx, rs, mode)))
        .collect();

    let nk = per_wave.len() as u64; // per-slot choice count
    let ns = shared.len() as u64;   // shared-param choice count

    // ── Iterate over all n-input merges ───────────────────────────────────────

    for n in min_inputs..=max_inputs {
        // nk^n is the number of ordered n-tuples of per-wave slots.
        // Skip this n if the count overflows u64 (would only happen for enormous
        // wave libraries combined with very large n).
        let Some(wave_combos) = nk.checked_pow(n as u32) else {
            break;
        };
        let Some(total) = wave_combos.checked_mul(ns) else {
            break;
        };

        (0..total)
            .into_par_iter()
            .map(|idx| {
                // Decode the flat index into shared params + n per-wave slots.
                // Layout: idx = wave_combo_idx + shared_idx * wave_combos
                let si = (idx / wave_combos) as usize;
                let wci = idx % wave_combos;
                let (rx, rs, mode) = shared[si];

                // Decode wci as an n-digit number in base nk (little-endian digits).
                // Digit i selects the per-wave-slot for input i.
                let inputs: Vec<InputSpec> = (0..n)
                    .map(|i| {
                        // nk^i fits in u64 because nk^n fits (checked above) and i < n.
                        let slot_i =
                            ((wci / nk.saturating_pow(i as u32)) % nk) as usize;
                        let (wave, x, s, om, rev) = per_wave[slot_i];
                        InputSpec { wave, x, s, om, rev }
                    })
                    .collect();

                // Record max input duration before inputs are moved into MergeParams.
                let max_input_duration: f64 = inputs
                    .iter()
                    .map(|inp| inp.wave.0.duration())
                    .fold(f64::NEG_INFINITY, f64::max);

                let params = MergeParams { inputs, rx, rs, mode };
                let h = params.compute_hash();
                if !h.starts_with(&pow) {
                    return Ok(());
                }

                let dir1 = format!("{out}/{}", &h[..4]);
                if !std::fs::exists(&dir1)? {
                    std::fs::create_dir(&dir1)?;
                }
                let path = format!("{dir1}/{h}.wav");
                if std::fs::exists(&path)? {
                    return Ok(());
                }

                if let Some(c) = merge(params) {
                    let mut f = OpenOptions::new()
                        .create(true)
                        .write(true)
                        .truncate(true)
                        .open(&path)?;
                    // Use 16-bit when the output is substantially longer than any
                    // single input (the extra resolution is lost in the stretching
                    // anyway); otherwise keep 32-bit for short outputs.
                    if c.duration() > max_input_duration * 1.4 {
                        c.write_wav16(&mut f)?;
                    } else {
                        c.write_wav32(&mut f)?;
                    }
                    if let Some(sz) = &size {
                        let mut sz = sz.lock().unwrap();
                        *sz = sz.saturating_sub(f.metadata()?.len() as usize);
                        if *sz == 0 {
                            std::process::exit(0);
                        }
                    }
                    println!("{path}");
                }

                Ok::<_, std::io::Error>(())
            })
            .collect::<Result<(), std::io::Error>>()?;
    }

    Ok(())
}
