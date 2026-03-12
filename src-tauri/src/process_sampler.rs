use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::OsStr;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::{Mutex, RwLock};
use serde::de;
use sysinfo::{Pid, Process, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time::sleep;
use tracing::debug;

// 单个进程指标
#[derive(Debug, Clone)]
pub struct ProcStat {
    pub pid: Pid,
    pub cpu_usage: f64,
    pub memory: u64,
}

#[derive(Debug, Clone)]
pub struct ProcessSnapshot {
    pub updated_at: Instant,
    pub procs: HashMap<Pid, ProcStat>,
    pub by_exe_path: HashMap<String, Vec<Pid>>,
}


#[derive(Debug)]
pub struct ProcessSampler {
    interval: Duration,
    state: Mutex<ProcessSamplerState>,
    sys: Arc<RwLock<System>>,
    cpu_len: usize,
}

#[derive(Debug, Default)]
struct ProcessSamplerState {
    stop_tx: Option<oneshot::Sender<()>>,
    handle: Option<JoinHandle<()>>,
}

impl ProcessSampler {
    pub fn new(interval: Duration) -> Self {
        Self {
            interval,
            state: Mutex::new(ProcessSamplerState::default()),
            sys: Arc::new(RwLock::new(System::new_all())),
            cpu_len: System::new_all().cpus().len(),
        }
    }

    pub fn spawn(&self) {
        let mut state = self.state.lock();
        if state.handle.is_some() {
            return;
        }
        debug!("启动进程指标采样器");
        let interval = self.interval;
        let (stop_tx, mut stop_rx) = oneshot::channel::<()>();
        let sys = self.sys.clone();
        let handle = tokio::spawn(async move {
            // 核心刷新代码，每隔 interval 刷新一次进程信息
            loop {
                tokio::select! {
                    _ = &mut stop_rx => break,
                    _ = sleep(interval) => {
                        sys.write().refresh_processes_specifics(ProcessesToUpdate::All, true,
                            ProcessRefreshKind::nothing().with_cpu().with_memory().with_exe(UpdateKind::OnlyIfNotSet));
                    }
                }
            }
        });

        state.stop_tx = Some(stop_tx);
        state.handle = Some(handle);
    }

    pub async fn stop(&self) {
        debug!("停止进程指标采样器");
        let (stop_tx, handle) = {
            let mut state = self.state.lock();
            (state.stop_tx.take(), state.handle.take())
        };

        if let Some(tx) = stop_tx {
            let _ = tx.send(());
        }

        if let Some(handle) = handle {
            let _ = handle.await;
        }
    }

    // 刷新系统进程信息
    pub fn refresh_processes(&self) {
        self.sys.write().refresh_processes_specifics(ProcessesToUpdate::All, true,
            ProcessRefreshKind::nothing().with_cpu().with_memory().with_exe(UpdateKind::OnlyIfNotSet));
    }

    // 检查程序是否正在运行中
    pub fn check_alive(&self, pid: Pid) -> bool {
        self.sys.read().process(pid).is_some()
    }

    // // 获取当前系统的所有进程
    // fn get_processes(&self) -> &HashMap<Pid, Process>{
    //     self.sys.read().processes()
    // }

    // // 获取指定进程
    // fn get_process(&self, pid: Pid) -> Option<&Process> {
    //     self.sys.read().process(pid)
    // }

    // 获取指定进程树的资源占用
    pub fn get_process_tree_usage(&self, root_pid: Pid) -> ProcStat {
        let mut total_cpu = 0.0;
        let mut total_mem = 0;
        let sys = self.sys.read();
        if sys.process(root_pid).is_none() {
            return ProcStat{
                pid: root_pid,
                cpu_usage: total_cpu,
                memory: total_mem,
            };
        }
        // 构造索引 parent -> children
        let mut child_map: HashMap<Pid, Vec<Pid>> = HashMap::new();
        for (pid, process) in sys.processes() {
            if let Some(parent_pid) = process.parent() {
                child_map.entry(parent_pid).or_default().push(*pid);
            }
        }
        let mut queue = VecDeque::new();
        queue.push_back(root_pid);
        if let Some(root_proc) = sys.process(root_pid) {
            total_cpu += root_proc.cpu_usage() as f64;
            total_mem += root_proc.memory();
        }
        // BFS 查找子进程
        while let Some(current_pid) = queue.pop_front() {
            if let Some(processes) = child_map.get(&current_pid){
                for pid in processes {
                    if let Some(process) = sys.process(*pid) {
                        total_cpu += process.cpu_usage() as f64;
                        total_mem += process.memory();
                        queue.push_back(*pid);
                    }
                }
            }
        }
        ProcStat{
            pid: root_pid,
            cpu_usage: total_cpu / self.cpu_len as f64,
            memory: total_mem,
        }
    }

    /// 根据给定的程序路径，查找运行中最顶层的进程 PID。
    pub fn find_process_by_path(&self, program_path: &Path) -> Option<Pid> {
        let mut candidates = Vec::new();
        let sys = self.sys.read();
        for (pid, process) in sys.processes() {
            if process.exe().is_some_and(|path| path==program_path ){
                candidates.push((*pid, process));
            }
        }
        for (pid, process) in &candidates {
            if let Some(parent_pid) = process.parent() {
                if let Some(parent_proc) = sys.process(parent_pid) {
                    if parent_proc.exe().is_some_and(|path| path == program_path) {
                        continue;
                    }
                }
            }
            // 如果没有父进程，或者父进程是别的程序，这就是根进程
            return Some(*pid);
        }
        // 如果没找到主进程，回退到返回第一个
        candidates.first().map(|(pid, _)| *pid)
    }

    //  检查是否还有同名子进程正在运行
    pub fn check_for_handover(&self, old_pid: Pid, program_name: &OsStr) -> Option<Pid> {
        if let Some((child_pid, _)) = self.sys.read().processes().iter()
        .find(|(_, p)| p.parent() == Some(old_pid) && p.name() == program_name) {
            return Some(*child_pid);
        }
        None
    }
}