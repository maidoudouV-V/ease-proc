use rusqlite::{params, Connection};
use serde::Serialize;
use std::sync::mpsc::{channel, Sender};
use std::thread;
use tracing::{Event, Subscriber};
use tracing_subscriber::{layer::Context, Layer};
use chrono::{Duration, Local};

// 日志结构体
#[derive(Debug, Serialize)]
pub struct LogEntry {
    pub time: String,
    pub level: String,
    pub message: String,
}

pub struct SqliteLayer {
    sender: Sender<LogEntry>,
}

impl SqliteLayer {
    pub fn new(db_path: String) -> Self {
        let (tx, rx) = channel::<LogEntry>();
        // 启动后台线程
        thread::spawn(move || {
            let conn = Connection::open(db_path).expect("无法打开日志数据库");
            let mut counter = 0;
            // 配置：每写入 100 条日志检查一次清理
            const CLEANUP_INTERVAL: usize = 100;
            // 配置：只保留最近 30 天的日志
            const RETENTION_DAYS: i64 = 30;
            while let Ok(entry) = rx.recv() {
                conn.execute(
                    "INSERT INTO SystemLog (log_time, log_level, log_message) VALUES (?1, ?2, ?3)",
                    params![entry.time, entry.level, entry.message],
                ).unwrap_or_else(|e| {eprintln!("写入日志失败: {}", e);0});
                counter += 1;
                // 触发清理逻辑
                if counter >= CLEANUP_INTERVAL {
                    counter = 0;
                    // 计算截止日期
                    let threshold_date = Local::now() - Duration::days(RETENTION_DAYS);
                    let threshold_str = threshold_date.format("%Y-%m-%d %H:%M:%S%.3f").to_string();
                    if let Err(e) = conn.execute(
                        "DELETE FROM SystemLog WHERE log_time < ?1",
                        params![threshold_str],
                    ) {
                        eprintln!("清理过期日志失败: {}", e);
                    }
                }
            }
        });

        SqliteLayer { sender: tx }
    }
}

impl<S> Layer<S> for SqliteLayer
where
    S: Subscriber,
{
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let meta = event.metadata();
        let level = meta.level().to_string();
        let time = Local::now().format("%Y-%m-%d %H:%M:%S%.3f").to_string();
        let mut visitor = MessageVisitor::default();
        event.record(&mut visitor);

        let log_entry = LogEntry {
            time,
            level,
            message: visitor.message,
        };
        let _ = self.sender.send(log_entry);
    }
}

#[derive(Default)]
struct MessageVisitor {
    message: String,
}

impl tracing::field::Visit for MessageVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.message = format!("{:?}", value);
        }
    }
    
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
         if field.name() == "message" {
            self.message = value.to_string();
        }
    }
}