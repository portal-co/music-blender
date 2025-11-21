use std::{collections::BTreeMap, f32::consts::PI, fs::OpenOptions, iter::once, mem::replace};

use fundsp::{
    hacker::{An, Lowpole, Pinkpass},
    prelude::{U1, U2, resynth},
    wave::Wave,
};
use itertools::Itertools;
use rayon::iter::{IntoParallelRefIterator, ParallelIterator};
use sha3::{Digest, Sha3_256};
#[derive(Clone, Copy)]
enum Mode {
    Standard,
    Div,
    Atan,
    FreqMult,
    FreqDivNorm,
}
fn merge(
    a: &Wave,
    ax: usize,
    as_: usize,
    b: &Wave,
    bx: usize,
    bs_: usize,
    rx: usize,
    rs: usize,
    // seed: u8,
    mode: Mode,
    aom: bool,
    bom: bool,
) -> Option<Wave> {
    if a.channels() != b.channels() {
        return None;
    };
    if a.sample_rate() != b.sample_rate() {
        return None;
    };
    let mut new = Wave::new(0, a.sample_rate());
    let aamp = a.amplitude();
    let aamp = if aamp == 0.0 {
        return None;
    } else {
        aamp
    };
    let bamp = a.amplitude();
    let bamp = if bamp == 0.0 {
        return None;
    } else {
        bamp
    };
    let ar = [(ax, as_), (bx, bs_), (rx, rs)];
    if ar
        .into_iter()
        .array_combinations()
        .filter(|[(vx, vs), (vx2, vs2)]| *vs * 2 > *vx && *vs2 * 2 > *vx2)
        .count()
        >= 3
    {
        return None;
    }
    if ar
        .into_iter()
        .array_combinations()
        .filter(|[(vx, vs), (vx2, vs2)]| *vx * 2 > *vs && *vx2 * 2 > *vs2)
        .count()
        >= 3
    {
        return None;
    }

    // if ar.into_iter().filter(|(vx, vs)| *vs * 3 > *vx * 2).count() >= 3 {
    //     return None;
    // }
    // if ar.into_iter().filter(|(vx, vs)| *vx * 3 > *vs * 2).count() >= 3 {
    //     return None;
    // }
    for x in 0..a.channels() {
        let zips = (0..a.len())
            .map(|p| a.at(x, p))
            .flat_map(|a| once(a).cycle().take(ax))
            .enumerate()
            .filter_map(|(a, b)| if a % as_ == 0 { Some(b) } else { None })
            .zip(
                (0..b.len())
                    .map(|p| b.at(x, p))
                    .flat_map(|b| once(b).cycle().take(bx))
                    .enumerate()
                    .filter_map(|(a, b)| if a % bs_ == 0 { Some(b) } else { None }),
            )
            .map(|(as_, bs)| (as_ / aamp, bs / bamp))
            .map(|(a, b)| {
                (
                    if aom && a != 0.0 {
                        match mode {
                            Mode::Atan => -a,
                            _ => (1.0 - a.abs()) * a.signum(),
                        }
                    } else {
                        a
                    },
                    if bom && b != 0.0 {
                        match mode {
                            Mode::Atan => -b,
                            _ => (1.0 - b.abs()) * b.signum(),
                        }
                    } else {
                        b
                    },
                )
            });
        let mut samples = match mode {
            Mode::Standard | Mode::Div | Mode::Atan => {
                let samples = zips
                    .map(|(a, b)| match mode {
                        Mode::Standard => a * b,
                        Mode::Atan => {
                            let a = (a * PI / 2.0).tan();
                            let b = (b * PI / 2.0).tan();
                            let c = (a + b);
                            if c.is_infinite() || c.is_nan() {
                                return 0.0;
                            }
                            (c.atan()) / PI * 2.0
                        }
                        Mode::Div => {
                            if b == 0.0 {
                                0.0
                            } else {
                                let c = a / b;
                                if c > 1.0 { 1.0 / c } else { c }
                            }
                        }
                        _ => unreachable!(),
                    })
                    .cycle()
                    .flat_map(|a| once(a).cycle().take(rx))
                    .enumerate()
                    .filter_map(|(a, b)| if a % rs == 0 { Some(b) } else { None })
                    .take(
                        a.len().min(b.len()) * ax.max(as_).max(bx).max(bs_).max(rx).max(rs).min(3),
                    )
                    .collect::<Vec<_>>();
                samples
            }
            Mode::FreqMult | Mode::FreqDivNorm => {
                let mut tmp = Wave::new(2, a.sample_rate());
                for z in zips {
                    tmp.push(z);
                }
                match mode {
                    Mode::FreqMult | Mode::FreqDivNorm => {
                        tmp = tmp.filter_latency(
                            tmp.duration(),
                            &mut resynth::<U2, U1, _>(256, |w| {
                                for i in 0..w.bins() {
                                    w.set(
                                        0,
                                        i,
                                        match mode {
                                            Mode::FreqMult => w.at(0, i) * w.at(1, i),
                                            Mode::FreqDivNorm => match w.at(1, i) {
                                                x => {
                                                    if x.norm() == 0.0 {
                                                        x
                                                    } else {
                                                        match w.at(0, i) / x {
                                                            a => {
                                                                if a.norm() > 1.0 {
                                                                    a.inv()
                                                                } else {
                                                                    a
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                            },
                                            _ => unreachable!(),
                                        },
                                    );
                                }
                            }),
                        )
                    }
                    _ => unreachable!(),
                };
                tmp.normalize();
                let samples = (0..tmp.len())
                    .map(|a| tmp.at(0, a))
                    .cycle()
                    .flat_map(|a| once(a).cycle().take(rx))
                    .enumerate()
                    .filter_map(|(a, b)| if a % rs == 0 { Some(b) } else { None })
                    .take(
                        a.len().min(b.len()) * ax.max(as_).max(bx).max(bs_).max(rx).max(rs).min(3),
                    )
                    .collect::<Vec<_>>();
                samples
            }
        };
        let mut tmp = Wave::new(0, a.sample_rate());
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
            continue;
        }
        tmp.normalize();
        tmp = tmp.filter_latency(tmp.duration(), &mut An(Lowpole::<f32, U1>::new(8000.0f32)));
        samples = (0..tmp.len()).map(|a| tmp.at(0, a)).collect();

        for _ in 0..(samples.len() / 3) {
            let Some(p) = samples.pop() else {
                break;
            };
            if p.abs() > threshold {
                samples.push(p);
                break;
            }
        }
        let mut bytes = samples
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
            .zip(
                samples
                    .iter()
                    .cloned()
                    .map(|a| (a * 128.0).ceil() as i8 as u8),
            )
            .map(|(a, b)| a.wrapping_sub(b))
            .scan(0u8, |a, b| {
                let c = replace(a, b);
                Some(c.wrapping_sub(b))
            })
            .scan(0u8, |a, b| {
                let c = replace(a, b);
                Some(c.wrapping_sub(b))
            })
            .collect_vec();
        let ent = entropy::shannon_entropy(&bytes);
        if ent > 7.0 {
            continue;
        }
        loop {
            let Some(p) = samples.pop() else {
                break;
            };
            if p.abs() > threshold {
                samples.push(p);
                break;
            }
        }
        if new.channels() != 0 {
            while new.len() > samples.len() {
                let l = samples.len();
                samples.push(new.at(0, l));
            }
            while samples.len() > new.len() {
                let mut c = new.remove_channel(0);
                let l = c.len();
                c.extend_from_slice(&samples[l..]);
                new.insert_channel(0, &c);
            }
        }
        new.push_channel(&samples);
    }
    if new.channels() == 0 {
        return None;
    }
    new.normalize();

    return Some(new);
}
fn main() -> Result<(), std::io::Error> {
    let mut waves = BTreeMap::new();
    let mut args = std::env::args();
    args.next();
    let mut out = args.next().unwrap();
    let mut pow = String::default();
    if out == "-pow" {
        out = args.next().unwrap();
        pow = replace(&mut out, args.next().unwrap());
    }
    for a in args {
        for a in walkdir::WalkDir::new(a) {
            let a = a?;
            if a.file_type().is_file() {
                if let Ok(w) = Wave::load(a.path()) {
                    waves.insert(a.into_path(), w);
                }
            }
        }
    }
    let xsi = [1usize, 2, 3, 5]
        .into_iter()
        .flat_map(|a| {
            [1, 2, 3, 5]
                .into_iter()
                .filter(move |b| *b != a || *b == 1)
                .map(move |b| (a, b))
        })
        .collect::<Vec<_>>();
    waves
        .par_iter()
        .flat_map(|a| waves.par_iter().map(move |b| (a, b)))
        .filter(|((ap, a), (bp, b))| {
            if a.channels() != b.channels() {
                return false;
            };
            if a.sample_rate() != b.sample_rate() {
                return false;
            };
            return true;
        })
        .flat_map(|a| xsi.par_iter().cloned().map(move |b| (a, b)))
        .flat_map(|a| xsi.par_iter().cloned().map(move |b| (a, b)))
        .flat_map(|a| xsi.par_iter().cloned().map(move |b| (a, b)))
        .flat_map_iter(|a| {
            [
                Mode::Standard,
                Mode::Atan,
                Mode::FreqMult,
                Mode::Div,
                Mode::FreqDivNorm,
            ]
            .map(move |b| (a, b))
        })
        .flat_map_iter(|a| {
            [true, false]
                .into_iter()
                .cartesian_product([true, false])
                .map(move |b| (a, b))
        })
        .map(
            |(((((((ap, a), (bp, b)), (rx, rs)), (bx, bs_)), (ax, as_)), mode), (aom, bom))| {
                let h = hex::encode({
                    let mut s = Sha3_256::default();
                    s.update(ap.as_os_str().as_encoded_bytes());
                    s.update(bp.as_os_str().as_encoded_bytes());
                    for (v, w) in [ax, as_, bx, bs_, rx, rs].into_iter().enumerate() {
                        if w != 1 {
                            s.update(usize::to_ne_bytes(v));
                            s.update(usize::to_ne_bytes(w));
                        }
                    }
                    if let Mode::Atan = mode {
                        s.update("atan");
                    }
                    if let Mode::Div = mode {
                        s.update("div");
                    }
                    if let Mode::FreqMult = mode {
                        s.update("freqmult");
                    }
                    if let Mode::FreqDivNorm = mode {
                        s.update("freqdivnorm");
                    }
                    if aom {
                        s.update("aom");
                    }
                    if bom {
                        s.update("bom");
                    }
                    s.finalize()
                });
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
                if let Some(c) = merge(a, ax, as_, b, bx, bs_, rx, rs, mode, aom, bom) {
                    let mut f = OpenOptions::new()
                        .create(true)
                        .write(true)
                        .truncate(true)
                        .open(&path)?;
                    if c.duration() > a.duration() * 1.4 && c.duration() > b.duration() * 1.4 {
                        c.write_wav16(&mut f)?;
                    } else {
                        c.write_wav32(&mut f)?;
                    }
                    println!("{path}");
                }

                Ok::<_, std::io::Error>(())
            },
        )
        .collect::<Result<(), std::io::Error>>()?;
    Ok(())
}
