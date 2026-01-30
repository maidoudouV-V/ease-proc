use anyhow::anyhow;
use anyhow::Result;
use chardetng::EncodingDetector;
use parking_lot::{Mutex, RwLock};
use rusqlite::types::ToSqlOutput;
use rusqlite::types::Value;
use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ValueRef};
use rusqlite::ToSql;
use serde::Deserialize;
use serde::{Serialize, Serializer};
use ssh2::Session;
use tauri::AppHandle;
use tokio::time::sleep;
use winapi::um::winnt::FILE_SHARE_DELETE;
use std::collections::VecDeque;
use std::{fmt, io};
use std::collections::HashMap;
use std::io::Error;
use std::io::{Read};
use std::net::TcpStream;
use std::path::Path;
use std::process::{Child, Command};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use chrono::Local;
use tokio::spawn;
use tokio::task::JoinHandle;
// use tauri::async_runtime::spawn;
// use tauri::async_runtime::JoinHandle;

use winapi::um::winbase::{CREATE_NEW_CONSOLE, CREATE_NEW_PROCESS_GROUP};

use crate::local_process_monitor::run_program_monitor;
use crate::local_host_monitor::run_host_monitor;
use crate::MonitorTargetDto;


#[derive(Debug)]
pub struct MonitorTarget {
    pub id: usize,                    // 数据库唯一ID
    pub target_type_cfg: MonitorTargetType, // 目标类型
    pub alias: String,                // 别名
    pub monitor_enabled: Arc<RwLock<bool>>,        // 启动监控标志
    pub task_handle: Option<JoinHandle<()>>, // 任务线程句柄
    pub performance_records: Arc<Mutex<VecDeque<MonitorRecord>>>, //性能指标
    pub control_signal: Arc<Mutex<String>>, // 控制信号
    pub console_outputs: Arc<Mutex<VecDeque<ConsoleMsg>>>, // 控制台输出记录
}

impl MonitorTarget {
    // 创建新的监控目标
    pub fn new(id: usize, target_type: MonitorTargetType, alias: String, monitor_enabled: bool) -> Result<Self> {
        Ok(Self { 
            id, 
            target_type_cfg: target_type, 
            alias, 
            monitor_enabled: Arc::new(RwLock::new(monitor_enabled)), 
            task_handle: None,
            performance_records: Default::default(),
            control_signal: Arc::new(Mutex::new(String::new())),
            console_outputs: Default::default(),
        })
    }
    
    // 启动监控
    pub fn start_monitor(&mut self, app_handle: AppHandle) {
        if self.task_handle.is_some() {
            // 已经在运行
            return;
        }
        let handle = match &self.target_type_cfg {
            MonitorTargetType::LocalHost => {
                spawn(run_host_monitor(self.id, self.monitor_enabled.clone(), self.performance_records.clone()))
            }
            MonitorTargetType::LocalProcess(cfg) => {
                spawn(run_program_monitor(self.id, cfg.clone(), 
                self.monitor_enabled.clone(),
                 self.performance_records.clone(),
                  self.control_signal.clone(),
                  app_handle,
                self.alias.clone(), self.console_outputs.clone()))
            }
            MonitorTargetType::RemoteHost => {panic!()}
        };
        self.task_handle = Some(handle);
        *self.monitor_enabled.write() = true;
    }
    
    // 停止监控 需要通过返回的handle来等待任务结束
    pub fn stop_monitor(&mut self) -> Option<JoinHandle<()>> {
        if let Some(handle) = self.task_handle.take() {
            *self.monitor_enabled.write() = false;
            self.task_handle = None;
            let mut finished = false;
            Some(handle)
        }else {
            None
        }
    }
    
    // 获取最新一次性能指标
    pub fn get_latest_performance(&self) -> Option<MonitorRecord> {
        self.performance_records.lock().front().cloned()
    }
    
    // 返回给前端的监控信息
    pub fn show_info(&self) -> MonitorShowInfo {
        MonitorShowInfo{
            id: self.id,
            alias: self.alias.clone(),
            target_type: self.target_type_cfg.to_string(),
            monitor_enabled: *self.monitor_enabled.read(),
            performance_record: self.get_latest_performance(),
        }
    }

