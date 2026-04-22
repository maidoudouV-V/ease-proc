use std::collections::HashMap;
use std::env;
use std::ops::Deref;
use std::process::Command;
use std::time::Duration;
use crate::logging::LogEntry;
use crate::monitor::{ConsoleMsg, LocalProcessConfig, MonitorShowInfo, MonitorTarget, MonitorTargetType};
use crate::monitor_manager::MonitorManager;
use crate::{calculate_sha256, compare_versions, NodeDiscoveryState, NodeInfo, UpdateDate, UPDATE_REQ_MAGIC};

use chrono::Local;
use parking_lot::{Mutex, RawRwLock, RwLock};
use rusqlite::Connection;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::{sleep, timeout};
use tracing::{error, info};
use std::sync::Arc;
use parking_lot::lock_api::RwLockReadGuard;
use sysinfo::{System};
use tauri::{AppHandle, State};
use tokio::spawn;
use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};

// 专门用于接收前端创建/更新请求的结构体
#[derive(Debug, Deserialize, Serialize)]
pub struct MonitorTargetDto {
    pub id: Option<usize>, 
    #[serde(flatten)]
    pub target_type: MonitorTargetType,
    pub alias: String,
    // pub monitor_enabled: bool,
}

#[derive(Debug, Serialize)]
pub struct CommandError(String);
impl From<anyhow::Error> for CommandError {
    fn from(error: anyhow::Error) -> Self {
        Self(error.to_string())
    }
}
pub type CommandResult<T> = Result<T, CommandError>;

// 获取系统基础信息
#[tauri::command]
pub fn get_system_info() -> SystemInfo {
    let system = System::new_all();
    let cpu_core_count = system.cpus().len();
    let physical_core = System::physical_core_count().unwrap_or(0);
    // let info = os_info::get();
    SystemInfo {
        hostname: System::host_name().unwrap_or_else(|| String::from("Unknown")),
        os_name: System::long_os_version().unwrap_or("Unknown".to_string()),
        start_time: System::boot_time().to_string(),
        cpu_core_count,
        physical_core,
        total_memory: format!("{:.1}",(system.total_memory() as f32)/1024.0/1024.0/1024.0),
    }
}

// 刷新监控目标列表
#[tauri::command]
pub async fn refresh_monitor_targets(monitor_manager: State<'_, Arc<MonitorManager>>) -> CommandResult<Vec<MonitorShowInfo>> {
    let mut targets = monitor_manager.get_all_monitor_info().await;
    targets.retain(|t| t.target_type != "LocalHost");
    Ok(targets)
}

// 启动指定ID的监控目标
#[tauri::command]
pub async fn enable_monitor(monitor_manager: State<'_, Arc<MonitorManager>>, id: usize) -> Result<(), String> {
    if monitor_manager.enable_monitor(id).await {
        Ok(())
    } else {
        Err(format!("监控目标 {} 启动请求失败", id))
    }
}

// 禁用指定ID的监控目标
#[tauri::command]
pub async fn disable_monitor(monitor_manager: State<'_, Arc<MonitorManager>>, id: usize) -> Result<(), String> {
    let manager = monitor_manager.inner().clone();
    if manager.disable_monitor(id).await {
        Ok(())
    } else {
        Err(format!("监控目标 {} 停止请求失败", id))
    }
}

// 对指定目标发送控制指令
#[tauri::command]
pub async fn send_control_signal(monitor_manager: State<'_, Arc<MonitorManager>>, id: usize, signal: String) -> Result<(), String> {
    let manager = monitor_manager.inner().clone();
    if manager.change_control_signal(id, signal).await {
        Ok(())
    } else {
        Err(format!("监控目标 {} 不存在，无法发送控制指令", id))
    }
}

