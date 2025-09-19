use std::{collections::BTreeMap, fs::OpenOptions};

use fundsp::wave::Wave;
use rayon::iter::{IntoParallelRefIterator, ParallelIterator};
use sha3::{Digest, Sha3_256};

fn merge(a: &Wave, b: &Wave) -> Option<Wave> {
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
            .zip((0..b.len()).map(|p| b.at(x, p)))
            .map(|(a, b)| a * b)
            .cycle()
            .take(a.len().min(b.len()))
            .collect::<Vec<_>>();
        new.push_channel(&samples);
    }
    return Some(new);
}
fn main() -> Result<(), std::io::Error> {
    let mut waves = BTreeMap::new();
    let mut args = std::env::args();
    args.next();
    let out = args.next().unwrap();
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
    waves
        .par_iter()
        .flat_map(|a| waves.par_iter().map(move |b| (a, b)))
        .map(|((ap, a), (bp, b))| {
            if let Some(c) = merge(a, b) {
                let mut f = OpenOptions::new()
                    .create(true)
                    .write(true)
                    .truncate(true)
                    .open(format!(
                        "{out}/{}.wav",
                        hex::encode({
                            let mut s = Sha3_256::default();
                            s.update(ap.as_os_str().as_encoded_bytes());
                            s.update(bp.as_os_str().as_encoded_bytes());
                            s.finalize()
                        })
                    ))?;
                c.write_wav32(&mut f)?;
            }
            Ok::<_, std::io::Error>(())
        })
        .collect::<Result<(), std::io::Error>>()?;
    Ok(())
}
