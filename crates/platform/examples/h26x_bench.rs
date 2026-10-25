//! Times the VideoToolbox H.264 / HEVC decoder against FilmCraft's software decoder on a movie:
//! decode speed, and the output delay (samples fed before the first picture comes out), which is
//! what a seek pays on top of decoding from the sync sample.
//!
//! `cargo run --profile local -p effectcraft-platform --example h26x_bench -- <file.mp4> [frames]`

#[cfg(target_os = "macos")]
fn main() {
    use effectcraft_platform::h26x::VtNalDecoder;
    use effectcraft_platform::nal::select;
    use filmcraft_codecs::VideoDecoder;
    use std::time::Instant;

    let args: Vec<String> = std::env::args().collect();
    let path = args.get(1).expect("usage: h26x_bench <file.mp4> [frames]");
    let frames: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(60);
    let file = std::fs::File::open(path).expect("open");
    let mp4 = filmcraft_isobmff::open(&file).expect("parse");
    let ti = mp4.track_of_kind(filmcraft_isobmff::TrackKind::Video).expect("video track");
    let track = &mp4.tracks[ti];
    let entry = &track.entries[0];
    let n = frames.min(track.samples.len());
    let samples: Vec<(Vec<u8>, i64)> = (0..n).map(|i| (mp4.read_sample(&file, ti, i).expect("sample"), track.samples[i].pts)).collect();
    let stream = match select(entry, true, |_| true) {
        Ok(s) => s,
        Err(why) => {
            println!("not taken by the hardware path: {why:?}");
            return;
        }
    };
    println!("{:?} {:?} reorder depth {} | {} samples", stream.codec, stream.layout.crop, stream.reorder, n);
    let run = |name: &str, d: &mut dyn VideoDecoder| {
        let t0 = Instant::now();
        let (mut first, mut out) = (None, 0usize);
        for (k, (s, pts)) in samples.iter().enumerate() {
            let got = d.decode(s, *pts).expect("decode").len();
            if got > 0 && first.is_none() {
                first = Some((k + 1, t0.elapsed().as_secs_f64() * 1e3));
            }
            out += got;
        }
        out += d.flush().len();
        let ms = t0.elapsed().as_secs_f64() * 1e3;
        let (fed, at) = first.unwrap_or((n, ms));
        println!("{name:>9}: {:.2} ms/frame, {out} pictures; first picture after {fed} samples ({at:.1} ms)", ms / n.max(1) as f64);
    };
    match VtNalDecoder::new(stream.clone()) {
        Ok(mut hw) => run("hardware", &mut hw),
        Err(why) => println!(" hardware: no session ({why})"),
    }
    run("software", stream.software_decoder().expect("software").as_mut());
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("VideoToolbox is macOS only");
}
