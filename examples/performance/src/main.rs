//! Real-`uv` performance runner for Runpy.
//!
//! ```text
//! performance run [--output report.json] [--requests N]
//! performance compare --baseline a.json [b.json ...] --current x.json [y.json ...]
//! ```
//!
//! `run` stages a PEP 723 echo worker that depends on this checkout's Python
//! SDK and measures, separately: the first spawn (which includes uv's
//! environment resolution), later cold spawns, steady-state request latency
//! for 1 KiB/64 KiB/1 MiB payloads on one reused worker, one-shot workers
//! (spawn + request + terminate), and pool batches at widths 1-3. It reports
//! median/p95/p99 latency, throughput, and the runner's peak RSS and open
//! descriptors as JSON.
//!
//! `compare` takes the median of each metric across several reports per side
//! and flags metrics whose current median is more than 20% slower. Flags are
//! printed (as GitHub warning annotations under Actions) and never fail the
//! process: shared hosts are too noisy for automatic gating.

use rand::{RngExt, SeedableRng, distr::Alphanumeric, rngs::StdRng};
use runpy::{Data, Manager, PoolConfig};
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

const TIMEOUT: Duration = Duration::from_secs(120);
const SIZES: [(&str, usize); 3] = [("1KiB", 1 << 10), ("64KiB", 64 << 10), ("1MiB", 1 << 20)];
const REGRESSION_RATIO: f64 = 1.20;

const ECHO_WORKER: &str = r#"
from runpyrs import RunScript, Worker

class Echo(Worker):
    def execute(self, data):
        return data

if __name__ == "__main__":
    RunScript(Echo)
"#;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("run") => run_command(&args[1..]),
        Some("compare") => compare_command(&args[1..]),
        _ => Err(
            "usage: performance run [--output FILE] [--requests N]\n       \
             performance compare --baseline FILE... --current FILE..."
                .to_string(),
        ),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
    }
}

fn run_command(args: &[String]) -> Result<(), String> {
    let mut output = None;
    let mut requests = 200_usize;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--output" => output = Some(PathBuf::from(args.next().ok_or("--output needs a path")?)),
            "--requests" => {
                requests = args
                    .next()
                    .and_then(|value| value.parse().ok())
                    .filter(|value| *value > 0)
                    .ok_or("--requests needs a positive number")?;
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    // Development mode forces verbose logging regardless of LOG, which would
    // put per-frame logging into every measurement.
    // SAFETY: runs before the runtime or any other thread starts.
    unsafe { std::env::remove_var("ENVIRONMENT") };
    if std::env::var_os("LOG").is_none() {
        // SAFETY: as above.
        unsafe { std::env::set_var("LOG", "off") };
    }
    let runtime = tokio::runtime::Runtime::new().map_err(|error| error.to_string())?;
    let report = runtime.block_on(measure(requests))?;
    let text = serde_json::to_string_pretty(&report).unwrap();
    match output {
        Some(path) => std::fs::write(&path, text + "\n").map_err(|error| error.to_string())?,
        None => println!("{text}"),
    }
    print_summary(&report);
    Ok(())
}

/// Deterministic payload of `size` ASCII bytes.
fn payload(size: usize) -> Data {
    let text: String = StdRng::seed_from_u64(0x5EED)
        .sample_iter(&Alphanumeric)
        .take(size)
        .map(char::from)
        .collect();
    json!({ "payload": text }).as_object().cloned().unwrap()
}

fn stage_worker(dir: &Path) -> Result<(), String> {
    let sdk = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../worker")
        .canonicalize()
        .map_err(|error| format!("local SDK not found: {error}"))?;
    let script = format!(
        "# /// script\n# requires-python = \">=3.10\"\n# dependencies = [\"runpyrs\"]\n#\n\
         # [tool.uv.sources]\n# runpyrs = {{ path = \"{}\", editable = true }}\n# ///\n{ECHO_WORKER}",
        sdk.display()
    );
    std::fs::write(dir.join("echo.py"), script).map_err(|error| error.to_string())
}

/// Samples peak open descriptors in the background while measuring.
struct DescriptorPeak {
    peak: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    thread: std::thread::JoinHandle<()>,
}

impl DescriptorPeak {
    fn start() -> Self {
        let peak = Arc::new(AtomicUsize::new(open_descriptors()));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let (peak, stop) = (peak.clone(), stop.clone());
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    peak.fetch_max(open_descriptors(), Ordering::Relaxed);
                    std::thread::sleep(Duration::from_millis(5));
                }
            })
        };
        Self { peak, stop, thread }
    }

    fn finish(self) -> usize {
        self.stop.store(true, Ordering::Relaxed);
        self.thread.join().unwrap();
        self.peak.load(Ordering::Relaxed)
    }
}

