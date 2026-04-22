use std::collections::VecDeque;
use std::io::Read;
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use chrono::Local;
use parking_lot::{Mutex, RwLock};
use ssh2::Session;
use tauri::{AppHandle, Manager};
use tracing::{error, info};

use crate::monitor::{
    MonitorRecord, RemoteHostConfig, RemoteHostMonitorRecord, StatusSignal,
};

const SSH_CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const SSH_IO_TIMEOUT_MS: u32 = 3_000;
const SAMPLE_INTERVAL: Duration = Duration::from_secs(5);
const CPU_SAMPLE_DELAY: Duration = Duration::from_millis(250);

pub async fn run_remote_host_monitor(
    id: usize,
    config: RemoteHostConfig,
    monitor_enabled: Arc<RwLock<bool>>,
    performance_records: Arc<Mutex<VecDeque<MonitorRecord>>>,
    app_handle: AppHandle,
    alias: String,
) {
    set_remote_record(
        &performance_records,
        id,
        0.0,
        "0.0 GB".to_string(),
        "0 B/s".to_string(),
        "0 B/s".to_string(),
        false,
    );

    if config.host.trim().is_empty()
        || config.username.trim().is_empty()
        || config.password.trim().is_empty()
    {
        *monitor_enabled.write() = false;
        emit_status(
            &app_handle,
            id,
            "error",
            "远程主机配置不完整，请检查主机地址、用户名和密码",
        );
        return;
    }

    info!("远程主机监测线程已启动 {}", alias);

    let mut last_error_message = String::new();
    let mut online = false;
    let mut previous_net_counters: Option<NetCounters> = None;
    let mut previous_sample_at: Option<Instant> = None;
    while *monitor_enabled.read() {
        let sample_res = tokio::task::spawn_blocking({
            let config = config.clone();
            let previous_net_counters = previous_net_counters;
            let previous_sample_at = previous_sample_at;
            move || collect_remote_host_metrics(&config, previous_net_counters, previous_sample_at)
        })
        .await;

        match sample_res {
            Ok(Ok(sample)) => {
                if !online {
                    info!("远程主机已连接 {}", alias);
                }
                online = true;
                last_error_message.clear();
                previous_net_counters = Some(sample.net_counters);
                previous_sample_at = Some(sample.sampled_at);
                set_remote_record(
                    &performance_records,
                    id,
                    sample.cpu_usage,
                    sample.memory_usage,
                    sample.download_speed,
                    sample.upload_speed,
                    true,
                );
            }
            Ok(Err(err)) => {
                let err_message = err.to_string();
                if online || last_error_message.is_empty() {
                    error!("远程主机 {} 采样失败: {}", alias, err_message);
                    last_error_message = err_message;
                }
                online = false;
                previous_net_counters = None;
                previous_sample_at = None;
                set_remote_record(
                    &performance_records,
                    id,
                    0.0,
                    "0.0 GB".to_string(),
                    "0 B/s".to_string(),
                    "0 B/s".to_string(),
                    false,
                );
            }
            Err(err) => {
                let err_message = err.to_string();
                if online || last_error_message.is_empty() {
                    error!("远程主机 {} 采样任务异常: {}", alias, err_message);
                    last_error_message = err_message;
                }
                online = false;
                previous_net_counters = None;
                previous_sample_at = None;
                set_remote_record(
                    &performance_records,
                    id,
                    0.0,
                    "0.0 GB".to_string(),
                    "0 B/s".to_string(),
                    "0 B/s".to_string(),
                    false,
                );
            }
        }

        tokio::time::sleep(SAMPLE_INTERVAL).await;
    }

    info!("远程主机监测线程已停止 {}", alias);
}

fn set_remote_record(
    performance_records: &Arc<Mutex<VecDeque<MonitorRecord>>>,
    id: usize,
    cpu_usage: f32,
    memory_usage: String,
    download_speed: String,
    upload_speed: String,
    running: bool,
) {
    let record = RemoteHostMonitorRecord {
        mt_id: id,
        cpu_usage,
        memory_usage,
        download_speed,
        upload_speed,
        timestamp: Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
        running,
    };
    let mut performance_records_lock = performance_records.lock();
    performance_records_lock.pop_back();
    performance_records_lock.push_front(MonitorRecord::RemoteHost(record));
}