/// 添加一个监控目标
#[tauri::command]
pub fn add_monitor_target(monitor_manager: State<'_, Arc<MonitorManager>>, new_target_form: MonitorTargetDto) -> Result<(), String> { 
    let mut conn = monitor_manager.get_connection_pool().get().map_err(|e| e.to_string())?;
    // 插入监控目标
    conn.execute(
        "INSERT INTO MonitorTarget (type, alias, monitor_enabled) VALUES (?1, ?2, ?3)",
        (&new_target_form.target_type, &new_target_form.alias, false),
    ).map_err(|e| e.to_string())?;
    
    let id = conn.last_insert_rowid() as usize;
    
    // 创建新的监控目标实例
    let new_target = MonitorTarget::new(id, new_target_form.target_type, new_target_form.alias, false).unwrap();
    
    // 将新目标添加到管理器中
    monitor_manager.add_monitor(id, Arc::new(Mutex::new(new_target)));
    Ok(())
}


/// 删除监控目标
#[tauri::command]
pub async fn delete_monitor_target(monitor_manager: State<'_, Arc<MonitorManager>>, id: usize) -> CommandResult<String> {
    monitor_manager.remove_monitor(id).await.map_err(|e| CommandError::from(e))
}

/// 更新监控目标配置
#[tauri::command]
pub async fn update_monitor_target(monitor_manager: State<'_, Arc<MonitorManager>>, update_target_form: MonitorTargetDto) -> CommandResult<String> {
    monitor_manager.update_monitor_config(update_target_form).await.map_err(|e| CommandError::from(e))
}

/// 获取监控目标完整配置
#[tauri::command]
pub async fn get_monitor_full_config(monitor_manager: State<'_, Arc<MonitorManager>>, id: usize) -> CommandResult<MonitorTargetDto> {
    let target = monitor_manager.get_monitor_config_by_id(id);
    if let Some(target) = target {
        Ok(target)
    } else {
        Err(CommandError(format!("监控目标 {} 不存在", id)))
    }
}

// 获取系统日志
#[tauri::command] 
pub fn get_app_logs( 
    monitor_manager: State<'_, Arc<MonitorManager>>, 
    filter_type: String, // 前端传入 "info" 或 "debug" 
) -> CommandResult<Vec<LogEntry>> { 
    // 获取数据库连接
    let conn = monitor_manager.get_connection_pool().get().map_err(|e| CommandError(e.to_string()))?; 
    // 构建 SQL：默认查询所有
    let mut sql = "SELECT log_time, log_level, log_message FROM SystemLog".to_string(); 
    if filter_type == "info" { 
        sql.push_str(" WHERE log_level != 'DEBUG'"); 
    } 
    sql.push_str(" ORDER BY id DESC LIMIT 1000"); 
  
    let mut stmt = conn.prepare(&sql).map_err(|e| CommandError(e.to_string()))?; 
    let today_prefix = Local::now().format("%Y-%m-%d").to_string();
    // 映射查询结果
    let logs_iter = stmt.query_map([], |row| { 
        let time: String = row.get(0)?;
        // 去掉毫秒部分（.999）
        let time_without_ms = time.split('.').next().unwrap_or(&time);
        // 如果是今天，去掉日期 (前十个字符)
        let formatted_time = if time_without_ms.len() > 10 && time_without_ms.starts_with(&today_prefix){
            time_without_ms[10..].to_string()
        } else if time_without_ms.len() > 5{
            time_without_ms[5..].to_string() // 去掉年份部分（前5个字符 "2026-"）
        }else{
            time_without_ms.to_string()
        };
        
        Ok(LogEntry { 
            time: formatted_time,
            level: row.get(1)?, 
            message: row.get(2)?, 
        }) 
    }).map_err(|e| CommandError(e.to_string()))?; 
  
    // 收集结果
    let mut logs = Vec::new(); 
    for log in logs_iter { 
        logs.push(log.map_err(|e| CommandError(e.to_string()))?); 
    } 
  
    Ok(logs) 
}

