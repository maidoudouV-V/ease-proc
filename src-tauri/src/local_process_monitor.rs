use std::path::{Path, PathBuf};
use std::io::{self};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use std::collections::{HashMap, HashSet, VecDeque}; use chrono::Local;
use encoding_rs::GBK;
use parking_lot::{Mutex, RwLock};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
use tauri::{AppHandle, Manager};
use tokio::io::{AsyncRead, AsyncReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::spawn;
use tokio::time::{interval, sleep, MissedTickBehavior};
use tracing::{debug, error, info};
use winapi::um::winbase::{CREATE_NEW_CONSOLE, CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW};


use serde::Serialize;
use crate::monitor::{ConsoleMsg, LocalProcessConfig, LocalProcessMonitorRecord, MonitorRecord, StatusSignal};
use crate::process_guard::add_to_job;
use crate::process_sampler::ProcessSampler;


/// 本地进程性能记录
#[derive(Debug,Clone,Serialize)]
pub struct LocalProcessRecord {
    /// 唯一id
    pub id: usize,
    /// 监控目标ID
    pub mt_id: usize,
    /// CPU使用率
    pub cpu_usage: f64,
    /// 内存使用率
    pub memory_usage: f64,
    /// 磁盘使用率
    pub disk_usage: f64,
    /// 时间戳
    pub timestamp: String,
}


/// 开始运行程序监控
pub async fn run_program_monitor( id:usize, config: LocalProcessConfig,  
    monitor_enabled: Arc<RwLock<bool>>, 
    performance_records: Arc<Mutex<VecDeque<MonitorRecord>>>, 
    control_signal: Arc<Mutex<String>>, app_handle: AppHandle, 
    alias: String, console_outputs: Arc<Mutex<VecDeque<ConsoleMsg>>>, 
    process_sampler: Arc<ProcessSampler>) {

    debug!("正在启动本地程序监控 ID: {}, 配置: {:?}", id, config);
    // 初始化一条空记录
    {
        let mut performance_records_lock = performance_records.lock();
        performance_records_lock.push_front(MonitorRecord::LocalProcess(LocalProcessMonitorRecord{
            mt_id: id,
            cpu_usage: 0.0,
            memory_usage: 0,
            pid: 0,
            running: false,
        }));
    }
    // 读取配置
    let program_path = normalize_path(&config.path);
    
    // 路径错误时
    if !program_path.exists() || program_path.is_dir() {
        error!("监控启动失败，程序路径不存在 {:?}", program_path);
        if let Err(e) = app_handle.emit_all("status_signal", StatusSignal{
            mt_id: id,
            target_type: "LocalProcess".to_string(),
            signal: "error".to_string(),
            message: format!("程序路径不存在: {:?}", program_path),
        }) {
            error!("发送 status_signal(error) 失败, mt_id={}: {}", id, e);
        }
        return;
    }
    if let Err(e) = app_handle.emit_all("status_signal", StatusSignal{
        mt_id: id,
        target_type: "LocalProcess".to_string(),
        signal: "enable".to_string(),
        message: "监控任务启动成功:".to_string(),
    }) {
        error!("发送 status_signal(enable) 失败, mt_id={}: {}", id, e);
    }
    info!("本地进程监控已启动，正在监控 {}", alias);
    // let mut sys = System::new_all();
    let mut pid_running = None;
    // 当手动停止程序时，记录临时停止状态，防止自动重启
    let mut temp_stop = false;
    // 程序名，用于判断子程序
    let program_name = program_path.file_name().unwrap_or_default();
    while *monitor_enabled.read() {
        // 接收启动或关闭进程指令
        {
            let signal = control_signal.lock().clone();
            if !signal.is_empty() {
                *control_signal.lock() = String::new();
                if signal == "stop" {
                    debug!("收到手动停止信号，正在停止程序 {}", alias);
                    temp_stop = true;
                    // 检查进程是否还在
                    if let Some(current_pid) = pid_running{
                        pid_running = None;
                        if process_sampler.check_alive(current_pid, &program_name) {
                            kill_process(current_pid.as_u32()).await.expect("停止进程失败");
                            info!("已手动停止监控进程 {} PID:{}", alias, current_pid);
                        }
                    }
                    let mut performance_records_lock = performance_records.lock();
                    performance_records_lock.pop_back();
                    performance_records_lock.push_front(MonitorRecord::LocalProcess(LocalProcessMonitorRecord{
                        mt_id: id,
                        cpu_usage: 0.0,
                        memory_usage: 0,
                        pid: 0,
                        running: false,
                    }));
                    if let Err(e) = app_handle.emit_all("status_signal", StatusSignal{
                        mt_id: id,
                        target_type: "LocalProcess".to_string(),
                        signal: "stop".to_string(),
                        message: format!("{:?} 已停止运行", alias),
                    }) {
                        error!("发送 status_signal(stop) 失败, mt_id={}: {}", id, e);
                    }
                }else if signal == "start" {
                    debug!("收到手动启动信号，启动程序 {}", alias);
                    if pid_running.is_none(){
                        let child_res = start_process(&program_path, config.capture_output, app_handle.clone(), id, console_outputs.clone());
                        match child_res {
                            Ok(child) => {
                                let new_pid = Pid::from_u32(child.id().unwrap());
                                pid_running = Some(new_pid);
                                let mut performance_records_lock = performance_records.lock();
                                performance_records_lock.push_front(MonitorRecord::LocalProcess(LocalProcessMonitorRecord{
                                    mt_id: id,
                                    cpu_usage: 0.0,
                                    memory_usage: 0,
                                    pid: 0,
                                    running: true,
                                }));
                                info!("已手动启动监控进程 {} PID:{}", alias, new_pid);
                                process_sampler.refresh_processes();
                            }
                            Err(e) => {
                                error!("监控进程手动启动失败，{} 错误: {}", alias, e);
                                if let Err(e) = app_handle.emit_all("status_signal", StatusSignal{
                                    mt_id: id,
                                    target_type: "LocalProcess".to_string(),
                                    signal: "error".to_string(),
                                    message: format!("应用启动失败: {}", e),
                                }) {
                                    error!("发送 status_signal(error) 失败, mt_id={}: {}", id, e);
                                }
                                return;
                            }
                        }
                    }
                    if let Err(e) = app_handle.emit_all("status_signal", StatusSignal{
                        mt_id: id,
                        target_type: "LocalProcess".to_string(),
                        signal: "start".to_string(),
                        message: format!("{:?} 已启动", alias),
                    }) {
                        error!("发送 status_signal(start) 失败, mt_id={}: {}", id, e);
                    }
                    temp_stop = false;
                }
            }
        }

        // 检查进程是否存在
        if let Some(current_pid) = pid_running {
            if process_sampler.check_alive(current_pid, &program_name) {
                // 获取进程树（父进程+所有子进程）的总负载
                let pstat = process_sampler.get_process_tree_usage(current_pid);
                let mut performance_records_lock = performance_records.lock();
                performance_records_lock.pop_back();
                performance_records_lock.push_front(MonitorRecord::LocalProcess(LocalProcessMonitorRecord{
                    mt_id: id,
                    cpu_usage: pstat.cpu_usage,
                    memory_usage: pstat.memory,
                    pid: current_pid.as_u32(),
                    running: true,
                }));
            } else {
                // 检查是否还有子进程
                if let Some(new_pid) = process_sampler.check_for_handover(current_pid, program_name) {
                    pid_running = Some(Pid::from(new_pid));
                    debug!("{} 监控跳转到子进程 PID:{}", alias, new_pid.as_u32());
                    continue;
                }else{
                    // 进程丢失，设置pid = None
                    pid_running = None;
                    error!("进程 {} 停止运行 PID:{}", alias, current_pid.as_u32());
                    // 添加程序未运行记录
                    let mut performance_records_lock = performance_records.lock();
                    performance_records_lock.pop_back();
                    performance_records_lock.push_front(MonitorRecord::LocalProcess(LocalProcessMonitorRecord{
                        mt_id: id,
                        cpu_usage: 0.0,
                        memory_usage: 0,
                        pid: 0,
                        running: false,
                    }));
                    if let Err(e) = app_handle.emit_all("status_signal", StatusSignal{
                        mt_id: id,
                        target_type: "LocalProcess".to_string(),
                        signal: "refresh".to_string(),
                        message: "".to_string(),
                    }) {
                        error!("发送 status_signal(refresh) 失败, mt_id={}: {}", id, e);
                    }
                }
            }
        }else{
            // 查找进程
            pid_running = process_sampler.find_process_by_path(&program_path);
            // 程序未启动，判断是否需要自动重启
            if pid_running.is_none() && config.auto_restart && !temp_stop {
                sleep(Duration::from_secs(1)).await; // 避免频繁重启
                info!("正在自动重启程序 {}", alias);
                let child_res = start_process(&program_path, config.capture_output, app_handle.clone(), id, console_outputs.clone());
                match child_res {
                    Ok(child) => {
                        // 直接从 Child 对象获取 PID
                        let new_pid = Pid::from_u32(child.id().unwrap());
                        pid_running = Some(new_pid);
                        info!("程序已重新启动 {} PID:{}", alias, new_pid.as_u32());
                    }
                    Err(e) => {error!("程序自动启动失败 {} 错误：{}", alias, e);}
                }
            }
            if let Some(pid) = pid_running {
                info!("已发现运行中进程 {} PID:{:?}，正在监控", alias, pid.as_u32());
            }
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    if let Some(current_pid) = pid_running{
        kill_process(current_pid.as_u32()).await.expect("停止进程失败");
    }
    info!("本地进程监控已停止 {}", alias);
}


/// 启动目标程序，返回 `Child` 对象。
/// capture_output: true = 接管输出且无黑窗； false = 独立黑窗运行
pub fn start_process(program_path: &Path, capture_output: bool, app_handle: AppHandle, id:usize, console_outputs: Arc<Mutex<VecDeque<ConsoleMsg>>>) -> io::Result<Child> {
    let working_dir = program_path.parent().unwrap_or(Path::new("."));
    let mut cmd = Command::new(program_path);
    cmd.current_dir(working_dir);

    if capture_output {
        // 无黑窗且接管输出
        cmd.creation_flags(CREATE_NO_WINDOW);
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
    } else {
        // 独立黑窗运行
        cmd.creation_flags(CREATE_NEW_CONSOLE | CREATE_NEW_PROCESS_GROUP);
    }

    let mut child = cmd.spawn()?;

    // 如果是接管模式，新建线程处理
    if capture_output {
        // 确保程序关闭时被监控程序也关闭
        if let Some(handle) = child.raw_handle() {
            add_to_job(handle);
        }
        // 处理 stdout
        if let Some(stdout) = child.stdout.take() {
            spawn(  handle_output(stdout, app_handle.clone(), id, console_outputs.clone()));
        }
        // 处理 stderr
        if let Some(stderr) = child.stderr.take() {
            spawn(handle_output(stderr, app_handle.clone(), id, console_outputs.clone()));
        }
    }
    Ok(child)
}



/// 先正常后强制关闭进程（递归清理整个进程树）
pub async fn kill_process(pid: u32) -> io::Result<()> {
    let mut system = System::new();
    system.refresh_processes(ProcessesToUpdate::All,true);
    let root_pid = Pid::from_u32(pid);
    if system.process(root_pid).is_none() {
        return Ok(());
    }

    // 收集所有需要终止的 PID (包括根进程和所有子孙进程)
    let mut pids_to_kill = vec![root_pid];
    let mut queue = vec![root_pid];

    // 查找所有后代
    while let Some(parent) = queue.pop() {
        for (child_pid, process) in system.processes() {
            if let Some(ppid) = process.parent() {
                if ppid == parent {
                    // 发现子进程，加入队列继续查它的子进程
                    queue.push(*child_pid);
                    pids_to_kill.push(*child_pid);
                }
            }
        }
    }
    // 尝试正常关闭 taskkill /T
    // CREATE_NO_WINDOW = 0x08000000 避免黑窗
    let test = Command::new("taskkill")
        .args(&["/PID", &pid.to_string()])
        .args(&["/T"]) 
        .creation_flags(0x08000000) 
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status().await;
    debug!("执行 taskkill 结果: {:?}", test);

    // 异步轮询等待退出
    let timeout = Duration::from_secs(3);
    let start = std::time::Instant::now();
    let interval = Duration::from_millis(300);

    // 循环检查所有目标进程
    while start.elapsed() < timeout {
        let mut all_dead = true;
        system.refresh_processes_specifics(ProcessesToUpdate::All, true,ProcessRefreshKind::nothing());
        for &target_pid in &pids_to_kill {
            if system.process(target_pid).is_some() {
                all_dead = false;
                break;
            }
        }
        
        if all_dead {
            return Ok(()); // 全部清理干净
        }
        sleep(interval).await;
    }

    // 超时未退，强制杀
    debug!("正在强制终止 PID: {}", pid);
    system.refresh_processes(ProcessesToUpdate::All, true);
    for &target_pid in &pids_to_kill {
        if let Some(process) = system.process(target_pid) {
            let _ = process.kill(); 
        }
    }

    Ok(())
}


// 比较两个路径是否相同（忽略 Windows 大小写差异）
fn paths_are_equal(p1: &Path, p2: &Path) -> bool {
    if cfg!(target_os = "windows") {
        let s1 = p1.to_string_lossy();
        let s2 = p2.to_string_lossy();
        if s1.len() != s2.len() {
            return false;
        }
        s1.bytes().zip(s2.bytes()).all(|(b1, b2)| {
            let n1 = if b1 == b'/' { b'\\' } else { b1.to_ascii_lowercase() };
            let n2 = if b2 == b'/' { b'\\' } else { b2.to_ascii_lowercase() };
            n1 == n2
        })
    } else {
        p1 == p2
    }
}


async fn handle_output <R>(out:R, app_handle: AppHandle, id:usize, console_outputs: Arc<Mutex<VecDeque<ConsoleMsg>>>) where R: AsyncRead + Unpin + Send + 'static {
    let mut reader = BufReader::new(out);
    let mut buf = [0u8; 4096];
    let mut pending: Vec<u8> = Vec::new();
    let mut ticker = interval(Duration::from_secs(1));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = ticker.tick() => {
                // 无换行残余按秒刷新
                if !pending.is_empty() {
                    emit_console_line(&pending, &app_handle, id, &console_outputs);
                    pending.clear();
                }
            }
            read_res = reader.read(&mut buf) => {
                let n = match read_res {
                    Ok(0) => break, // EOF
                    Ok(n) => n,
                    Err(e) => {
                        error!("读取子进程输出失败: {}", e);
                        break;
                    }
                };

                pending.extend_from_slice(&buf[..n]);

                // 有换行立即刷新
                let mut start = 0;
                while let Some(rel_pos) = pending[start..].iter().position(|&b| b == b'\n') {
                    let pos = start + rel_pos;
                    emit_console_line(&pending[start..=pos], &app_handle, id, &console_outputs);
                    start = pos + 1;
                }
                // 清理已消费部分
                if start > 0 {
                    pending.drain(..start);
                }
                
            }
        }
    }
    // flush 最后残余
    if !pending.is_empty() {
        emit_console_line(&pending, &app_handle, id, &console_outputs);
    }
}

fn emit_console_line(bytes: &[u8], app_handle: &AppHandle, id: usize, console_outputs: &Arc<Mutex<VecDeque<ConsoleMsg>>>) {
    let text = match String::from_utf8(bytes.to_vec()) {
        Ok(s) => s,
        Err(_) => {
            // 失败：尝试用 GBK 解码
            let (cow, _encoding_used, _had_errors) = GBK.decode(bytes);
            cow.to_string()
        }
    };
    let payload = ConsoleMsg{
        msg: text,
        time: Local::now().format("%H:%M:%S%.3f").to_string()
    };
    let _ = app_handle.emit_all(&format!("console_out_stream_{}", id), &payload);
    let mut lock = console_outputs.lock();
    lock.push_back(payload);
    if lock.len() > 2000 {
        lock.pop_front();
    }
}


fn normalize_path(s: &str) -> PathBuf {
    if cfg!(target_os = "windows") {
        PathBuf::from(s.replace('/', "\\"))
    } else {
        PathBuf::from(s)
    }
}
