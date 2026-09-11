//! GATE 0: render the engine-decision passage through the ONNX Kokoro path and
//! report the real-time factor, so the new engine can be listened to against the
//! PyTorch render Fernando already accepted.
use std::path::PathBuf;
use std::time::Instant;

use narrator::tts::{g2p::Phonemizer, kokoro};

#[derive(serde::Deserialize)]
struct Chunk {
    text: String,
    #[allow(dead_code)]
    para: usize,
    silent: bool,
}

fn main() -> anyhow::Result<()> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let chunks: Vec<Chunk> =
        serde_json::from_slice(&std::fs::read(root.join("listen-test/chunks.json"))?)?;
    let voice = std::env::var("KOKORO_VOICE").unwrap_or_else(|_| "af_heart".into());
    let g2p = Phonemizer::default();
    eprintln!("espeak: {}", g2p.probe()?);
    let t_load = Instant::now();
    let k = kokoro::Kokoro::load(
        &root.join("models/kokoro/model.onnx"),
        &root.join("models/kokoro/voices"),
        &voice,
        1.0,
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4),
    )?;
    eprintln!("model loaded in {:.1}s", t_load.elapsed().as_secs_f32());

    let gap = kokoro::silence(0.35);
    let mut out: Vec<f32> = Vec::new();
    let mut synth_s = 0.0f64;
    let mut audio_s = 0.0f64;
    for (i, c) in chunks.iter().enumerate() {
        if c.silent {
            continue;
        }
        let t0 = Instant::now();
        let ph = g2p.phonemize(&c.text)?;
        let wav = k.synth(&ph)?;
        let dt = t0.elapsed().as_secs_f64();
        synth_s += dt;
        audio_s += wav.len() as f64 / kokoro::SR as f64;
        eprintln!(
            "{i:3}  {:5.2}s audio  {:5.2}s cpu  rtf {:5.2}  {} phon  {:?}",
            wav.len() as f64 / kokoro::SR as f64,
            dt,
            (wav.len() as f64 / kokoro::SR as f64) / dt.max(1e-9),
            ph.chars().count(),
            c.text.chars().take(48).collect::<String>()
        );
        if !out.is_empty() {
            out.extend_from_slice(&gap);
        }
        out.extend_from_slice(&wav);
    }
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: kokoro::SR,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let path = root.join("listen-test/kokoro_onnx_af_heart.wav");
    let mut w = hound::WavWriter::create(&path, spec)?;
    for s in kokoro::to_i16(&out) {
        w.write_sample(s)?;
    }
    w.finalize()?;
    println!("\nwrote {}", path.display());
    println!(
        "chunks:        {}",
        chunks.iter().filter(|c| !c.silent).count()
    );
    println!(
        "audio:         {audio_s:.2}s (file {:.2}s with gaps)",
        out.len() as f64 / kokoro::SR as f64
    );
    println!("synthesis cpu: {synth_s:.2}s");
    println!("RTF:           {:.2}x realtime", audio_s / synth_s);
    Ok(())
}