fn open_descriptors() -> usize {
    let directory = if cfg!(target_os = "linux") {
        "/proc/self/fd"
    } else {
        "/dev/fd"
    };
    std::fs::read_dir(directory).map_or(0, Iterator::count)
}

/// Peak resident set size of this process in bytes.
///
/// Linux uses `VmHWM`: `getrusage`'s `ru_maxrss` survives `execve`, so it
/// can report a launcher's peak instead of this program's.
fn peak_rss_bytes() -> Option<u64> {
    if cfg!(target_os = "linux") {
        let status = std::fs::read_to_string("/proc/self/status").ok()?;
        let line = status.lines().find(|line| line.starts_with("VmHWM:"))?;
        let kib: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
        return Some(kib * 1024);
    }
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
    // SAFETY: getrusage fills the provided struct.
    let usage = unsafe {
        libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr());
        usage.assume_init()
    };
    let max = usage.ru_maxrss.max(0) as u64;
    // macOS reports bytes, the BSDs KiB.
    Some(if cfg!(target_os = "macos") {
        max
    } else {
        max * 1024
    })
}

fn latency(samples: &mut [Duration]) -> Value {
    samples.sort_unstable();
    let at = |quantile: f64| {
        let index = ((samples.len() as f64 - 1.0) * quantile).round() as usize;
        samples[index].as_secs_f64() * 1000.0
    };
    json!({
        "samples": samples.len(),
        "median_ms": at(0.50),
        "p95_ms": at(0.95),
        "p99_ms": at(0.99),
    })
}

async fn measure(requests: usize) -> Result<Value, String> {
    let dir = tempfile::TempDir::new().map_err(|error| error.to_string())?;
    stage_worker(dir.path())?;
    let descriptors = DescriptorPeak::start();
    let manager = Manager::builder(dir.path())
        .startup_timeout(TIMEOUT)
        .build()
        .await
        .map_err(|error| format!("uv must be on PATH: {error}"))?;
    let mut metrics = Map::new();
    let mut throughput = Map::new();

    // Startup: the first spawn includes uv resolving the script environment;
    // later spawns reuse uv's cache and measure process + SDK startup.
    let started = Instant::now();
    let first = manager
        .worker("echo")
        .spawn()
        .await
        .map_err(|error| error.to_string())?;
    let first_spawn = started.elapsed();
    first.terminate().await.map_err(|error| error.to_string())?;
    metrics.insert("startup/first_spawn".into(), latency(&mut [first_spawn]));
    let cold_runs = (requests / 10).max(5);
    let mut spawns = Vec::with_capacity(cold_runs);
    let mut one_shots = Vec::with_capacity(cold_runs);
    let small = payload(1 << 10);
    for _ in 0..cold_runs {
        let started = Instant::now();
        let worker = manager
            .worker("echo")
            .spawn()
            .await
            .map_err(|error| error.to_string())?;
        spawns.push(started.elapsed());
        worker
            .execute(small.clone(), TIMEOUT)
            .await
            .map_err(|error| error.to_string())?;
        worker
            .terminate()
            .await
            .map_err(|error| error.to_string())?;
        one_shots.push(started.elapsed());
    }
    metrics.insert("startup/cold_spawn".into(), latency(&mut spawns));
    metrics.insert("request/cold_1KiB".into(), latency(&mut one_shots));

    // Steady state on one reused worker.
    let worker = manager
        .worker("echo")
        .spawn()
        .await
        .map_err(|error| error.to_string())?;
    for (name, size) in SIZES {
        let data = payload(size);
        let count = if size >= 1 << 20 {
            requests / 4
        } else {
            requests
        }
        .max(5);
        let mut samples = Vec::with_capacity(count);
        let started = Instant::now();
        for _ in 0..count {
            let begin = Instant::now();
            worker
                .execute(data.clone(), TIMEOUT)
                .await
                .map_err(|error| error.to_string())?;
            samples.push(begin.elapsed());
        }
        let elapsed = started.elapsed().as_secs_f64();
        metrics.insert(format!("request/reused_{name}"), latency(&mut samples));
        throughput.insert(
            format!("request/reused_{name}"),
            json!({
                "requests_per_s": count as f64 / elapsed,
                "mib_per_s": (count * size) as f64 / elapsed / f64::from(1 << 20),
            }),
        );
    }
    worker
        .terminate()
        .await
        .map_err(|error| error.to_string())?;

    // Pools: a fixed batch submitted at once, reused warm workers.
    for width in 1..=3_usize {
        let config = PoolConfig::default()
            .min_workers(width)
            .max_workers(width)
            .reuse_workers(true);
        let pool = manager
            .pool(manager.worker("echo"), config)
            .await
            .map_err(|error| error.to_string())?;
        let mut samples = Vec::with_capacity(requests);
        let started = Instant::now();
        let mut pending = Vec::with_capacity(requests);
        for _ in 0..requests {
            let submitted = Instant::now();
            pending.push((
                submitted,
                pool.submit(small.clone(), TIMEOUT)
                    .await
                    .map_err(|error| error.to_string())?,
            ));
        }
        for (submitted, request) in pending {
            request.result().await.map_err(|error| error.to_string())?;
            samples.push(submitted.elapsed());
        }
        let elapsed = started.elapsed().as_secs_f64();
        pool.close().await;
        metrics.insert(format!("pool/width_{width}_1KiB"), latency(&mut samples));
        throughput.insert(
            format!("pool/width_{width}_1KiB"),
            json!({ "requests_per_s": requests as f64 / elapsed }),
        );
    }
    drop(manager);

    Ok(json!({
        "environment": {
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "uv": command_version("uv"),
            "rustc": command_version("rustc"),
        },
        "requests": requests,
        "latency": metrics,
        "throughput": throughput,
        "resources": {
            "peak_rss_bytes": peak_rss_bytes(),
            "peak_open_descriptors": descriptors.finish(),
        },
    }))
}

