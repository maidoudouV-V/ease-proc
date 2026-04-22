// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
use anyhow::{Result};
use logging::SqliteLayer;
use monitor::MonitorTargetType;
use monitor_manager::MonitorManager;
use parking_lot::{Mutex};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use socket2::SockRef;
use sysinfo::{Networks};
use tauri_plugin_autostart::MacosLauncher;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tracing::{debug, error, info};
use tracing_subscriber::fmt::time::ChronoLocal;
use tracing_subscriber::{fmt, EnvFilter};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use uuid::Uuid;
use std::net::{IpAddr, Ipv4Addr, SocketAddrV4};
use std::{env, fs};
use std::{collections::HashMap, sync::Arc};
use std::os::windows::io::AsRawSocket;
use chrono::Local;
use tauri::Manager;
use std::time::Duration;
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use tokio::time::{sleep, timeout};
use tauri::{CustomMenuItem, SystemTray, SystemTrayMenu, SystemTrayEvent};
use winapi::um::handleapi::SetHandleInformation;
use winapi::um::winbase::HANDLE_FLAG_INHERIT;
use winapi::um::winnt::HANDLE;

use crate::monitor::MonitorTarget;
use crate::commands::*;

mod commands;
mod monitor;
mod local_process_monitor;
mod local_host_monitor;
mod remote_host_monitor;
mod monitor_manager;
mod logging;
mod process_guard;
mod process_sampler;

