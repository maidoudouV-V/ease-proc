use std::{collections::HashMap, time::Duration};
use std::sync::Arc;
use parking_lot::Mutex;
use r2d2::{Pool, PooledConnection};
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::Connection;
use tauri::AppHandle;
use tokio::spawn;
use tokio::time::sleep;
use tracing::{debug, error, info};
use crate::monitor::ConsoleMsg;
use crate::process_sampler::ProcessSampler;
use crate::{monitor::{MonitorShowInfo, MonitorTarget, MonitorTargetType}, MonitorTargetDto};
use anyhow::{anyhow, Result};

/// 监控管理器，用于统一管理所有监控目标
pub struct MonitorManager {
    /// 存储所有监控目标的映射表
    monitors: Arc<Mutex<HashMap<usize, Arc<Mutex<MonitorTarget>>>>>,
    /// 数据库连接池
    connection_pool: Pool<SqliteConnectionManager>,
    /// Tauri 应用句柄
    app_handle: AppHandle,
    // 本地进程监控数据采样器
    process_sampler: Arc<ProcessSampler>,
}

impl MonitorManager {
    /// 创建新的监控管理器实例
    pub fn new(pool: Pool<SqliteConnectionManager>, app_handle: AppHandle) -> Self {
        Self {
            monitors: Arc::new(Mutex::new(HashMap::new())),
            connection_pool: pool,
            app_handle,
            process_sampler: Arc::new(ProcessSampler::new(Duration::from_secs(1)))
        }
    }

    /// 添加监控目标到管理器
    pub fn add_monitor(&self, id: usize, monitor: Arc<Mutex<MonitorTarget>>) {
        self.monitors.lock().insert(id, monitor);
    }

    /// 从管理器中移除监控目标
    pub async fn remove_monitor(&self, id: usize) -> Result<String> {
        debug!("删除监控项目 {}",id);
        self.disable_monitor(id).await;
        let mut conn =self.connection_pool.get()?;
        let tx = conn.transaction()?;
        // 删除相关配置和记录
        tx.execute("DELETE FROM MonitorTarget WHERE id = ?1", [id])?;
        // 从管理器中移除
        let rm = self.monitors.lock().remove(&id);
        // 提交事务
        tx.commit()?;
        match rm {
            Some(target) => {
                info!("删除监控目标 {}", target.lock().alias);
                Ok("删除成功".to_string())
            },
            None => Err(anyhow!("删除目标不存在")),
        }
    }

    /// 更新指定目标的配置
    pub async fn update_monitor_config(&self, update_target_form: MonitorTargetDto) -> Result<String> {
        let id = update_target_form.id.expect("前端配置错误");
        let exist_target = match self.monitors.lock().get(&id).cloned(){
            Some(target) => target,
            None => return Err(anyhow!("修改的监控目标不存在")),
        };
        // 更新监控目标表
        let conn = self.connection_pool.get()?;
        conn.execute(
            "UPDATE MonitorTarget SET alias = ?1, type = ?2 WHERE id = ?3",
            (&update_target_form.alias, &update_target_form.target_type, id),
        )?;
        let alias = update_target_form.alias.clone();
        // 更新内存中的监控目标
        {
            let mut target = exist_target.lock();
            target.alias = update_target_form.alias;
            target.target_type_cfg = update_target_form.target_type;
        }
        // 如果正在运行，重启监控任务
        if *exist_target.lock().monitor_enabled.read(){
            let app = self.app_handle.clone();
            let sampler = self.process_sampler.clone();
            spawn(async move {
                let mut finished = false;
                let handle_to_wait = exist_target.lock().stop_monitor();
                if let Some(handle) = handle_to_wait{
                    for _ in 0..200 {
                        if handle.is_finished() {
                            finished = true;
                            break;
                        }
                        sleep(Duration::from_millis(50)).await;
                    }
                    if !finished {
                        handle.abort();
                    }
                }
                exist_target.lock().start_monitor(app, sampler);
            });
        }
        info!("已修改监控目标配置 {}", alias);
        Ok("修改成功，正在自动重启监控".to_string())
    }

    /// 开启指定ID的监控目标
    pub async fn enable_monitor(&self, id: usize) -> bool {
        debug!("启动监控项目 {}",id);
        let monitor = {
            self.monitors.lock().get(&id).cloned()
        };
        if let Some(monitor) = monitor {
            self.process_sampler.spawn();
            monitor.lock().start_monitor(self.app_handle.clone(), self.process_sampler.clone());
            // 修改数据库状态
            match self.connection_pool.get() {
                Ok(conn) => {
                    match conn.execute("UPDATE MonitorTarget SET monitor_enabled = 1 WHERE id = ?1", [id])
                                            .map_err(|e| e.to_string()) {
                        Ok(_) => {true},
                        Err(err) => {
                            error!("更新数据库状态失败: {}", err);
                            false
                        },
                    }
                },
                Err(err) => {
                    error!("获取数据库连接失败: {}", err);
                    false
                },
            }
        } else {
            false
        }
    }