fn command_version(program: &str) -> Value {
    std::process::Command::new(program)
        .arg("--version")
        .output()
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map_or(Value::Null, |text| Value::String(text.trim().to_string()))
}

fn print_summary(report: &Value) {
    eprintln!(
        "{:<28} {:>10} {:>10} {:>10}",
        "latency (ms)", "median", "p95", "p99"
    );
    for (name, stats) in report["latency"].as_object().into_iter().flatten() {
        eprintln!(
            "{name:<28} {:>10.2} {:>10.2} {:>10.2}",
            stats["median_ms"].as_f64().unwrap_or(f64::NAN),
            stats["p95_ms"].as_f64().unwrap_or(f64::NAN),
            stats["p99_ms"].as_f64().unwrap_or(f64::NAN),
        );
    }
    eprintln!("resources: {}", report["resources"]);
}

fn compare_command(args: &[String]) -> Result<(), String> {
    let mut baseline = Vec::new();
    let mut current = Vec::new();
    let mut side = None;
    for arg in args {
        match arg.as_str() {
            "--baseline" => side = Some(&mut baseline),
            "--current" => side = Some(&mut current),
            path => side
                .as_mut()
                .ok_or("reports must follow --baseline or --current")?
                .push(read_report(Path::new(path))?),
        }
    }
    if baseline.is_empty() || current.is_empty() {
        return Err("compare needs at least one baseline and one current report".into());
    }
    let github = std::env::var_os("GITHUB_ACTIONS").is_some();
    let mut flagged = 0;
    println!(
        "{:<28} {:>10} {:>10} {:>8}",
        "median latency (ms)", "baseline", "current", "change"
    );
    for (name, _) in current[0]["latency"].as_object().into_iter().flatten() {
        let (Some(before), Some(after)) = (median_of(&baseline, name), median_of(&current, name))
        else {
            continue;
        };
        let ratio = after / before;
        let marker = if ratio > REGRESSION_RATIO {
            flagged += 1;
            if github {
                println!(
                    "::warning title=Possible performance regression::{name}: median {before:.2} ms -> {after:.2} ms ({:+.0}%)",
                    (ratio - 1.0) * 100.0
                );
            }
            "REVIEW"
        } else {
            ""
        };
        println!(
            "{name:<28} {before:>10.2} {after:>10.2} {:>+7.1}% {marker}",
            (ratio - 1.0) * 100.0
        );
    }
    println!("{flagged} metric(s) more than 20% slower; review, do not gate on noisy hosts.");
    Ok(())
}

fn read_report(path: &Path) -> Result<Value, String> {
    let text =
        std::fs::read_to_string(path).map_err(|error| format!("{}: {error}", path.display()))?;
    serde_json::from_str(&text).map_err(|error| format!("{}: {error}", path.display()))
}

/// Median across reports of each report's median latency for `name`.
fn median_of(reports: &[Value], name: &str) -> Option<f64> {
    let mut medians: Vec<f64> = reports
        .iter()
        .filter_map(|report| report["latency"][name]["median_ms"].as_f64())
        .collect();
    if medians.is_empty() {
        return None;
    }
    medians.sort_by(f64::total_cmp);
    Some(medians[medians.len() / 2])
}