fn emit_status(app_handle: &AppHandle, id: usize, signal: &str, message: &str) {
    if let Err(err) = app_handle.emit_all(
        "status_signal",
        StatusSignal {
            mt_id: id,
            target_type: "RemoteHost".to_string(),
            signal: signal.to_string(),
            message: message.to_string(),
        },
    ) {
        error!("发送远程主机状态事件失败 mt_id={}: {}", id, err);
    }
}

#[derive(Clone, Copy)]
struct NetCounters {
    rx_bytes: u64,
    tx_bytes: u64,
}

struct RemoteHostSample {
    cpu_usage: f32,
    memory_usage: String,
    download_speed: String,
    upload_speed: String,
    net_counters: NetCounters,
    sampled_at: Instant,
}

fn collect_remote_host_metrics(
    config: &RemoteHostConfig,
    previous_net_counters: Option<NetCounters>,
    previous_sample_at: Option<Instant>,
) -> Result<RemoteHostSample> {
    let socket_addr = (config.host.as_str(), config.port)
        .to_socket_addrs()
        .map_err(|e| anyhow!("解析 {}:{} 失败: {}", config.host, config.port, e))?
        .next()
        .ok_or_else(|| anyhow!("未找到 {}:{} 的可用地址", config.host, config.port))?;
    let tcp = TcpStream::connect_timeout(&socket_addr, SSH_CONNECT_TIMEOUT)
        .map_err(|e| anyhow!("连接 {}:{} 失败: {}", config.host, config.port, e))?;
    tcp.set_read_timeout(Some(SSH_CONNECT_TIMEOUT)).ok();
    tcp.set_write_timeout(Some(SSH_CONNECT_TIMEOUT)).ok();

    let mut session = Session::new().map_err(|e| anyhow!("创建 SSH 会话失败: {}", e))?;
    session.set_timeout(SSH_IO_TIMEOUT_MS);
    session.set_tcp_stream(tcp);
    session
        .handshake()
        .map_err(|e| anyhow!("SSH 握手失败: {}", e))?;
    session
        .userauth_password(&config.username, &config.password)
        .map_err(|e| anyhow!("SSH 认证失败: {}", e))?;

    if !session.authenticated() {
        return Err(anyhow!("SSH 认证失败"));
    }

    let cpu_stat_before = exec_remote_command(&session, "cat /proc/stat")?;
    let memory_info = exec_remote_command(&session, "cat /proc/meminfo")?;
    let network_info = exec_remote_command(&session, "cat /proc/net/dev")?;
    std::thread::sleep(CPU_SAMPLE_DELAY);
    let cpu_stat_after = exec_remote_command(&session, "cat /proc/stat")?;

    let cpu_usage = parse_cpu_usage(&cpu_stat_before, &cpu_stat_after)?;
    let memory_usage = parse_memory_usage(&memory_info)?;
    let net_counters = parse_network_counters(&network_info)?;
    let sampled_at = Instant::now();

    let (download_speed, upload_speed) =
        format_network_speeds(previous_net_counters, previous_sample_at, net_counters, sampled_at);

    Ok(RemoteHostSample {
        cpu_usage,
        memory_usage,
        download_speed,
        upload_speed,
        net_counters,
        sampled_at,
    })
}

fn exec_remote_command(session: &Session, command: &str) -> Result<String> {
    let mut channel = session
        .channel_session()
        .map_err(|e| anyhow!("创建 SSH 通道失败: {}", e))?;
    channel
        .exec(command)
        .map_err(|e| anyhow!("执行远程命令失败: {}", e))?;

    let mut output = String::new();
    channel
        .read_to_string(&mut output)
        .map_err(|e| anyhow!("读取远程命令输出失败: {}", e))?;
    channel
        .wait_close()
        .map_err(|e| anyhow!("关闭 SSH 通道失败: {}", e))?;

    let exit_status = channel
        .exit_status()
        .map_err(|e| anyhow!("读取远程命令退出状态失败: {}", e))?;
    if exit_status != 0 {
        return Err(anyhow!("远程命令退出码异常: {}", exit_status));
    }

    Ok(output.trim().to_string())
}

fn parse_cpu_usage(before: &str, after: &str) -> Result<f32> {
    let before = parse_cpu_totals(before)?;
    let after = parse_cpu_totals(after)?;
    let total_delta = after.0.saturating_sub(before.0);
    let idle_delta = after.1.saturating_sub(before.1);
    if total_delta == 0 {
        return Ok(0.0);
    }
    Ok(((total_delta - idle_delta) as f32 / total_delta as f32) * 100.0)
}

