use crate::{
    command::{sample_encode, PROGRESS_CHARS},
    console_ext::style,
};
use anyhow::{bail, ensure};
use clap::Parser;
use console::style;
use indicatif::{HumanBytes, HumanDuration, ProgressBar, ProgressStyle};
use std::{path::PathBuf, time::Duration};

const BAR_LEN: u64 = 1000;

/// Pseudo binary search using sample-encode to find the best crf value
/// delivering min-vmaf & max-encoded-percent.
///
/// Outputs:
/// * Best crf value
/// * Mean sample VMAF score
/// * Predicted full encode size
/// * Predicted full encode time
#[derive(Parser)]
pub struct Args {
    /// Input video file.
    #[clap(short, long)]
    pub input: PathBuf,

    /// Encoder preset. Higher presets means faster encodes, but with a quality tradeoff.
    #[clap(long)]
    pub preset: u8,

    /// Desired VMAF score for the
    #[clap(long, default_value_t = 95.0)]
    pub min_vmaf: f32,

    /// Maximum desired encoded size percentage of the input size.
    #[clap(long, default_value_t = 80.0)]
    pub max_encoded_percent: f32,

    /// Minimum (highest quality) crf value to try.
    #[clap(long, default_value_t = 10)]
    pub min_crf: u8,

    /// Maximum (lowest quality) crf value to try.
    #[clap(long, default_value_t = 55)]
    pub max_crf: u8,

    /// Number of 20s samples to use across the input video.
    /// More samples take longer but may provide a more accurate result.
    #[clap(long, default_value_t = 3)]
    pub samples: u64,
}

pub async fn crf_search(args: Args) -> anyhow::Result<()> {
    let bar = ProgressBar::new(12).with_style(
        ProgressStyle::default_bar()
            .template("{spinner:.cyan.bold} {elapsed_precise:.bold} {wide_bar:.cyan/blue} ({msg}eta {eta})")
            .progress_chars(PROGRESS_CHARS)
    );
    bar.enable_steady_tick(100);

    let best = run(&args, &bar).await?;

    bar.finish();

    // encode how-to hint + predictions
    eprintln!(
        "\n{} {}\n",
        style("Encode with:").dim(),
        style!(
            "ab-av1 encode -i {:?} --crf {} --preset {}",
            args.input,
            best.crf,
            args.preset,
        )
        .dim()
        .italic()
    );

    StdoutFormat::Human.print_result(&best);

    Ok(())
}

async fn run(
    Args {
        input,
        preset,
        min_vmaf,
        max_encoded_percent,
        min_crf,
        max_crf,
        samples,
    }: &Args,
    bar: &ProgressBar,
) -> anyhow::Result<Sample> {
    ensure!(min_crf <= max_crf, "Invalid --min-crf & --max-crf");

    let mut args = sample_encode::Args {
        input: input.clone(),
        crf: (min_crf + max_crf) / 2,
        preset: *preset,
        samples: 1,
        keep: false,
        stdout_format: sample_encode::StdoutFormat::Json,
    };

    bar.set_length(BAR_LEN);
    let sample_bar = ProgressBar::hidden();
    let mut crf_attempts = Vec::new();
    // if we're doing/did a 1-sample 3rd run
    let mut quick_3rd_run = false;

    for run in 1.. {
        // how much we're prepared to go higher than the min-vmaf
        let higher_tolerance = run as f32 * 0.2;
        args.samples = match run {
            // use a single sample on the first 2 runs for speed
            1 | 2 => 1,
            // use a single sample to test 3rd run on the boundary for speed
            3 if args.crf == *min_crf || args.crf == *max_crf => {
                quick_3rd_run = true;
                1
            }
            _ => *samples,
        };

        bar.set_message(format!("sampling crf {}, ", args.crf));
        let mut sample_task =
            tokio::task::spawn_local(sample_encode::run(args.clone(), sample_bar.clone()));

        // TODO replace with channel updates
        let sample_task = loop {
            match tokio::time::timeout(Duration::from_millis(100), &mut sample_task).await {
                Err(_) => {
                    let sample_progress =
                        sample_bar.position() as f64 / sample_bar.length().max(1) as f64;
                    bar.set_position(guess_progress(run, sample_progress, quick_3rd_run) as _);
                }
                Ok(o) => {
                    sample_bar.set_position(0);
                    break o;
                }
            }
        };

        let sample = Sample {
            crf: args.crf,
            samples: args.samples,
            enc: sample_task??,
        };
        crf_attempts.push(sample.clone());

        if sample.enc.vmaf > *min_vmaf {
            // good
            if run > 2
                && sample.enc.predicted_encode_percent < *max_encoded_percent as _
                && sample.enc.vmaf < min_vmaf + higher_tolerance
            {
                return Ok(sample);
            }
            let u_bound = crf_attempts
                .iter()
                .filter(|s| s.crf > sample.crf)
                .min_by_key(|s| s.crf);

            match u_bound {
                Some(upper) if upper.crf == sample.crf + 1 => {
                    return Ok(sample);
                }
                Some(upper) => {
                    args.crf = vmaf_lerp_crf(*min_vmaf, upper, &sample);
                }
                None if sample.crf == *max_crf => {
                    return Ok(sample);
                }
                None if run == 1 && sample.crf + 1 < *max_crf => {
                    args.crf = (sample.crf + max_crf) / 2;
                }
                None => args.crf = *max_crf,
            };
        } else {
            // not good enough
            if sample.enc.predicted_encode_percent > *max_encoded_percent as _
                || sample.crf == *min_crf
            {
                sample.print_attempt(bar, *min_vmaf, *max_encoded_percent);
                bail!("Failed to find a suitable crf");
            }

            let l_bound = crf_attempts
                .iter()
                .filter(|s| s.crf < sample.crf)
                .max_by_key(|s| s.crf);

            match l_bound {
                Some(lower) if lower.crf + 1 == sample.crf => {
                    sample.print_attempt(bar, *min_vmaf, *max_encoded_percent);
                    return Ok(lower.clone());
                }
                Some(lower) => {
                    args.crf = vmaf_lerp_crf(*min_vmaf, &sample, lower);
                }
                None if run == 1 && sample.crf > min_crf + 1 => {
                    args.crf = (min_crf + sample.crf) / 2;
                }
                None => args.crf = *min_crf,
            };
        }
        sample.print_attempt(bar, *min_vmaf, *max_encoded_percent);
    }
    unreachable!();
}

