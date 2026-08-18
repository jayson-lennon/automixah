//! Terminal auto-DJ: analyze tracks, plan the session, render the mix,
//! write a WAV. The offline render *is* the interface at this stage.
#![allow(
    clippy::cast_precision_loss,
    reason = "CLI logging; precision loss irrelevant"
)]

use std::path::{Path, PathBuf};

use clap::Parser;
use error_stack::{Report, ResultExt};
use wherror::Error;

use automixah_engine::render::renderer::{Renderer, TrackProvider};
use automixah_engine::timeline::plan::{PlanOptions, plan_with};
use automixah_engine::timeline::types::{SessionTime, TrackAnalysis, TrackHash, TransitionPlan};
use djcore::analyzer::{AnalyzerOutput, AudioAnalyzer, StratumAnalyzer};
use djcore::decoder::{DecodeAudio, DecoderRegistry};

/// Errors surfaced by the CLI pipeline.
#[derive(Debug, Error)]
#[error("automixah cli error")]
pub struct CliError;

/// How the session maps track tempos.
#[derive(Debug, Clone, Copy, clap::ValueEnum, PartialEq)]
pub enum TempoStrategyArg {
    /// Constant stretch to the session BPM (default).
    Session,
    /// Pairwise drift-back on every segment.
    Driftback,
}

/// Command-line configuration.
#[derive(Debug, Parser)]
#[command(name = "automixah", about = "analyze, plan, mix, write WAV")]
pub struct Config {
    /// Track file, in playlist order. Repeat the flag.
    #[arg(short, long = "track")]
    pub tracks: Vec<PathBuf>,

    /// Output WAV path.
    #[arg(short, long, default_value = "mix.wav")]
    pub out: PathBuf,

    /// Target session BPM (default: median of folded track BPMs).
    #[arg(long)]
    pub target_bpm: Option<f32>,

    /// Tempo strategy.
    #[arg(long, value_enum, default_value = "session")]
    pub tempo_strategy: TempoStrategyArg,

    /// RON file with a custom automation pair (TransitionSpec)
    /// applied to every transition.
    #[arg(long)]
    pub automation: Option<PathBuf>,
}

/// One decoded+analyzed track with its source PCM.
struct LoadedTrack {
    analysis: TrackAnalysis,
    pcm: Vec<f32>,
}

/// Plans the session for `config` without loading/analyzing files
/// (test helper: exposes what `run` will hand the renderer).
pub fn plan_only(
    config: &Config,
    analyses: &[TrackAnalysis],
) -> automixah_engine::timeline::SessionPlan {
    let transition = automixah_engine::automation::transition_spec::default_pair();
    plan_session_for(analyses, config, &transition)
}

/// Parses argv into a [`Config`] (binary entry helper).
pub fn cli_config() -> Config {
    Config::parse()
}

/// Parses argv into a [`Config`], exiting on CLI errors.
pub fn parse_args() -> Config {
    Config::parse()
}

/// Runs the full pipeline: load → analyze → plan → render → write.
///
/// # Errors
///
/// Returns an error if any track cannot be read/decoded/analyzed,
/// or the WAV cannot be written.
pub fn run(config: &Config) -> Result<SessionTime, Report<CliError>> {
    if config.tracks.is_empty() {
        return Err(Report::new(CliError).attach("no --track flags given"));
    }

    let loaded = load_all(&config.tracks)?;
    let analyses: Vec<TrackAnalysis> = loaded.iter().map(|t| t.analysis.clone()).collect();

    let transition = load_transition(config)?;
    let plan = plan_session_for(&analyses, config, &transition);
    log_plan(&plan, &analyses, &transition);

    let mut provider = SessionPcm::new(&loaded, &plan);
    let total = SessionTime(plan.total_len_samples());
    let mut renderer = Renderer::with_transition(plan, transition.clone());
    let mix = renderer
        .render_until(&mut provider, total)
        .change_context(CliError)
        .attach("session render failed")?;

    write_wav(&config.out, &mix, 44_100).change_context(CliError)?;

    eprintln!(
        "[mix] wrote {} samples ({:.1}s) to {}",
        mix.len(),
        mix.len() as f64 / 44_100.0,
        config.out.display()
    );
    Ok(total)
}