    /// 停止指定ID的监控目标
    pub async fn disable_monitor(&self, id: usize) -> bool {
        debug!("停止监控项目 {}",id);
        // 修改数据库状态
        match self.connection_pool.get() {
            Ok(conn) => {
                match conn.execute("UPDATE MonitorTarget SET monitor_enabled = 0 WHERE id = ?1", [id]).map_err(|e| e.to_string()) {
                    Ok(_) => {},
                    Err(err) => {
                        error!("更新数据库状态失败: {}", err);
                        return false;
                    },
                }
            },
            Err(err) => {
                error!("获取数据库连接失败: {}", err);
                return false;
            },
        }
        let monitor;
        if let Some(monitor_arc) = self.monitors.lock().get(&id) {
            monitor = monitor_arc.clone();
        } else {
            // 目标不存在
            return false;
        }
        let is_remote_host = {
            let monitor = monitor.lock();
            matches!(monitor.target_type_cfg, MonitorTargetType::RemoteHost(_))
        };
        let handle_to_wait = monitor.lock().stop_monitor();
        // 检查是否还有运行中的本地进程监控
        if !self.monitors.lock().values().any(|m| {
            let m = m.lock();
            *m.monitor_enabled.read() && matches!(m.target_type_cfg, MonitorTargetType::LocalProcess(_))
        }) {
            // 如果没有本地进程正在运行，关闭采集
            self.process_sampler.stop().await;
        }
        if is_remote_host {
            if let Some(handle) = handle_to_wait {
                handle.abort();
            }
            return true;
        }
        // 超时强行关闭线程
        let mut finished = false;
        if let Some(handle) = handle_to_wait{
            for _ in 0..200 {
                if handle.is_finished() {
                    finished = true;
                    break;
                }
                sleep(Duration::from_millis(50)).await;
            }
            if finished {
                true
            }else {
                false
            }
        }else {
            true
        }
    }

    /// 启动所有已激活的监控目标
    pub async fn start_all_active_monitors(&self) {
        debug!("正在启动所有已激活的监控目标");
        let monitors = self.monitors.lock().clone();
        for (_, monitor) in monitors.iter() {
            if *monitor.lock().monitor_enabled.read() {
                let id = monitor.lock().id;
                self.enable_monitor(id).await;
            }
        }
    }

    /// 获取所有监控目标首页展示信息
    pub async fn get_all_monitor_info(&self) -> Vec<MonitorShowInfo> {
        let monitors = {
            self.monitors.lock().clone()
        };
        let mut infos = Vec::with_capacity(monitors.len());
        for (_id, monitor) in monitors.into_iter() {
            let lock_info = monitor.try_lock();
            if let Some(mut info) = lock_info {
                let info = info.show_info();
                infos.push(info);
            }
        }
        infos.sort_by(|a,b| b.id.cmp(&a.id));
        infos
    }

    // 从数据库获取并初始化全部监控目标列表
    pub fn ini_monitor_targets_from_db(&self) -> Result<()> {
        debug!("正在初始化监控目标");
        let conn = self.connection_pool.get()?;
        let mut stmt = conn.prepare("SELECT * FROM MonitorTarget")?;
        let rows = stmt.query_map([], |row| {
            let s: String = row.get(1)?;
            Ok(MonitorTarget::new(
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
            ))
        })?;
        for row in rows {
            match row {
                Ok(target) => {
                    let target = target?;
                    let id = target.id;
                    let monitor = Arc::new(Mutex::new(target));
                    self.add_monitor(id, monitor);
                },
                Err(err) => {
                    error!("读取监控目标失败: {}", err);
                    continue;
                },
            }
        }
        Ok(())
    }

    pub fn get_connection_pool(&self) -> Pool<SqliteConnectionManager> {
        self.connection_pool.clone()
    }
    
    // 对目标发送控制指令
    pub async fn change_control_signal(&self, id: usize, signal: String) -> bool {
        debug!("对目标 id:{} 发送控制指令 {}", id, signal);
        let monitor_to_update = {
            let monitors = self.monitors.lock();
            monitors.get(&id).cloned() 
        };
        if let Some(monitor) = monitor_to_update {
            *monitor.lock().control_signal.lock() = signal;
            true
        } else {
            false
        }
    }

    // 获取指定ID的监控目标完整信息
    pub fn get_monitor_config_by_id(&self, id: usize) -> Option<MonitorTargetDto> {
        let monitor = {
            let monitors = self.monitors.lock();
            monitors.get(&id).cloned()
        };
        if let Some(monitor) = monitor {
            let lock_info = monitor.lock();
            return Some(lock_info.get_config());
        }
        None
    }

    // 获取指定目标历史控制台输出
    pub fn get_target_console_output(&self, id: usize) -> Vec<ConsoleMsg>{
        let monitor = {
            let monitors = self.monitors.lock();
            monitors.get(&id).cloned()
        };
        if let Some(monitor) = monitor {
            let lock_info = monitor.lock();
            let outputs = lock_info.console_outputs.lock();
            return outputs.iter().cloned().collect();
        }
        vec![]
    }
}
