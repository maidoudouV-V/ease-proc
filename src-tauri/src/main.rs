// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
use anyhow::{anyhow, Result};

use logging::SqliteLayer;
use monitor::MonitorTargetType;
use monitor_manager::MonitorManager;
use parking_lot::{Mutex, RwLock};
use rusqlite::Connection;
use tauri_plugin_autostart::MacosLauncher;
use tokio::spawn;
use tracing::{debug, info};
use tracing_subscriber::fmt::time::ChronoLocal;
use tracing_subscriber::{fmt, EnvFilter};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use std::{env, fs};
use std::iter::Filter;
use std::{collections::HashMap, sync::Arc};

use chrono::Local;
use tauri::Manager;
use std::time::Duration;
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use tokio::time::sleep;
use tauri::{CustomMenuItem, SystemTray, SystemTrayMenu, SystemTrayEvent, WindowEvent};

use crate::monitor::MonitorTarget;
use crate::commands::*;

mod commands;
mod monitor;
mod local_process_monitor;
mod local_host_monitor;
mod monitor_manager;
mod logging;
mod process_guard;
mod process_sampler;

fn main() {
    println!("初始化应用...");
    // 1. 获取当前 exe 的完整路径
    if let Ok(current_exe) = env::current_exe() {
        // 2. 获取 exe 所在的目录
        if let Some(exe_dir) = current_exe.parent() {
            // 3. 强制修改当前进程的工作目录
            if let Err(e) = env::set_current_dir(exe_dir) {
                eprintln!("无法更改工作目录: {}", e);
            } else {
                println!("工作目录已切换至: {:?}", exe_dir);
            }
        }
    }

    // 定义托盘菜单
    let quit = CustomMenuItem::new("quit".to_string(), "退出");
    let tray_menu = SystemTrayMenu::new().add_item(quit);
    // 创建系统托盘
    let system_tray = SystemTray::new()
    .with_tooltip("进程监控工具")
    .with_menu(tray_menu);

    tauri::Builder::default()
    // 单实例运行
    .plugin(tauri_plugin_single_instance::init(|app, argv, cwd| {
        let window = app.get_window("main").unwrap();
        window.show().unwrap();
        window.set_focus().unwrap();
        app.emit_all("single-instance", Payload { args: argv, cwd }).unwrap();
    }))
    // 注册开机自启插件
    .plugin(tauri_plugin_autostart::init(MacosLauncher::LaunchAgent, Some(vec![]))) 
    // 系统托盘
    .system_tray(system_tray)
    // 处理托盘事件
    .on_system_tray_event(|app, event| match event {
        SystemTrayEvent::LeftClick { .. } => {
            let window = app.get_window("main").unwrap();
            // 永远：恢复显示 + 解除最小化 + 聚焦
            window.show().unwrap();
            if window.is_minimized().unwrap_or(false) {
                window.unminimize().unwrap();
            }
            window.set_focus().unwrap();
        }            
        SystemTrayEvent::MenuItemClick { id, .. } => {
            match id.as_str() {
                "quit" => app.exit(0), // 彻底退出
                "show" => {
                    let window = app.get_window("main").unwrap();
                    window.show().unwrap();
                    window.set_focus().unwrap();
                }
                _ => {}
            }
        }
        _ => {}
    })
    .setup(|app| {
        // 初始化数据库
        let db_conn_pool = init_db_pool().unwrap();
        let db_conn = db_conn_pool.get().unwrap();
        init_db_table(&db_conn)?;
        // 初始化日志系统
        let sqlite_layer = SqliteLayer::new("database.db".to_string());
        // 创建过滤器
        let filter_layer = EnvFilter::new("error,ease_proc=debug");
        // 创建格式化输出层
        let timer = ChronoLocal::new("%Y-%m-%d %H:%M:%S%.3f".to_string());
        let fmt_layer = fmt::layer()
            .with_target(false) // 是否显示日志来源的模块路径
            .with_level(true) // 是否显示 INFO/ERROR 等等级标签
            .with_timer(timer); // 自定义时间格式
        // 注册并初始化
        tracing_subscriber::registry()
            .with(filter_layer) // 叠加上过滤器
            .with(fmt_layer)    // 叠加上输出格式
            .with(sqlite_layer) // 叠加上 SQLite 日志层
            .init();
        debug!("应用启动中，初始化管理对象...");
        // 目标管理对象
        let monitor_manager = MonitorManager::new(db_conn_pool, app.handle().clone());
        // 获取监控目标
        monitor_manager.ini_monitor_targets_from_db()?;
        // 默认添加本地监控
        let local_host_monitor = MonitorTarget::new(0, MonitorTargetType::LocalHost, "本机监控".to_string(), true).unwrap();
        monitor_manager.add_monitor(0, Arc::new(Mutex::new(local_host_monitor)));
        let monitor_manager = Arc::new(monitor_manager);
        // 将状态放入 Tauri 的管理中
        app.manage(monitor_manager.clone());
        // 定时推送基础信息
        let app_handle = app.handle();
        let monitor_manager_arc = monitor_manager.clone();
        tauri::async_runtime::spawn(async move {
            loop {
                // 向前端推送事件
                app_handle.emit_all("monitor-info-update", monitor_manager_arc.get_all_monitor_info().await).unwrap();
                sleep(Duration::from_secs(2)).await;
            }
        });
        tauri::async_runtime::spawn(async move {
            monitor_manager.start_all_active_monitors().await;
        });
        info!("======应用已启动======");
        Ok(())
    })
    .invoke_handler(tauri::generate_handler![
        get_system_info,
        refresh_monitor_targets,
        enable_monitor,
        add_monitor_target,
        disable_monitor,
        delete_monitor_target,
        update_monitor_target,
        send_control_signal,
        get_monitor_full_config,
        get_app_logs,
        open_app_folder,
        reset_database,
        get_target_console_output 
    ])
    .run(tauri::generate_context!())
    .unwrap_or_else(|err|{
        let _ = fs::write("ErrorLog.txt", format!("{} : 程序启动失败，错误原因：{}", Local::now().format("%Y-%m-%d %H:%M:%S%"), err));
        std::process::exit(1);
    });

}

// 数据库初始化
fn init_db_pool()->Result<Pool<SqliteConnectionManager>, Box<dyn std::error::Error>> {
    let manager = SqliteConnectionManager::file("database.db");
    let pool = Pool::builder().max_size(5).build(manager)?;
    Ok(pool)
}
fn init_db_table(conn: &Connection) -> Result<()>{
    // 创建监控目标表
    conn.execute(
        "CREATE TABLE IF NOT EXISTS MonitorTarget (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            type TEXT NOT NULL,
            alias TEXT,
            monitor_enabled INTEGER
        )",
        (),
    )?;
    
    // 创建日志表
    conn.execute(
        "CREATE TABLE IF NOT EXISTS SystemLog (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            log_time TEXT,
            log_level TEXT,
            log_message TEXT
        )",
        (),
    )?;
    Ok(())
}

#[derive(Clone, serde::Serialize)]
struct Payload {
    args: Vec<String>,
    cwd: String,
}