fn parse_cpu_totals(contents: &str) -> Result<(u64, u64)> {
    let cpu_line = contents
        .lines()
        .find(|line| line.starts_with("cpu "))
        .ok_or_else(|| anyhow!("未找到 /proc/stat 的 cpu 行"))?;
    let values = cpu_line
        .split_whitespace()
        .skip(1)
        .map(|value| {
            value
                .parse::<u64>()
                .map_err(|e| anyhow!("解析 CPU 统计值失败: {}", e))
        })
        .collect::<Result<Vec<_>>>()?;
    if values.len() < 5 {
        return Err(anyhow!("CPU 统计字段数量不足"));
    }
    let total = values.iter().sum();
    let idle = values[3] + values.get(4).copied().unwrap_or(0);
    Ok((total, idle))
}

fn parse_memory_usage(contents: &str) -> Result<String> {
    let mut total_kb = None;
    let mut available_kb = None;
    for line in contents.lines() {
        if line.starts_with("MemTotal:") {
            total_kb = Some(parse_meminfo_kb(line)?);
        } else if line.starts_with("MemAvailable:") {
            available_kb = Some(parse_meminfo_kb(line)?);
        }
    }
    let total_kb = total_kb.ok_or_else(|| anyhow!("未找到 MemTotal"))?;
    let available_kb = available_kb.ok_or_else(|| anyhow!("未找到 MemAvailable"))?;
    let used_gb = (total_kb.saturating_sub(available_kb)) as f64 / 1024.0 / 1024.0;
    Ok(format!("{:.1} GB", used_gb))
}

fn parse_meminfo_kb(line: &str) -> Result<u64> {
    line.split_whitespace()
        .nth(1)
        .ok_or_else(|| anyhow!("内存信息字段缺失"))?
        .parse::<u64>()
        .map_err(|e| anyhow!("解析内存信息失败: {}", e))
}

fn parse_network_counters(contents: &str) -> Result<NetCounters> {
    let ignore_prefixes = ["lo", "docker", "veth", "br-", "tun", "virbr", "VMware"];
    let mut rx_bytes = 0_u64;
    let mut tx_bytes = 0_u64;

    for line in contents.lines().skip(2) {
        let Some((name, stats)) = line.split_once(':') else {
            continue;
        };
        let interface_name = name.trim();
        if ignore_prefixes
            .iter()
            .any(|prefix| interface_name.starts_with(prefix))
        {
            continue;
        }
        let values = stats
            .split_whitespace()
            .map(|value| {
                value
                    .parse::<u64>()
                    .map_err(|e| anyhow!("解析网卡统计失败: {}", e))
            })
            .collect::<Result<Vec<_>>>()?;
        if values.len() < 16 {
            continue;
        }
        rx_bytes = rx_bytes.saturating_add(values[0]);
        tx_bytes = tx_bytes.saturating_add(values[8]);
    }

    Ok(NetCounters { rx_bytes, tx_bytes })
}

fn format_network_speeds(
    previous_counters: Option<NetCounters>,
    previous_sample_at: Option<Instant>,
    current_counters: NetCounters,
    current_sample_at: Instant,
) -> (String, String) {
    let Some(previous_counters) = previous_counters else {
        return ("0 B/s".to_string(), "0 B/s".to_string());
    };
    let Some(previous_sample_at) = previous_sample_at else {
        return ("0 B/s".to_string(), "0 B/s".to_string());
    };
    let elapsed_secs = current_sample_at
        .saturating_duration_since(previous_sample_at)
        .as_secs_f64();
    if elapsed_secs <= 0.0 {
        return ("0 B/s".to_string(), "0 B/s".to_string());
    }
    let rx_per_sec = ((current_counters.rx_bytes.saturating_sub(previous_counters.rx_bytes)) as f64
        / elapsed_secs)
        .round() as u64;
    let tx_per_sec = ((current_counters.tx_bytes.saturating_sub(previous_counters.tx_bytes)) as f64
        / elapsed_secs)
        .round() as u64;
    (format_speed(rx_per_sec), format_speed(tx_per_sec))
}

fn format_speed(bytes: u64) -> String {
    let speed = bytes as f64;
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;

    if speed < KB {
        format!("{:.0} B/s", speed)
    } else if speed < MB {
        format!("{:.0} KB/s", speed / KB)
    } else if speed < GB {
        format!("{:.1} MB/s", speed / MB)
    } else {
        format!("{:.1} GB/s", speed / GB)
    }
}