/// Loads, hashes, decodes, and analyzes each path in order (deduped).
fn load_all(paths: &[PathBuf]) -> Result<Vec<LoadedTrack>, Report<CliError>> {
    let registry = DecoderRegistry::with_symphonia();
    let analyzer = StratumAnalyzer::new();
    let mut loaded: Vec<LoadedTrack> = Vec::new();
    let mut seen: Vec<String> = Vec::new();

    for (i, path) in paths.iter().enumerate() {
        let name = path.display().to_string();
        eprintln!("[load {}/{}] {}", i + 1, paths.len(), name);

        let hash = hash_of(path)?;
        if seen.contains(&hash) {
            eprintln!("  duplicate of an earlier --track, skipped");
            continue;
        }
        seen.push(hash.clone());

        let decoded = decode_of(path, &registry)?;
        eprintln!(
            "  decoded: {}s @ {} Hz ({} samples)",
            decoded.frames() / decoded.sample_rate as usize,
            decoded.sample_rate,
            decoded.frames()
        );

        let out = analyzer
            .analyze(&decoded.to_mono(), decoded.sample_rate)
            .change_context(CliError)
            .attach(format!("analysis failed for {name}"))?;
        eprintln!(
            "  {} BPM (conf {:.2}), key {} (conf {:.2}), grid {} beats",
            out.bpm,
            out.bpm_confidence,
            out.key,
            out.key_confidence,
            out.beat_grid.beats.len()
        );

        loaded.push(LoadedTrack {
            analysis: analysis_from(&hash, &decoded, &out),
            pcm: decoded.samples,
        });
    }
    if loaded.len() < 2 {
        return Err(Report::new(CliError).attach("need at least two distinct tracks to mix"));
    }
    Ok(loaded)
}

/// SHA-256 prefix identity of a file's bytes.
fn hash_of(path: &Path) -> Result<String, Report<CliError>> {
    use sha2::{Digest, Sha256};
    let bytes = std::fs::read(path)
        .change_context(CliError)
        .attach(format!("cannot read {}", path.display()))?;
    let digest = Sha256::digest(&bytes);
    Ok(hex_prefix(&digest))
}

/// Lowercase hex of the first 8 bytes.
fn hex_prefix(digest: &[u8]) -> String {
    digest
        .iter()
        .take(8)
        .fold(String::with_capacity(16), |mut acc, b| {
            use std::fmt::Write as _;
            let _ = write!(acc, "{b:02x}");
            acc
        })
}

/// Decodes a file via the registry (extension-dispatched).
fn decode_of(path: &Path, registry: &DecoderRegistry) -> Result<DecodeAudio, Report<CliError>> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_lowercase)
        .unwrap_or_default();
    let bytes = std::fs::read(path)
        .change_context(CliError)
        .attach(format!("cannot read {}", path.display()))?;
    registry
        .decode(&bytes, &ext)
        .change_context(CliError)
        .attach(format!("decode failed for {}", path.display()))
}

/// Maps djcore's analyzer output into the engine's planner input.
fn analysis_from(hash: &str, decoded: &DecodeAudio, out: &AnalyzerOutput) -> TrackAnalysis {
    TrackAnalysis {
        hash: TrackHash(hash.to_owned()),
        bpm: out.bpm,
        bpm_confidence: out.bpm_confidence,
        key: out.key.clone(),
        duration: out.duration_seconds,
        beat_grid: djcore::analyzer::BeatGrid {
            downbeats: out.beat_grid.downbeats.clone(),
            beats: out.beat_grid.beats.clone(),
            bars: out.beat_grid.bars.clone(),
        },
        grid_stability: out.grid_stability,
        sample_rate: decoded.sample_rate,
        channels: decoded.channels.max(1),
        format: String::new(),
    }
}

/// Plans with the CLI's tempo-strategy mapping and the active
/// transition pair (name + beats thread into the plan).
fn plan_session_for(
    analyses: &[TrackAnalysis],
    config: &Config,
    transition: &automixah_engine::automation::transition_spec::TransitionSpec,
) -> automixah_engine::timeline::SessionPlan {
    let options = PlanOptions {
        target_bpm: config.target_bpm,
        force_drift_back: config.tempo_strategy == TempoStrategyArg::Driftback,
        transition_beats: transition.beats,
        transition_name: transition.name.clone(),
    };
    plan_with(analyses, options)
}

/// Loads the transition pair: `--automation` file or the default
/// 16-bar crossfade.
fn load_transition(
    config: &Config,
) -> Result<automixah_engine::automation::transition_spec::TransitionSpec, Report<CliError>> {
    use automixah_engine::automation::transition_spec::TransitionSpec;
    let Some(path) = &config.automation else {
        return Ok(TransitionSpec::default_pair());
    };
    let text = std::fs::read_to_string(path)
        .change_context(CliError)
        .attach(format!("cannot read automation file {}", path.display()))?;
    let spec = TransitionSpec::from_ron(&text).map_err(|reason| {
        Report::new(CliError)
            .attach(format!("invalid automation file {}", path.display()))
            .attach(reason)
    })?;
    eprintln!(
        "[automation] loaded pair {} ({} beats)",
        spec.name, spec.beats
    );
    Ok(spec)
}

