use std::path::Path;
use std::io::{self};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use std::collections::{HashMap, HashSet, VecDeque}; use chrono::Local;
use encoding_rs::GBK;
use parking_lot::{Mutex, RwLock};
use sysinfo::{
    Pid, PidExt, ProcessExt, System, SystemExt,
};
use tauri::{AppHandle, Manager};
use tokio::io::{AsyncRead, AsyncReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::spawn;
use tokio::time::sleep;
use tracing::{debug, error, info};
use winapi::um::winbase::{CREATE_NEW_CONSOLE, CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW};


use serde::Serialize;
use crate::monitor::{ConsoleMsg, LocalProcessConfig, LocalProcessMonitorRecord, MonitorRecord, StatusSignal};
use crate::process_guard::add_to_job;


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
pub async fn run_program_monitor( id:usize, config: LocalProcessConfig,  monitor_enabled: Arc<RwLock<bool>>, performance_records: Arc<Mutex<VecDeque<MonitorRecord>>>, control_signal: Arc<Mutex<String>>, app_handle: AppHandle, alias: String, console_outputs: Arc<Mutex<VecDeque<ConsoleMsg>>>) {
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
    let program_path = Path::new(&config.path);
    // 路径错误时
    if !program_path.exists() || program_path.is_dir() {
        error!("监控启动失败，程序路径不存在 {:?}", program_path);
        app_handle.emit_all("status_signal", StatusSignal{
            mt_id: id,
            target_type: "LocalProcess".to_string(),
            signal: "error".to_string(),
            message: format!("程序路径不存在: {:?}", program_path),
        }).unwrap();
        return;
    }
    app_handle.emit_all("status_signal", StatusSignal{
        mt_id: id,
        target_type: "LocalProcess".to_string(),
        signal: "enable".to_string(),
        message: "监控任务启动成功:".to_string(),
    }).unwrap();
    info!("本地程序监控已启动 {}", alias);
    let mut sys = System::new_all();
    let mut pid = None;
    // 当手动停止程序时，记录临时停止状态，防止自动重启
    let mut temp_stop = false;
    // 低频全量刷新计数
    let mut refresh_count = 0;
    while *monitor_enabled.read() {
        // 接收启动或关闭进程指令
        let signal = control_signal.lock().clone();
        if !signal.is_empty() {
            *control_signal.lock() = String::new();
            if signal == "stop" {
                debug!("收到手动停止信号，正在停止程序 {}", alias);
                temp_stop = true;
                // 检查进程是否还在
                if let Some(current_pid) = pid{
                    sys.refresh_process(current_pid);
                    pid = None;
                    if sys.process(current_pid).is_some() {
                        kill_process(current_pid.as_u32()).await.expect("停止进程失败");
                        let mut performance_records_lock = performance_records.lock();
                        performance_records_lock.pop_back();
                        performance_records_lock.push_front(MonitorRecord::LocalProcess(LocalProcessMonitorRecord{
                            mt_id: id,
                            cpu_usage: 0.0,
                            memory_usage: 0,
                            pid: 0,
                            running: false,
                        }));
                        app_handle.emit_all("status_signal", StatusSignal{
                            mt_id: id,
                            target_type: "LocalProcess".to_string(),
                            signal: "stop".to_string(),
                            message: format!("{:?} 已停止运行", alias),
                        }).unwrap();
                        info!("监控目标已手动停止运行 {} PID:{}", alias, current_pid);
                    }
                }
            }else if signal == "start" {
                debug!("收到手动启动信号，启动程序 {}", alias);
                if pid.is_none(){
                    let child_res = start_process(program_path, config.capture_output, app_handle.clone(), id, console_outputs.clone());
                    match child_res {
                        Ok(child) => {
                            let new_pid = Pid::from_u32(child.id().unwrap());
                            pid = Some(new_pid);
                            let mut performance_records_lock = performance_records.lock();
                            performance_records_lock.push_front(MonitorRecord::LocalProcess(LocalProcessMonitorRecord{
                                mt_id: id,
                                cpu_usage: 0.0,
                                memory_usage: 0,
                                pid: 0,
                                running: true,
                            }));
                            app_handle.emit_all("status_signal", StatusSignal{
                                mt_id: id,
                                target_type: "LocalProcess".to_string(),
                                signal: "start".to_string(),
                                message: format!("{:?} 已启动", alias),
                            }).unwrap();
                            info!("监控目标已手动启动 {} PID:{}", alias, new_pid);
                        }
                        Err(e) => {
                            error!("监控目标手动启动失败，{} 错误: {}", alias, e);
                            app_handle.emit_all("status_signal", StatusSignal{
                                mt_id: id,
                                target_type: "LocalProcess".to_string(),
                                signal: "error".to_string(),
                                message: format!("应用启动失败: {}", e),
                            }).unwrap();
                            return;
                        }
                    }
                }
                temp_stop = false;
            }
        }
        // 检查进程是否存在
        if let Some(current_pid) = pid {
            if refresh_count>60 {
                sys.refresh_processes();
                refresh_count = 0;
            }else{
                sys.refresh_process(current_pid);
                refresh_count += 1;
            }
            if sys.process(current_pid).is_some() {
                // 获取进程树（父进程+所有子进程）的总负载
                let (tree_cpu, tree_mem) = get_process_tree_usage(&mut sys, current_pid);
                let mut performance_records_lock = performance_records.lock();
                performance_records_lock.pop_back();
                performance_records_lock.push_front(MonitorRecord::LocalProcess(LocalProcessMonitorRecord{
                    mt_id: id,
                    cpu_usage: tree_cpu,
                    memory_usage: tree_mem,
                    pid: current_pid.as_u32(),
                    running: true,
                }));
            } else {
                // 检查是否是启动器模式（父死子在）
                if let Some(new_pid) = check_for_handover(&sys, current_pid, program_path) {
                    pid = Some(Pid::from(new_pid));
                    debug!("{} 监控跳转到子进程 PID:{}", alias, new_pid);
                    continue;
                }else{
                    // 进程丢失，设置pid = None
                    pid = None;
                    error!("程序已停止运行 {} PID:{}", alias, current_pid);
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
                    app_handle.emit_all("status_signal", StatusSignal{
                        mt_id: id,
                        target_type: "LocalProcess".to_string(),
                        signal: "refresh".to_string(),
                        message: "".to_string(),
                    }).unwrap();
                }
            }
        }else{
            // 查找进程
            sys.refresh_processes();
            pid = find_process_by_path(&sys, program_path);
            // 程序未启动，判断是否需要自动重启
            if pid.is_none() && config.auto_restart && !temp_stop {
                sleep(Duration::from_secs(1)).await; // 避免频繁重启
                info!("正在自动重启程序 {}", alias);
                let child_res = start_process(program_path, config.capture_output, app_handle.clone(), id, console_outputs.clone());
                match child_res {
                    Ok(child) => {
                        // 直接从 Child 对象获取 PID
                        let new_pid = Pid::from_u32(child.id().unwrap());
                        info!("程序已自动启动 {} PID:{}", alias, new_pid);
                        pid = Some(new_pid);
                    }
                    Err(e) => {error!("程序自动启动失败 {} 错误：{}", alias, e);}
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    if let Some(current_pid) = pid{
        kill_process(current_pid.as_u32()).await.expect("停止进程失败");
    }
    info!("本地程序监控已停止 {}", alias);
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

/// 根据给定的程序路径，查找运行中最顶层的进程 PID。
fn find_process_by_path(sys: &System, program_path: &Path) -> Option<Pid> {
    let mut candidates = Vec::new();
    for (pid, process) in sys.processes() {
        if paths_are_equal(process.exe(), program_path) {
            candidates.push((*pid, process));
        }
    }

    for (pid, process) in &candidates {
        if let Some(parent_pid) = process.parent() {
            // 检查父进程是否存在于我们的候选列表中（即父进程是否也是同一个程序）
            if let Some(parent_proc) = sys.process(parent_pid) {
                if paths_are_equal(parent_proc.exe(), program_path) {
                    // 父进程也是同一个程序，说明当前进程是子进程，跳过
                    continue;
                }
            }
        }
        // 如果没有父进程，或者父进程是别的程序（如 explorer.exe, cmd.exe），它就是我们要找的主进程
        return Some(*pid);
    }
    // 如果没找到主进程，回退到返回第一个
    candidates.first().map(|(pid, _)| *pid)
}

/// 优雅关闭进程（递归清理整个进程树）
pub async fn kill_process(pid: u32) -> io::Result<()> {
    let mut system = System::new();
    let root_pid = Pid::from_u32(pid);

    // 1. 全局刷新，建立父子关系树
    system.refresh_processes();

    // 如果连根进程都找不到，直接返回
    if system.process(root_pid).is_none() {
        return Ok(());
    }

    // 2. 收集所有需要终止的 PID (包括根进程和所有子孙进程)
    let mut pids_to_kill = vec![root_pid];
    let mut queue = vec![root_pid];

    // 使用简单的 BFS (广度优先搜索) 查找所有后代
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
    // 3. 尝试优雅关闭 (针对根进程调用 taskkill /T)
    // 虽然 taskkill /T 可能漏网，但作为第一波信号发送依然有价值
    // CREATE_NO_WINDOW = 0x08000000 避免黑窗
    let _ = Command::new("taskkill")
        .args(&["/PID", &pid.to_string()])
        .args(&["/T"]) 
        .creation_flags(0x08000000) 
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();

    // 4. 异步轮询等待退出
    let timeout = Duration::from_secs(6);
    let start = std::time::Instant::now();
    let interval = Duration::from_millis(200);

    // 循环检查所有目标进程
    while start.elapsed() < timeout {
        // 每次只刷新我们需要关注的那些进程，提高效率
        let mut all_dead = true;
        for &target_pid in &pids_to_kill {
            if system.refresh_process(target_pid) {
                all_dead = false; // 只要还有一个活着，就不能结束等待
            }
        }
        
        if all_dead {
            return Ok(()); // 全部清理干净
        }
        sleep(interval).await;
    }

    // 5. 超时未退，执行强制查杀 (对列表中的每一个幸存者补刀)
    // 倒序杀，先杀子进程可能更好，但全部强杀顺序无所谓
    for &target_pid in &pids_to_kill {
        if system.refresh_process(target_pid) {
            if let Some(process) = system.process(target_pid) {
                let _ = process.kill(); 
            }
        }
    }

    Ok(())
}

//  检查是否发生了进程交接 (启动器模式)
fn check_for_handover(sys: &System, old_pid: Pid, program_path: &Path) -> Option<Pid> {
    // 1. 找子进程
    if let Some((child_pid, _)) = sys.processes().iter().find(|(_, p)| p.parent() == Some(old_pid)) {
        return Some(*child_pid);
    }
    // 2. 找同路径但 PID 变了的进程 (Self-Restart)
    if let Some(new_pid) = find_process_by_path(sys, program_path) {
        if new_pid != old_pid {
            return Some(new_pid);
        }
    }
    None
}

// 递归计算进程树的资源占用
fn get_process_tree_usage(sys: &mut System, root_pid: Pid) -> (f64, u64) {
    let mut total_cpu = 0.0;
    let mut total_mem = 0;
    let mut queue = VecDeque::new();
    let mut visited = HashSet::new();

    queue.push_back(root_pid);
    visited.insert(root_pid);

    while let Some(current_pid) = queue.pop_front() {
        sys.refresh_process(current_pid);
        if let Some(process) = sys.process(current_pid) {
            total_cpu += process.cpu_usage() as f64;
            total_mem += process.memory();

            // 查找该进程的所有子进程
            for (child_pid, child_proc) in sys.processes() {
                if child_proc.parent() == Some(current_pid) && !visited.contains(child_pid) {
                    visited.insert(*child_pid);
                    queue.push_back(*child_pid);
                }
            }
        }
    }
    // 归一化 CPU 使用率
    (total_cpu / sys.cpus().len() as f64, total_mem)
}

// 比较两个路径是否相同（忽略 Windows 大小写差异）
fn paths_are_equal(p1: &Path, p2: &Path) -> bool {
    // let canon_p1 = p1.canonicalize().unwrap_or_else(|_| p1.to_path_buf());
    // let canon_p2 = p2.canonicalize().unwrap_or_else(|_| p2.to_path_buf());
    if cfg!(target_os = "windows") {
        p1.to_string_lossy().to_lowercase().replace("/", "\\") 
            == p2.to_string_lossy().to_lowercase().replace("/", "\\")
    } else {
        p1 == p2
    }
}

async fn handle_output <R>(out:R, app_handle: AppHandle, id:usize, console_outputs: Arc<Mutex<VecDeque<ConsoleMsg>>>) where R: AsyncRead + Unpin + Send + 'static {
    let mut reader = BufReader::new(out);
    let mut buf = [0u8; 4096];
    let mut pending: Vec<u8> = Vec::new();
    const MAX_PARTIAL: usize = 8192;

    loop {
        let n = match reader.read(&mut buf).await {
            Ok(0) => break, // EOF
            Ok(n) => n,
            Err(e) => {
                error!("读取子进程输出失败: {}", e);
                break;
            }
        };

        pending.extend_from_slice(&buf[..n]);

        while let Some(pos) = pending.iter().position(|&b| b == b'\n') {
            let line_bytes: Vec<u8> = pending.drain(..=pos).collect();
            emit_console_line(&line_bytes, &app_handle, id, &console_outputs);
        }

        // 如果一直没换行，避免堆积过大，直接刷出一段
        if pending.len() >= MAX_PARTIAL {
            let line_bytes = pending.split_off(0);
            emit_console_line(&line_bytes, &app_handle, id, &console_outputs);
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


