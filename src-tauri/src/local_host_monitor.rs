use std::{collections::VecDeque, sync::Arc};
use std::time::Duration;
use chrono::Local;
use parking_lot::{Mutex, RwLock};
use rusqlite::Connection;
use sysinfo::{
    CpuExt, CpuRefreshKind, NetworkExt, Networks, NetworksExt, System, SystemExt,
};
use tokio::time::sleep;
use crate::monitor::{LocalHostMonitorRecord, MonitorRecord, MonitorTarget};

/// 运行本地主机监控
pub async fn run_host_monitor(id: usize, monitor_enabled: Arc<RwLock<bool>>, performance_records: Arc<Mutex<VecDeque<MonitorRecord>>>) {
    let mut sys = System::new();
    sys.refresh_networks_list();
    sys.refresh_networks();
    // 寻找主网卡名称
    let main_interface_name = get_main_interface_name(sys.networks());
    // 监控循环
    while *monitor_enabled.read() {
        sleep(Duration::from_secs(1)).await;
        
        // 只刷新CPU使用率
        sys.refresh_cpu_specifics(CpuRefreshKind::everything().without_frequency());
    
        // 获取CPU使用率
        let cpu_usage = {
            let cpus = sys.cpus();
            if !cpus.is_empty() {
                cpus.iter().map(|cpu| cpu.cpu_usage()).sum::<f32>() / cpus.len() as f32
            } else {
                0.0
            }
        };
        // 获取内存使用率
        sys.refresh_memory();
        let used_memory = format!("{:.1}",(sys.used_memory() as f32) /1024.0/1024.0/1024.0);
        
        // 获取磁盘使用率(暂时使用0.0作为占位符)
        let disk_usage = 0.0;
        
        // 遍历所有网卡计算总速度
        let mut total_rx = 0;
        let mut total_tx = 0;
        // 刷新网络数据（这会更新自上次刷新以来的数据变动）
        sys.refresh_networks();
        for (name, network) in sys.networks() {
            if name == &main_interface_name {
                // received() 和 transmitted() 返回的是"自上次刷新后"的新增字节数
                total_rx += network.received();
                total_tx += network.transmitted();
                break;
            }
        }

        let timestamp = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();

        // 记录
        let record = LocalHostMonitorRecord {
            mt_id: id,
            cpu_usage,
            memory_usage: used_memory,
            disk_usage,
            timestamp,
            download_speed: format_speed(total_rx),
            upload_speed: format_speed(total_tx),
            network_saturation: 0,
        };
        let mut records = performance_records.lock();
        records.pop_back();
        records.push_front(MonitorRecord::LocalHost(record));
    }
}



/// 寻找主网卡
fn get_main_interface_name(networks: &Networks) -> String {
    let mut max_traffic: u64 = 0;
    let mut main_interface = String::from("");

    // 定义需要忽略的网卡前缀或完整名称
    // lo: 本地回环
    // docker: docker 容器网桥
    // veth: 虚拟以太网接口(通常用于容器)
    // br-: 网桥
    // tun: 隧道 (VPN 等)
    // virbr: 虚拟化网桥(KVM/QEMU)
    let ignore_prefixes = ["lo", "docker", "veth", "br-", "tun", "virbr", "VMware"];

    for (interface_name, data) in networks {
        // 1. 过滤逻辑
        let is_ignored = ignore_prefixes.iter().any(|&prefix| interface_name.starts_with(prefix));
        if is_ignored {
            continue; // 跳过这些虚拟/回环网卡
        }
        // 2. 比较逻辑：看谁的累计流量大
        // 注意：这里用的是 total_xxx，是自开机以来的总流量
        let current_traffic = data.total_received() + data.total_transmitted();
        // 如果还没找到任何网卡，或者当前网卡流量比之前记录的更大
        if main_interface.is_empty() || current_traffic > max_traffic {
            max_traffic = current_traffic;
            main_interface = interface_name.clone();
        }
    }

    // 兜底：如果过滤完发现没网卡了（极少见），就随便返回第一个，或者返回"lo"
    if main_interface.is_empty() {
        if let Some((name, _)) = networks.iter().next() {
            return name.clone();
        }
        return "unknown".to_string();
    }

    main_interface
}


/// 自动格式化速度：B/s -> KB/s -> MB/s -> GB/s
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