/// Prints the plan summary and per-transition presets to stderr.
fn log_plan(
    plan: &automixah_engine::timeline::SessionPlan,
    _analyses: &[TrackAnalysis],
    transition: &automixah_engine::automation::transition_spec::TransitionSpec,
) {
    eprintln!("[plan] session {} BPM", plan.session_bpm);
    eprintln!("[plan] transition pair: {}", transition.name);
    for seg in &plan.segments {
        eprintln!(
            "  segment {} starts @ {} ({} s, stretch ratio {:.3} {:?})",
            seg.track_hash.0,
            seg.session_start.0,
            seg.len_samples / 44_100,
            seg.stretch.ratio,
            seg.stretch.mode
        );
        if let Some(t) = &seg.transition {
            log_transition(t);
        }
    }
    eprintln!(
        "[plan] total {:.1}s",
        plan.total_len_samples() as f64 / 44_100.0
    );
}

/// Prints one transition's window and automation preset.
fn log_transition(t: &TransitionPlan) {
    eprintln!(
        "    transition @ {:.1}s→{:.1}s preset {}",
        t.window.start.0 as f64 / 44_100.0,
        t.window.end.0 as f64 / 44_100.0,
        t.preset.0
    );
}

/// Writes mono f32 samples as a 32-bit float WAV.
fn write_wav(path: &Path, samples: &[f32], rate: u32) -> Result<(), hound::Error> {
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: rate,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut writer = hound::WavWriter::create(path, spec)?;
    for s in samples {
        writer.write_sample(*s)?;
    }
    writer.finalize()?;
    Ok(())
}

/// Stretched-PCM provider over the loaded tracks.
struct SessionPcm {
    pcms: std::collections::HashMap<String, Vec<f32>>,
}

/// Channels of a loaded track by hash (stereo machinery needs this
/// when stretching).
fn channels_of(loaded: &[LoadedTrack], hash: &str) -> u16 {
    loaded
        .iter()
        .find(|t| t.analysis.hash.0 == hash)
        .map_or(2, |t| t.analysis.channels.max(1))
}

impl SessionPcm {
    /// Stretches each track per its segment decision, caching by hash.
    fn new(loaded: &[LoadedTrack], plan: &automixah_engine::timeline::SessionPlan) -> Self {
        let by_hash: std::collections::HashMap<&str, &[f32]> = loaded
            .iter()
            .map(|t| (t.analysis.hash.0.as_str(), t.pcm.as_slice()))
            .collect();
        let mut pcms = std::collections::HashMap::new();
        for seg in &plan.segments {
            let hash = seg.track_hash.0.as_str();
            if pcms.contains_key(hash) {
                continue;
            }
            let source = by_hash.get(hash).copied().unwrap_or(&[]);
            let channels = usize::from(channels_of(loaded, hash));
            let stretched = match seg.stretch.mode {
                automixah_engine::timeline::types::StretchMode::Resample => {
                    automixah_engine::render::resample::Resampler::new(seg.stretch)
                        .resample_all_frames(source, channels)
                }
                automixah_engine::timeline::types::StretchMode::Wsola => {
                    automixah_engine::render::wsola::Wsola::new(seg.stretch)
                        .stretch_all_frames(source, channels)
                }
            };
            // Slice from the stretched cue: the renderer indexes
            // segment PCM relative to the segment start, so the cue
            // must be removed here (frames → interleaved offset).
            // src_start is in source *frames*; ratio maps source
            // frames to stretched frames directly.
            #[expect(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "cue bounded by stretched length"
            )]
            let cue_frames = ((f64::from(seg.stretch.ratio) * seg.src_start as f64).round()
                as usize)
                .min(stretched.len() / channels);
            let sliced_raw = stretched[cue_frames * channels..].to_vec();
            // The renderer is interleaved stereo; mono sources are
            // duplicated to L==R here.
            let sliced = if channels == 2 {
                sliced_raw
            } else {
                sliced_raw
                    .iter()
                    .flat_map(|&s| [s, s])
                    .collect::<Vec<f32>>()
            };
            eprintln!(
                "  [stretch] {} → {} frames from cue {} ({})",
                hash,
                sliced.len() / channels,
                cue_frames,
                format!("{:?}", seg.stretch.mode).as_str()
            );
            pcms.insert(hash.to_owned(), sliced);
        }
        Self { pcms }
    }
}

impl TrackProvider for SessionPcm {
    fn name(&self) -> &'static str {
        "session-pcm"
    }

    fn stretched_pcm(
        &mut self,
        hash: &TrackHash,
    ) -> Result<&[f32], automixah_engine::render::renderer::TrackFetchError> {
        self.pcms
            .get(&hash.0)
            .map(Vec::as_slice)
            .ok_or(automixah_engine::render::renderer::TrackFetchError)
    }
}
