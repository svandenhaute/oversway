//! CPU, memory and power from /proc and sysfs at 1 Hz, whether or not the overlay
//! shows, so deltas are ready. The GPU is polled on its own thread through NVML.

use std::time::{Duration, Instant};

pub const GPU_INTERVAL: Duration = Duration::from_secs(2);
const RAPL: &str = "/sys/class/powercap/intel-rapl:0";

#[derive(Default)]
pub struct Stats {
    previous_cores: Vec<(u64, u64)>,
    previous_energy: Option<(u64, Instant)>,
    pub cpu: f32,
    pub cores: Vec<f32>,
    pub cpu_watts: Option<f32>,
    pub memory_used: u64,
    pub memory_total: u64,
    pub swap_used: u64,
    pub swap_total: u64,
    pub gpu: Option<GpuStats>,
}

#[derive(Clone, Debug)]
pub struct GpuStats {
    pub watts: f32,
    pub utilization: f32,
    pub memory_used: u64,
    pub memory_total: u64,
}

/// (total, idle) jiffies for the aggregate line followed by every core.
fn read_cpu_times() -> Vec<(u64, u64)> {
    let text = std::fs::read_to_string("/proc/stat").unwrap_or_default();
    text.lines()
        .take_while(|line| line.starts_with("cpu"))
        .map(|line| {
            let fields: Vec<u64> = line.split_whitespace().skip(1).filter_map(|field| field.parse().ok()).collect();
            // user nice system idle iowait irq softirq steal (guest is included in user).
            let total = fields.iter().take(8).sum();
            let idle = fields.get(3).copied().unwrap_or(0) + fields.get(4).copied().unwrap_or(0);
            (total, idle)
        })
        .collect()
}

fn read_number(path: &str) -> Option<u64> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

pub fn sample(stats: &mut Stats) {
    let times = read_cpu_times();
    if times.len() == stats.previous_cores.len() {
        let loads: Vec<f32> = times
            .iter()
            .zip(&stats.previous_cores)
            .map(|(&(total, idle), &(previous_total, previous_idle))| {
                let delta_total = total.saturating_sub(previous_total) as f32;
                let delta_idle = idle.saturating_sub(previous_idle) as f32;
                if delta_total > 0.0 { (1.0 - delta_idle / delta_total).clamp(0.0, 1.0) } else { 0.0 }
            })
            .collect();
        if let Some((aggregate, cores)) = loads.split_first() {
            stats.cpu = *aggregate;
            stats.cores = cores.to_vec();
        }
    }
    stats.previous_cores = times;

    // Package power from the RAPL energy counter (microjoules, wraps at max_energy_range_uj).
    if let Some(energy) = read_number(&format!("{RAPL}/energy_uj")) {
        let now = Instant::now();
        if let Some((previous, then)) = stats.previous_energy {
            let range = read_number(&format!("{RAPL}/max_energy_range_uj")).unwrap_or(u64::MAX);
            let delta = if energy >= previous { energy - previous } else { energy + range - previous };
            stats.cpu_watts = Some(delta as f32 / 1e6 / now.duration_since(then).as_secs_f32());
        }
        stats.previous_energy = Some((energy, now));
    }

    if let Ok(text) = std::fs::read_to_string("/proc/meminfo") {
        let field = |name: &str| {
            text.lines()
                .find(|line| line.starts_with(name))
                .and_then(|line| line.split_whitespace().nth(1))
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(0)
                * 1024
        };
        stats.memory_total = field("MemTotal:");
        stats.memory_used = stats.memory_total.saturating_sub(field("MemAvailable:"));
        stats.swap_total = field("SwapTotal:");
        stats.swap_used = stats.swap_total.saturating_sub(field("SwapFree:"));
    }
}

/// Blocking loop for its own thread: NVML (no nvidia-smi process), every GPU_INTERVAL.
pub fn poll_gpu(sender: impl Fn(GpuStats) -> bool) {
    let nvml = match nvml_wrapper::Nvml::init() {
        Ok(nvml) => nvml,
        Err(error) => {
            log::warn!("NVML unavailable, no GPU stats: {error}");
            return;
        }
    };
    let Ok(device) = nvml.device_by_index(0) else { return };
    loop {
        let memory = device.memory_info().ok();
        let stats = GpuStats {
            watts: device.power_usage().map_or(0.0, |milliwatts| milliwatts as f32 / 1000.0),
            utilization: device.utilization_rates().map_or(0.0, |rates| rates.gpu as f32 / 100.0),
            memory_used: memory.as_ref().map_or(0, |memory| memory.used),
            memory_total: memory.as_ref().map_or(0, |memory| memory.total),
        };
        if !sender(stats) {
            return;
        }
        std::thread::sleep(GPU_INTERVAL);
    }
}
