use std::{collections::BTreeMap, fs::OpenOptions, iter::once, mem::replace};

use fundsp::wave::Wave;
use rayon::iter::{IntoParallelRefIterator, ParallelIterator};
use sha3::{Digest, Sha3_256};

fn merge(
    a: &Wave,
    ax: usize,
    as_: usize,
    b: &Wave,
    bx: usize,
    bs_: usize,
    rx: usize,
    rs: usize,
) -> Option<Wave> {
    if a.channels() != b.channels() {
        return None;
    };
    if a.sample_rate() != b.sample_rate() {
        return None;
    };
    let mut new = Wave::new(0, a.sample_rate());
    for x in 0..a.channels() {
        let samples = (0..a.len())
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
            .map(|(a, b)| a * b)
            .cycle()
            .flat_map(|a| once(a).cycle().take(rx))
            .enumerate()
            .filter_map(|(a, b)| if a % rs == 0 { Some(b) } else { None })
            .take(a.len().min(b.len()) * ax.max(as_).max(bx).max(bs_).max(rx).max(rs))
            .collect::<Vec<_>>();
        new.push_channel(&samples);
    }
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
    let xsi = [1usize, 2, 3]
        .into_iter()
        .flat_map(|a| {
            [1, 2, 3]
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
        .map(|(((((ap, a), (bp, b)), (rx, rs)), (bx, bs_)), (ax, as_))| {
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
                s.finalize()
            });
            if !h.starts_with(&pow) {
                return Ok(());
            }
            let path = format!("{out}/{}", &h[..2]);
            if !std::fs::exists(&path)? {
                std::fs::create_dir(path)?;
            }
            let path = format!("{out}/{}/{h}.wav", &h[..2]);
            if std::fs::exists(&path)? {
                return Ok(());
            }
            if let Some(c) = merge(a, ax, as_, b, bx, bs_, rx, rs) {
                let mut f = OpenOptions::new()
                    .create(true)
                    .write(true)
                    .truncate(true)
                    .open(&path)?;
                c.write_wav32(&mut f)?;
                println!("{path}");
            }

            Ok::<_, std::io::Error>(())
        })
        .collect::<Result<(), std::io::Error>>()?;
    Ok(())
}