#[derive(Debug, Clone)]
struct Sample {
    enc: sample_encode::Output,
    crf: u8,
    samples: u64,
}

impl Sample {
    fn print_attempt(&self, bar: &ProgressBar, min_vmaf: f32, max_encoded_percent: f32) {
        let crf_label = style("- crf").dim();
        let mut crf = style(self.crf);
        let samples = style(match self.samples {
            1 => ", 1 sample",
            _ => "",
        })
        .dim();
        let vmaf_label = style("VMAF").dim();
        let mut vmaf = style(self.enc.vmaf);
        let mut percent = style!("{:.0}%", self.enc.predicted_encode_percent);
        let open = style("(").dim();
        let close = style(")").dim();

        if self.enc.vmaf < min_vmaf {
            crf = crf.red();
            vmaf = vmaf.red().bright();
        }
        if self.enc.predicted_encode_percent > max_encoded_percent as _ {
            crf = crf.red();
            percent = percent.red();
        }

        bar.println(format!(
            "{crf_label} {crf} {vmaf_label} {vmaf:.2} {open}{percent}{samples}{close}"
        ));
    }
}

#[derive(Debug, Clone, Copy, clap::ArgEnum)]
pub enum StdoutFormat {
    Human,
}

impl StdoutFormat {
    fn print_result(self, Sample { crf, enc, .. }: &Sample) {
        match self {
            Self::Human => {
                let crf = style(crf).bold().green();
                let vmaf = style(enc.vmaf).bold().green();
                let size = style(HumanBytes(enc.predicted_encode_size)).bold().green();
                let percent = style!("{}%", enc.predicted_encode_percent.round())
                    .bold()
                    .green();
                let time = style(HumanDuration(enc.predicted_encode_time)).bold();
                println!(
                    "crf {crf} VMAF {vmaf:.2} predicted full encode size {size} ({percent}) taking {time}"
                );
            }
        }
    }
}

/// Produce a crf value between given samples using vmaf score linear interpolation.
fn vmaf_lerp_crf(min_vmaf: f32, worse_q: &Sample, better_q: &Sample) -> u8 {
    assert!(
        worse_q.enc.vmaf <= min_vmaf
            && worse_q.enc.vmaf < better_q.enc.vmaf
            && better_q.crf < worse_q.crf,
        "invalid vmaf_lerp_crf usage: {:?}, {:?}",
        worse_q,
        better_q
    );

    let vmaf_diff = better_q.enc.vmaf - worse_q.enc.vmaf;
    let vmaf_factor = (min_vmaf - worse_q.enc.vmaf) / vmaf_diff;

    let crf_diff = worse_q.crf - better_q.crf;
    let lerp = (worse_q.crf as f32 - crf_diff as f32 * vmaf_factor).round() as u8;
    lerp.max(better_q.crf + 1)
}

fn guess_progress(run: usize, sample_progress: f64, quick_3rd_run: bool) -> f64 {
    let guess_total_samples = match run {
        // Guess 4 iterations initially
        1 | 2 | 3 | 4 if quick_3rd_run => 1 + 1 + 1 + 3,
        1 | 2 | 3 | 4 => 1 + 1 + 3 + 3,
        // Otherwise guess this iteration is the last
        _ if quick_3rd_run => 3 + (run - 3) * 3,
        _ => 2 + (run - 2) * 3,
    };

    (match run {
        1 => sample_progress,
        2 => 1.0 + sample_progress,
        3 if quick_3rd_run => 2.0 + sample_progress,
        _ if quick_3rd_run => 3.0 + (run - 4) as f64 * 3.0 + sample_progress * 3.0,
        _ => 2.0 + (run - 3) as f64 * 3.0 + sample_progress * 3.0,
    }) * BAR_LEN as f64
        / guess_total_samples as f64
}