fn main() {
    // 1. 拦截更新后的重启参数
    let args: Vec<String> = env::args().collect();
    if args.contains(&String::from("--post-update")) {
        println!("检测到更新重启，等待旧进程退出...");
        // 阻塞主线程 2 秒钟。
        std::thread::sleep(Duration::from_millis(2000));
    }
     println!("初始化应用... 当前版本: {}", env!("CARGO_PKG_VERSION"));
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
        if let Err(e) = app.emit_all("single-instance", Payload { args: argv, cwd }) {
            error!("发送 single-instance 事件失败: {}", e);
        }
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
                "quit" => {
                    // 彻底退出
                    app.exit(0)
                }
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
                if let Err(e) = app_handle.emit_all("monitor-info-update", monitor_manager_arc.get_all_monitor_info().await) {
                    error!("发送 monitor-info-update 事件失败: {}", e);
                }
                sleep(Duration::from_secs(2)).await;
            }
        });
        tauri::async_runtime::spawn(async move {
            monitor_manager.start_all_active_monitors().await;
        });
        info!("======应用已启动======");

        // 节点发现和更新机制
        let discovered_nodes = Arc::new(Mutex::new(HashMap::<String, NodeInfo>::new()));
        app.manage(NodeDiscoveryState(discovered_nodes.clone()));
        let app_handle = app.handle();
        // 节点通讯线程
        tauri::async_runtime::spawn(async move {
            // 初始化节点
            let machine_id =get_or_init_uuid(&db_conn).expect("无法获取uuid");
            println!("应用唯一ID: {}", machine_id);
            let tcp_connect_timeout = Duration::from_secs(3);
            let tcp_write_timeout = Duration::from_secs(5);
            // 组播地址和端口
            let multicast_addr = Ipv4Addr::new(239, 30, 4, 71);
            let udp_listen_port = 56311;
            let tcp_listen_port = 56311;
        
            // 绑定所有使用到的端口
            let socket = match UdpSocket::bind(format!("0.0.0.0:{}", udp_listen_port)).await {
                Ok(s) => {s},
                Err(e) => {
                    eprintln!("无法绑定 UDP 端口 {}: {}", udp_listen_port, e);
                    return;
                }
            };
            let listener = match TcpListener::bind(format!("0.0.0.0:{}", tcp_listen_port)).await {
                Ok(l) => {l},
                Err(e) => {
                    eprintln!("无法绑定 TCP 端口 {}: {}", tcp_listen_port, e);
                    return;
                }
            };

            // 禁止网络句柄的“可继承”属性
            unsafe {
                // 将 UDP Socket 的底层句柄转换为 winapi 需要的 HANDLE 类型
                let udp_handle = socket.as_raw_socket() as HANDLE;
                // 参数解释：句柄，要修改的标志位（继承标志），新的值（0 表示清除该标志）
                let result_udp = SetHandleInformation(udp_handle, HANDLE_FLAG_INHERIT, 0);
                if result_udp == 0 {
                    eprintln!("警告: 无法清除 UDP 句柄的继承标志");
                }
                let tcp_handle = listener.as_raw_socket() as HANDLE;
                let result_tcp = SetHandleInformation(tcp_handle, HANDLE_FLAG_INHERIT, 0);
                if result_tcp == 0 {
                    eprintln!("警告: 无法清除 TCP 句柄的继承标志");
                }
            }

            // 监听其它节点发来的TCP请求
            let discovery_state = discovered_nodes.clone();
            let tcp_listener_handle = tokio::spawn(async move {
                loop {
                    match listener.accept().await {
                        Ok((mut socket, addr)) => {
                            let discovery_state = discovery_state.clone();
                            tokio::spawn(async move {
                                // 读取客户端发来的指令
                                let mut buf = [0; 4096];
                                match timeout(Duration::from_secs(5), socket.read(&mut buf)).await {
                                    Ok(Ok(n)) => {
                                        if n == 0 {
                                            return;
                                        }
                                        // 如果是更新请求
                                        if buf.starts_with(UPDATE_REQ_MAGIC) {
                                            // 读取本地 .exe 文件并发送
                                            if let Ok(current_exe) = env::current_exe() {
                                                if let Ok(file_data) = tokio::fs::read(&current_exe).await{
                                                    let file_size = file_data.len() as u64;
                                                    let sha256_hash = calculate_sha256(&file_data);
                                                    let node_message = UpdateDate{
                                                        version: env!("CARGO_PKG_VERSION").to_string(),
                                                        file_data,
                                                        file_size,
                                                        sha256_hash,
                                                    };
                                                    let encoded: Vec<u8> = bincode::serialize(&node_message).expect("序列化失败");
                                                    let _ = timeout(tcp_write_timeout, socket.write_all(&encoded)).await;
                                                    println!("已响应 {} 的更新请求", addr);
                                                }else {
                                                    eprintln!("无法读取当前可执行文件内容，无法响应更新请求");
                                                }
                                            }else {
                                                eprintln!("无法获取当前可执行文件路径，无法响应更新请求");
                                            }
                                        } else {
                                            // 消息结构
                                            let node_message: NodeMessage = if let Ok(msg) = bincode::deserialize(&buf[..n]) {
                                                msg
                                            } else {
                                                eprintln!("无法反序列化节点消息");
                                                return;
                                            };
                                            match node_message {
                                                // 如果是发现新节点
                                                NodeMessage::RequestMessage(RequestMessageType::NewNodeMessage(mut new_node)) => {
                                                    let mut nodes = discovery_state.lock();
                                                    new_node.ip = addr.ip().to_string();
                                                    new_node.update_time = Local::now().timestamp().to_string();
                                                    if nodes.len() < 1000{
                                                        if let None = nodes.insert(new_node.dev_id.clone(), new_node){
                                                            println!("收到来自 {} 的TCP响应 已注册新节点，当前节点数: {}", addr.ip().to_string(), nodes.len());
                                                        }
                                                    }
                                                },
                                                _ => {
                                                    return ;
                                                }
                                            }
                                        }
                                    }
                                    Ok(Err(e)) => {
                                        eprintln!("读取请求失败: {}", e);
                                        return;
                                    }
                                    Err(_) => {
                                        eprintln!("读取请求超时");
                                        return;
                                    }
                                }
                            });
                        },
                        Err(e) => {
                            println!("接受客户端连接失败: {}", e);
                            sleep(Duration::from_secs(10)).await;
                        }
                    };
                }
            });

            // 加入组播组
            for (name, data) in &Networks::new_with_refreshed_list(){
                for ip_network in data.ip_networks() {
                    match ip_network.addr {
                        IpAddr::V4(ipv4) =>{
                            if ipv4.is_loopback() {
                                continue;
                            }
                            if let Err(e) = socket.join_multicast_v4(multicast_addr, ipv4) {
                                eprintln!("在网卡 {} {} 加入组播组失败: {}", name, ipv4, e);
                                continue;
                            }
                            println!("在网卡 {} {} 加入组播组...", name, ipv4);
                        },
                        IpAddr::V6(_) => continue,
                    }
                }
            }

            let socket_receiver = Arc::new(socket);
            let socket_sender = socket_receiver.clone();
            // 广播自身信息
            let uid = machine_id.clone();
            let sender_handle = tokio::spawn(async move{
                let version = env!("CARGO_PKG_VERSION");
                let networks = Networks::new_with_refreshed_list();
                loop{
                    for (name, data) in &networks {
                        for ip_network in data.ip_networks() {
                            match ip_network.addr {
                                IpAddr::V4(ipv4) => {
                                    // 排除回环地址 127.0.0.1
                                    if ipv4.is_loopback() {
                                        continue;
                                    }
                                    // 执行发送逻辑
                                    let my_info = NodeInfo {
                                        dev_id: uid.clone(),
                                        ip: ipv4.to_string(),
                                        port: tcp_listen_port,
                                        version: version.to_string(),
                                        update_time: 0.to_string(),
                                    };
                                    let encoded: Vec<u8> = bincode::serialize(&my_info).expect("序列化失败");
                                    let target = SocketAddrV4::new(multicast_addr, udp_listen_port);
                                    // 设置从指定网卡发出去
                                    let sock_ref = SockRef::from(socket_sender.as_ref());
                                    if let Err(_e) = sock_ref.set_multicast_if_v4(&ipv4) {
                                        // 可能不支持组播
                                        continue;
                                    }
                                    let _ = socket_sender.send_to(
                                        &encoded, 
                                        &target
                                    ).await;
                                    println!("正在从网卡 {} {} 发送广播...", name, ipv4);
                                },
                                IpAddr::V6(_) => continue,
                            }
                        }
                    }
                    sleep(Duration::from_secs(3600)).await
                }
            });
            // 监听局域网广播信息
            let discovery_state = discovered_nodes.clone();
            let receiver_handle = tokio::spawn(async move {
                let version = env!("CARGO_PKG_VERSION");
                let mut buf = [0u8; 1024];
                loop {
                    // 等待接收数据
                    match socket_receiver.recv_from(&mut buf).await {
                        Ok((len, addr)) => {
                            let data = &buf[..len];
                            if let Ok(mut info) = bincode::deserialize::<NodeInfo>(data) {
                                // 过滤掉自己发送的广播
                                if info.dev_id == machine_id{
                                    continue;
                                }

                                let app_handle = app_handle.clone();
                                let discovery_state = discovery_state.clone();
                                // 验证连接并告诉对方自己的信息
                                let my_dev_id = machine_id.clone();
                                tokio::spawn(async move {
                                    if let Ok(Ok(mut stream)) = timeout(tcp_connect_timeout, TcpStream::connect(&addr)).await{
                                        let my_info = NodeMessage::RequestMessage(RequestMessageType::NewNodeMessage(NodeInfo {
                                             dev_id: my_dev_id, 
                                             ip: 0.to_string(),
                                             port: tcp_listen_port, 
                                             version: version.to_string(), 
                                             update_time: 0.to_string()}));
                                        let encoded: Vec<u8> = bincode::serialize(&my_info).expect("序列化失败");
                                        match timeout(tcp_write_timeout, stream.write_all(&encoded)).await {
                                            Ok(Ok(_)) => {
                                                // 如果是新版本，前端展示更新按钮
                                                if compare_versions(&info.version, version) == std::cmp::Ordering::Greater {
                                                    if let Err(e) = app_handle.emit_all("hasUpdate", ()) {
                                                        error!("发送 hasUpdate 事件失败: {}", e);
                                                    }
                                                }
                                                // 将节点信息存储到共享状态中
                                                info.ip = addr.ip().to_string();
                                                info.update_time = Local::now().timestamp().to_string();
                                                let mut nodes = discovery_state.lock();
                                                if nodes.len() < 1000{
                                                    let dev_id = info.dev_id.clone();
                                                    if let None = nodes.insert(info.dev_id.clone(), info) {
                                                        println!("收到来自 {} 的广播 已注册新节点 {}，当前节点数: {}", addr, dev_id, nodes.len());
                                                    };
                                                }
                                            },
                                            Ok(Err(e)) => {
                                                println!("无法验证节点连接 {}: {}", addr, e);
                                            },
                                            Err(_) => {
                                                eprintln!("向节点 {} 发送验证信息超时", addr);
                                            }
                                        };
                                    } else {
                                        eprintln!("连接节点 {} 超时或失败", addr);
                                    }
                                });
                            }
                        }
                        Err(e) => eprintln!("接收出错: {}", e),
                    }
                }
            });
            // 清理长时间没有心跳的节点
            let discovery_state = discovered_nodes.clone();
            tokio::spawn(async move{
                loop{
                    sleep(Duration::from_secs(600)).await;
                    let mut nodes = discovery_state.lock();
                    let now = Local::now().timestamp();
                    nodes.retain(|_, info| {
                        let last_update = info.update_time.parse::<i64>().unwrap_or(0);
                        now - last_update < 7200 // 2小时没有更新就认为掉线了
                    });
                }
            });
            let _ = tokio::join!(receiver_handle, sender_handle, tcp_listener_handle);
        });

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
        get_target_console_output,
        update_self,
        check_update_self
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

    // 创建系统配置表
    conn.execute(
        "CREATE TABLE IF NOT EXISTS AppConfig (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            config_key TEXT,
            config_value TEXT
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

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct NodeInfo {
    dev_id: String,
    ip: String,
    port: u16,
    version: String,
    update_time: String,
}

// 节点列表
pub struct NodeDiscoveryState(pub Arc<Mutex<HashMap<String, NodeInfo>>>);

pub fn compare_versions(a: &str, b: &str) -> std::cmp::Ordering {
    let a_parts = match parse_version_parts(a) {
        Some(parts) => parts,
        None => {
            eprintln!("版本号解析失败，已跳过比较: a={}", a);
            return std::cmp::Ordering::Equal;
        }
    };
    let b_parts = match parse_version_parts(b) {
        Some(parts) => parts,
        None => {
            eprintln!("版本号解析失败，已跳过比较: b={}", b);
            return std::cmp::Ordering::Equal;
        }
    };
    a_parts.cmp(&b_parts)
}

fn parse_version_parts(version: &str) -> Option<Vec<u32>> {
    let mut parts = Vec::new();
    for part in version.split('.') {
        match part.parse::<u32>() {
            Ok(n) => parts.push(n),
            Err(e) => {
                eprintln!("版本号字段解析失败: version={}, field={}, err={}", version, part, e);
                return None;
            }
        }
    }
    Some(parts)
}

/// 请求更新指令
const UPDATE_REQ_MAGIC: &[u8] = b"EP_UPDATE_V1:REQUEST";

// 节点之间的通讯消息
#[derive(Serialize, Deserialize, Debug)]
pub enum NodeMessage{
    RequestMessage(RequestMessageType),
    ResponseMessage(ResponseMessageType),
}
/// 请求消息
#[derive(Serialize, Deserialize, Debug)]
pub enum RequestMessageType{
    /// 新节点信息
    NewNodeMessage(NodeInfo),
    /// 下线通知
    ExitNode(String), // 设备ID
    /// 自定义消息
    CustomMessage(String),
}
/// 响应消息
#[derive(Serialize, Deserialize, Debug)]
pub enum ResponseMessageType{
    /// 错误信息
    ErrorMessage(String),
    /// 自定义消息
    CustomMessage(String),
}
#[derive(Serialize, Deserialize, Debug)]
struct UpdateDate {
    version: String,
    file_data: Vec<u8>,
    file_size: u64,
    sha256_hash: String,
}

fn calculate_sha256(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    format!("{:x}", hasher.finalize())
}

const UUID_KEY: &str = "app_uuid";
pub fn get_or_init_uuid(conn: &Connection) -> rusqlite::Result<String> {
    // 尝试读取
    let mut stmt = conn.prepare(
        "SELECT config_value FROM AppConfig WHERE config_key = ?1 LIMIT 1"
    )?;
    let result: rusqlite::Result<String> =
        stmt.query_row(params![UUID_KEY], |row| row.get(0));

    if let Ok(uuid) = result {
        return Ok(uuid);
    }
    // 不存在则生成
    let uuid = Uuid::new_v4().to_string();
    conn.execute(
        "INSERT INTO AppConfig (config_key, config_value) VALUES (?1, ?2)",
        params![UUID_KEY, uuid],
    )?;
    Ok(uuid)
}