// 打开文件夹
#[tauri::command]
pub fn open_app_folder(monitor_manager: State<'_, Arc<MonitorManager>>, id: usize) -> CommandResult<MonitorTargetDto>{
    let target = monitor_manager.get_monitor_config_by_id(id);
    if let Some(target) = target {
        if let MonitorTargetType::LocalProcess(config) = &target.target_type {
            #[cfg(target_os = "windows")]
            {
                Command::new("explorer")
                    .args(["/select,", &config.path]) // 注意这里的逗号
                    .spawn()
                    .unwrap();
            }
        }
        Ok(target)
    } else {
        Err(CommandError(format!("监控目标 {} 不存在", id)))
    }
}

// 删除数据库数据并退出程序
#[tauri::command]
pub fn reset_database(app_handle: AppHandle, monitor_manager: State<'_, Arc<MonitorManager>>) -> Result<(), String> {
    let conn = monitor_manager.get_connection_pool().get().map_err(|e| e.to_string())?;
    // 关闭外键
    conn.execute("PRAGMA foreign_keys = OFF;", []).map_err(|e| e.to_string())?;

    // 找出所有的表 (排除 sqlite_sequence 等系统表)
    let mut stmt = conn.prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'").map_err(|e| e.to_string())?;
    
    let tables: Vec<String> = stmt.query_map([], |row| row.get(0))
        .map_err(|e| e.to_string())?
        .collect::<Result<_, _>>()
        .map_err(|e| e.to_string())?;

    // 循环删除所有表
    for table in tables {
        conn.execute(&format!("DROP TABLE IF EXISTS \"{}\"", table), [])
            .map_err(|e| format!("删除表 {} 失败: {}", table, e))?;
    }
    conn.execute("VACUUM;", []).map_err(|e| e.to_string())?;
    conn.execute("PRAGMA foreign_keys = ON;", []).map_err(|e| e.to_string())?;

    info!("数据库已重置，程序即将退出...");

    // 自行关闭程序
    app_handle.exit(0);
    
    Ok(())
}

