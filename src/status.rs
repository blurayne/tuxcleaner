use std::fs;
use std::process::Command;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryStatus {
    pub total_bytes: u64,
    pub available_bytes: u64,
    pub used_bytes: u64,
    pub used_percent: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiskStatus {
    pub filesystem: String,
    pub mount: String,
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub available_bytes: u64,
    pub used_percent: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemStatus {
    pub hostname: String,
    pub logical_cpus: usize,
    pub load_average: [f64; 3],
    pub uptime_seconds: u64,
    pub memory: MemoryStatus,
    pub disks: Vec<DiskStatus>,
}

pub fn collect() -> Result<SystemStatus> {
    Ok(SystemStatus {
        hostname: read_hostname(),
        logical_cpus: std::thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(1),
        load_average: read_load_average()?,
        uptime_seconds: read_uptime()?,
        memory: read_memory()?,
        disks: read_disks()?,
    })
}

fn read_hostname() -> String {
    fs::read_to_string("/etc/hostname")
        .map(|value| value.trim().to_owned())
        .unwrap_or_else(|_| "unknown".into())
}

fn read_load_average() -> Result<[f64; 3]> {
    let content = fs::read_to_string("/proc/loadavg").context("failed to read /proc/loadavg")?;
    let values: Vec<f64> = content
        .split_whitespace()
        .take(3)
        .map(str::parse)
        .collect::<Result<_, _>>()?;
    Ok([
        values.first().copied().unwrap_or(0.0),
        values.get(1).copied().unwrap_or(0.0),
        values.get(2).copied().unwrap_or(0.0),
    ])
}

fn read_uptime() -> Result<u64> {
    let content = fs::read_to_string("/proc/uptime").context("failed to read /proc/uptime")?;
    let seconds: f64 = content.split_whitespace().next().unwrap_or("0").parse()?;
    Ok(seconds.max(0.0) as u64)
}

fn read_memory() -> Result<MemoryStatus> {
    let content = fs::read_to_string("/proc/meminfo").context("failed to read /proc/meminfo")?;
    let value = |key: &str| -> u64 {
        content
            .lines()
            .find(|line| line.starts_with(key))
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(0)
            .saturating_mul(1024)
    };
    let total_bytes = value("MemTotal:");
    let available_bytes = value("MemAvailable:");
    let used_bytes = total_bytes.saturating_sub(available_bytes);
    let used_percent = percentage(used_bytes, total_bytes);
    Ok(MemoryStatus {
        total_bytes,
        available_bytes,
        used_bytes,
        used_percent,
    })
}

fn read_disks() -> Result<Vec<DiskStatus>> {
    let output = Command::new("df")
        .args(["-B1", "-P"])
        .output()
        .context("failed to run df")?;
    if !output.status.success() {
        anyhow::bail!("df exited with {}", output.status);
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let disks = text
        .lines()
        .skip(1)
        .filter_map(|line| {
            let fields: Vec<_> = line.split_whitespace().collect();
            if fields.len() < 6 || !fields[0].starts_with('/') {
                return None;
            }
            let total_bytes = fields[1].parse().ok()?;
            let used_bytes = fields[2].parse().ok()?;
            let available_bytes = fields[3].parse().ok()?;
            Some(DiskStatus {
                filesystem: fields[0].into(),
                mount: fields[5..].join(" "),
                total_bytes,
                used_bytes,
                available_bytes,
                used_percent: percentage(used_bytes, total_bytes),
            })
        })
        .collect();
    Ok(disks)
}

fn percentage(part: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        part as f64 * 100.0 / total as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentage_handles_zero_total() {
        assert_eq!(percentage(10, 0), 0.0);
        assert_eq!(percentage(1, 4), 25.0);
    }
}