    // 返回完整配置信息
    pub fn get_config(&self) -> MonitorTargetDto {
        MonitorTargetDto{
            id: Some(self.id),
            target_type: self.target_type_cfg.clone(),
            alias: self.alias.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct MonitorShowInfo{
    pub id: usize,
    pub alias: String,
    pub target_type: String, // 也许可以用枚举
    pub monitor_enabled: bool,
    pub performance_record: Option<MonitorRecord>,
}
// 监控类型枚举
#[derive(Debug,Clone,Serialize,Deserialize)]
#[serde(tag="target_type", content="type_config")]
pub enum MonitorTargetType {
    LocalHost,           // 本机监控
    LocalProcess(LocalProcessConfig), // 本地进程监控
    RemoteHost,         // 远程主机监控
}
// 监控类型显示实现
impl fmt::Display for MonitorTargetType {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            MonitorTargetType::LocalHost => write!(f, "LocalHost"),
            MonitorTargetType::LocalProcess(_) => write!(f, "LocalProcess"),
            MonitorTargetType::RemoteHost => write!(f, "RemoteHost"),
        }
    }
}

// 为枚举实现数据库转换
impl ToSql for MonitorTargetType {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        let json_string = serde_json::to_string(self)
            .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
        Ok(ToSqlOutput::Owned(Value::Text(json_string)))
    }
}
impl FromSql for MonitorTargetType {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        value.as_str().and_then(|s| {
            serde_json::from_str(s)
                .map_err(|e| FromSqlError::Other(Box::new(e)))
        })
    }
}

/// 本地进程配置
#[derive(Debug,Clone,Serialize,Deserialize)]
pub struct LocalProcessConfig {
    /// 程序路径
    pub path: String,
    /// 是否自动重启
    pub auto_restart: bool,
    /// 是否接管输出
    pub capture_output: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum MonitorRecord{
    LocalHost(LocalHostMonitorRecord),
    LocalProcess(LocalProcessMonitorRecord)
}

#[derive(Clone, Debug, Serialize)]
pub struct LocalHostMonitorRecord {
    pub mt_id: usize,
    pub cpu_usage: f32,
    pub memory_usage: String,
    pub disk_usage: f32,
    pub timestamp: String,
    pub download_speed: String,
    pub upload_speed: String,
    pub network_saturation: u8,
}
#[derive(Clone, Debug, Serialize)]
pub struct LocalProcessMonitorRecord {
    pub mt_id: usize,
    pub cpu_usage: f64,
    pub memory_usage: u64,
    pub pid: u32,
    pub running: bool,
}

// 向前端发送的信号
#[derive(Clone, Debug, Serialize)]
pub struct StatusSignal {
    pub mt_id: usize,
    pub target_type: String,
    pub signal: String,
    pub message: String,
}

// ====test
pub async fn monitor_remote_server() -> Result<()> {
    // SSH连接配置
    let host = "192.168.244.135";
    let port = 22;
    let username = "root";
    let password = "Huawei12#$";

    // 建立连接
    let tcp = TcpStream::connect((host, port))?;
    let mut sess = Session::new()?;
    sess.set_tcp_stream(tcp);
    sess.handshake()?;
    
    // 认证
    sess.userauth_password(username, password)?;
    if !sess.authenticated() {
        return Err(anyhow!("SSH authentication failed"));
    }

    // 性能监控循环
    loop {
        // 获取CPU使用率
        let mut channel = sess.channel_session()?;
        channel.exec("top -bn1 | grep 'Cpu(s)' | awk '{print $2 + $4}'")?;
        let mut cpu_usage = String::new();
        channel.read_to_string(&mut cpu_usage)?;
        cpu_usage = cpu_usage.trim().to_string();

        // 获取内存使用率
        let mut channel = sess.channel_session()?;
        channel.exec("free | grep Mem | awk '{print $3/$2 * 100.0}'")?;
        let mut mem_usage = String::new();
        channel.read_to_string(&mut mem_usage)?;
        mem_usage = mem_usage.trim().to_string();

        // 获取磁盘使用率
        let mut channel = sess.channel_session()?;
        channel.exec("df -h / | awk 'NR==2 {print $5}'")?;
        let mut disk_usage = String::new();
        channel.read_to_string(&mut disk_usage)?;
        disk_usage = disk_usage.trim().to_string();

        println!(
            "CPU Usage: {}%, Memory Usage: {}%, Disk Usage: {}",
            cpu_usage, mem_usage, disk_usage
        );

        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

#[derive(Debug,Clone,Serialize)]
pub struct HostMonitorConfig {
    pub id: usize,           // 数据库唯一ID
    pub mt_id: usize,        // 监控目标ID
    pub user_name: String,   // 用户名
    pub password: String,    // 密码
}

#[derive(Debug,Clone,Serialize)]
pub struct Log {
    pub id: usize,           // 数据库唯一ID
    pub mt_id: usize,        // 监控目标ID
    pub log_type: String,    // 日志类型
    pub log_message: String, // 日志信息
    pub timestamp: String,   // 时间戳
}

#[derive(Clone, Serialize, Debug)]
pub struct ConsoleMsg{
    pub msg:String,
    pub time:String
}