// 获取指定目标历史控制台输出
#[tauri::command]
pub fn get_target_console_output(monitor_manager: State<'_, Arc<MonitorManager>>, id: usize) -> CommandResult<Vec<ConsoleMsg>>{
    let outputs = monitor_manager.get_target_console_output(id);
    Ok(outputs)
}
// 更新版本
#[tauri::command]
pub async fn update_self(node_discovery: State<'_, NodeDiscoveryState>) -> CommandResult<()>{
    let mut update_candidates: Vec<NodeInfo> = {
        let nodes = node_discovery.0.lock();
        // 查找版本更高的节点并按时间排序
        let version = env!("CARGO_PKG_VERSION");
        nodes.values()
            .filter(|info| compare_versions(&info.version, version) == std::cmp::Ordering::Greater)
            .cloned()
            .collect()
    };
    update_candidates.sort_by(|a,b|{
        let version_ord = compare_versions(&b.version, &a.version);
        if version_ord != std::cmp::Ordering::Equal {
            return version_ord;
        }
        b.update_time.cmp(&a.update_time)
    });
    for node in update_candidates {
        info!("正在手动更新到版本 {}，尝试向 {} 下载更新包...", node.version, node.ip);    
        let addr = format!("{}:{}", node.ip, node.port);
        // 建立 TCP 连接
        let mut stream = match timeout(Duration::from_secs(10), TcpStream::connect(&addr)).await {
            Ok(Ok(stream)) => {
                // 连接成功
                stream
            }
            Ok(Err(e)) => {
                // 连接失败（非超时原因）
                info!("无法连接到更新节点 {}: {}，尝试更换节点...", node.ip, e);
                continue;
            }
            Err(_) => {
                // 超时
                info!("连接更新节点 {} 超时，尝试更换节点...", node.ip);
                continue;
            }
        };

        match stream.write_all(&UPDATE_REQ_MAGIC).await {
            Ok(_) => {},
            Err(e) => {
                error!("向节点 {} 发送更新请求失败: {}", node.ip, e);
                return Err(CommandError(format!("向节点 {} 发送更新请求失败: {}", node.ip, e)));
            },
        }

        // 请求成功，等待收文件数据
        let mut file_data = Vec::new();
        
        match timeout(Duration::from_secs(60), stream.read_to_end(&mut file_data)).await {
            Ok(Ok(_)) => {
                let update_date: UpdateDate = if let Ok(msg) = bincode::deserialize(&file_data) {
                    msg
                } else {
                    error!("节点 {} 提供的更新数据格式不正确", node.ip);
                    return Err(CommandError(format!("节点 {} 提供的更新数据格式不正确", node.ip)));
                };
                let actual_size = update_date.file_data.len() as u64;
                if actual_size != update_date.file_size {
                    error!(
                        "节点 {} 提供的更新包长度校验失败: expected={}, actual={}",
                        node.ip,
                        update_date.file_size,
                        actual_size
                    );
                    return Err(CommandError(format!(
                        "更新失败，更新包长度校验失败: expected={}, actual={}",
                        update_date.file_size, actual_size
                    )));
                }
                let actual_hash = calculate_sha256(&update_date.file_data);
                if actual_hash != update_date.sha256_hash {
                    error!("节点 {} 提供的更新包哈希校验失败", node.ip);
                    return Err(CommandError("更新失败，更新包哈希校验失败".to_string()));
                }
                // 执行更新
                let mut temp_file = env::temp_dir();
                temp_file.push("ease-proc-update.tmp"); 

                if let Err(e) = tokio::fs::write(&temp_file, &update_date.file_data).await {
                    error!("更新失败，写入临时文件失败: {}", e);
                    return Err(CommandError(format!("更新失败，写入临时文件失败: {}", e)));
                };
                println!("新版本已写入临时文件: {:?}", temp_file);
                // 3. 使用 self_replace 库替换当前的 .exe
                // 这一步是“黑魔法”：它会处理 Windows 的文件锁定，把当前运行的 exe 换掉
                if let Err(e) = self_update::self_replace::self_replace(&temp_file) {
                    error!("更新失败，替换当前程序文件失败: {}", e);
                    return Err(CommandError(format!("更新失败，替换当前程序文件失败: {}", e)));
                };
                info!("更新到 {} 成功，即将重启...", node.version);
                if let Ok(current_exe) = env::current_exe(){
                    if let Ok(_) = Command::new(current_exe).arg("--post-update").spawn() {
                        spawn(async {
                            sleep(Duration::from_secs(3)).await;
                            std::process::exit(0);
                        });
                        return Ok(()); // 这里返回成功，前端会显示更新成功的提示
                    }else {
                        error!("更新成功，但无法启动新版本，自动重启失败");
                        return Err(CommandError(format!("更新成功，但无法启动新版本，自动重启失败")));
                    }
                }else {
                    error!("更新成功，但无法启动新版本，自动重启失败");
                    return Err(CommandError(format!("更新成功，但无法获取当前程序路径，自动重启失败")));
                }
            },
            Ok(Err(e)) => {
                error!("从节点 {} 接收更新数据失败: {}", node.ip, e);
                return Err(CommandError(format!("从节点 {} 接收更新数据失败: {}", node.ip, e)));
            },
            Err(_) => {
                info!("从节点 {} 接收更新数据超时，尝试更换节点...", node.ip);
                continue;
            }
        }
    }
    return Err(CommandError(format!("更新失败，没有可用的更新节点")));
}

// 检查是否有更新版本
#[tauri::command]
pub fn check_update_self(node_discovery: State<'_, NodeDiscoveryState>) -> CommandResult<bool>{
    let nodes = node_discovery.0.lock();
    let version = env!("CARGO_PKG_VERSION");
    let re= nodes.values()
    .any(|info| compare_versions(&info.version, version) == std::cmp::Ordering::Greater);
    Ok(re)
}

#[derive(serde::Serialize)]
pub struct SystemInfo {
    pub hostname: String,
    pub os_name: String,
    pub start_time: String,
    pub cpu_core_count: usize,
    pub physical_core: usize,
    pub total_memory: String,
}

#[derive(Serialize)]
pub struct HostRecord {
    cpu_usage: f32,
    memory_usage: f32,
    disk_usage: f32,
    timestamp: String,
}
