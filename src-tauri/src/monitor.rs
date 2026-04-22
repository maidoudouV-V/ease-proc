use anyhow::Result;
use parking_lot::{Mutex, RwLock};
use rusqlite::types::ToSqlOutput;
use rusqlite::types::Value;
use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ValueRef};
use rusqlite::ToSql;
use serde::Deserialize;
use serde::Serialize;
use std::collections::VecDeque;
use std::fmt;
use std::sync::Arc;
use tauri::AppHandle;
use tokio::spawn;
use tokio::task::JoinHandle;

use crate::local_host_monitor::run_host_monitor;
use crate::local_process_monitor::run_program_monitor;
use crate::process_sampler::ProcessSampler;
use crate::remote_host_monitor::run_remote_host_monitor;
use crate::MonitorTargetDto;

#[derive(Debug)]
pub struct MonitorTarget {
    pub id: usize,
    pub target_type_cfg: MonitorTargetType,
    pub alias: String,
    pub monitor_enabled: Arc<RwLock<bool>>,
    pub task_handle: Option<JoinHandle<()>>,
    pub performance_records: Arc<Mutex<VecDeque<MonitorRecord>>>,
    pub control_signal: Arc<Mutex<String>>,
    pub console_outputs: Arc<Mutex<VecDeque<ConsoleMsg>>>,
}

impl MonitorTarget {
    pub fn new(
        id: usize,
        target_type: MonitorTargetType,
        alias: String,
        monitor_enabled: bool,
    ) -> Result<Self> {
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

    fn reconcile_runtime_state(&mut self) {
        let finished = self
            .task_handle
            .as_ref()
            .map(|handle| handle.is_finished())
            .unwrap_or(false);
        if finished {
            self.task_handle = None;
            *self.monitor_enabled.write() = false;
        }
    }

    pub fn start_monitor(
        &mut self,
        app_handle: AppHandle,
        local_process_sampler: Arc<ProcessSampler>,
    ) {
        self.reconcile_runtime_state();
        if self.task_handle.is_some() {
            return;
        }
        let handle = match &self.target_type_cfg {
            MonitorTargetType::LocalHost => spawn(run_host_monitor(
                self.id,
                self.monitor_enabled.clone(),
                self.performance_records.clone(),
            )),
            MonitorTargetType::LocalProcess(cfg) => spawn(run_program_monitor(
                self.id,
                cfg.clone(),
                self.monitor_enabled.clone(),
                self.performance_records.clone(),
                self.control_signal.clone(),
                app_handle,
                self.alias.clone(),
                self.console_outputs.clone(),
                local_process_sampler.clone(),
            )),
            MonitorTargetType::RemoteHost(cfg) => spawn(run_remote_host_monitor(
                self.id,
                cfg.clone(),
                self.monitor_enabled.clone(),
                self.performance_records.clone(),
                app_handle,
                self.alias.clone(),
            )),
        };
        self.task_handle = Some(handle);
        *self.monitor_enabled.write() = true;
    }

    pub fn stop_monitor(&mut self) -> Option<JoinHandle<()>> {
        self.reconcile_runtime_state();
        if let Some(handle) = self.task_handle.take() {
            *self.monitor_enabled.write() = false;
            self.task_handle = None;
            Some(handle)
        } else {
            None
        }
    }

    pub fn get_latest_performance(&self) -> Option<MonitorRecord> {
        self.performance_records.lock().front().cloned()
    }

    pub fn show_info(&mut self) -> MonitorShowInfo {
        self.reconcile_runtime_state();
        MonitorShowInfo {
            id: self.id,
            alias: self.alias.clone(),
            target_type: self.target_type_cfg.to_string(),
            monitor_enabled: *self.monitor_enabled.read(),
            performance_record: self.get_latest_performance(),
        }
    }

    pub fn get_config(&self) -> MonitorTargetDto {
        MonitorTargetDto {
            id: Some(self.id),
            target_type: self.target_type_cfg.clone(),
            alias: self.alias.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct MonitorShowInfo {
    pub id: usize,
    pub alias: String,
    pub target_type: String,
    pub monitor_enabled: bool,
    pub performance_record: Option<MonitorRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "target_type", content = "type_config")]
pub enum MonitorTargetType {
    LocalHost,
    LocalProcess(LocalProcessConfig),
    RemoteHost(RemoteHostConfig),
}

impl fmt::Display for MonitorTargetType {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            MonitorTargetType::LocalHost => write!(f, "LocalHost"),
            MonitorTargetType::LocalProcess(_) => write!(f, "LocalProcess"),
            MonitorTargetType::RemoteHost(_) => write!(f, "RemoteHost"),
        }
    }
}

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
            serde_json::from_str(s).map_err(|e| FromSqlError::Other(Box::new(e)))
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalProcessConfig {
    pub path: String,
    pub auto_restart: bool,
    pub capture_output: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteHostConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum MonitorRecord {
    LocalHost(LocalHostMonitorRecord),
    LocalProcess(LocalProcessMonitorRecord),
    RemoteHost(RemoteHostMonitorRecord),
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

#[derive(Clone, Debug, Serialize)]
pub struct RemoteHostMonitorRecord {
    pub mt_id: usize,
    pub cpu_usage: f32,
    pub memory_usage: String,
    pub download_speed: String,
    pub upload_speed: String,
    pub timestamp: String,
    pub running: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct StatusSignal {
    pub mt_id: usize,
    pub target_type: String,
    pub signal: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Log {
    pub id: usize,
    pub mt_id: usize,
    pub log_type: String,
    pub log_message: String,
    pub timestamp: String,
}

#[derive(Clone, Serialize, Debug)]
pub struct ConsoleMsg {
    pub msg: String,
    pub time: String,
}
