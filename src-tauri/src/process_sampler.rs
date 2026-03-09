use std::collections::{HashMap, HashSet, VecDeque};
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
    pub children: Arc<Mutex<HashMap<Pid, Vec<Pid>>>>,
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
            children: Arc::new(Mutex::new(HashMap::new())),
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
        let children = self.children.clone();
        let handle = tokio::spawn(async move {
            // 核心刷新代码，每隔 interval 刷新一次进程信息
            loop {
                tokio::select! {
                    _ = &mut stop_rx => break,
                    _ = sleep(interval) => {
                        sys.write().refresh_processes_specifics(ProcessesToUpdate::All, true,
                            ProcessRefreshKind::nothing().with_cpu().with_memory().with_exe(UpdateKind::OnlyIfNotSet));
                        // 建立 parent -> children
                        let mut children_guard = children.lock();
                        children_guard.clear();
                        for (child_pid, child_proc) in sys.read().processes() {
                            if let Some(parent_pid) = child_proc.parent() {
                                children_guard.entry(parent_pid).or_default().push(*child_pid);
                            }
                        }
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
        // 建立 parent -> children
        let mut children = self.children.lock();
        children.clear();
        for (child_pid, child_proc) in self.sys.read().processes() {
            if let Some(parent_pid) = child_proc.parent() {
                children.entry(parent_pid).or_default().push(*child_pid);
            }
        }
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
        let mut queue = VecDeque::new();
        let mut visited = HashSet::new();
        queue.push_back(root_pid);
        visited.insert(root_pid);
        let sys = self.sys.read();
        while let Some(current_pid) = queue.pop_front() {
            if let Some(process) = sys.process(current_pid) {
                total_cpu += process.cpu_usage() as f64;
                total_mem += process.memory();

                // 查找该进程的所有子进程
                if let Some(children) = &self.children.lock().get(&current_pid) {
                    for child_pid in *children {
                        if visited.insert(*child_pid) {
                            queue.push_back(*child_pid);
                        }
                    }
                }
            }
        }
        ProcStat{
            pid: root_pid,
            cpu_usage: total_cpu / self.sys.read().cpus().len() as f64,
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
            // 如果没有父进程，或者父进程是别的程序（如 explorer.exe, cmd.exe），这就是根进程
            return Some(*pid);
        }
        // 如果没找到主进程，回退到返回第一个
        candidates.first().map(|(pid, _)| *pid)
    }

    //  检查是否发生了进程交接 (启动器模式)
    pub fn check_for_handover(&self, old_pid: Pid, program_path: &Path) -> Option<Pid> {
        // 找子进程
        if let Some((child_pid, _)) = self.sys.read().processes().iter().find(|(_, p)| p.parent() == Some(old_pid)) {
            return Some(*child_pid);
        }
        None
    }
}