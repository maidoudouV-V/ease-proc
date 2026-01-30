use std::mem;
use std::os::windows::io::RawHandle;
use std::ptr;
use std::sync::OnceLock;

// 引入 winapi 的相关定义
use winapi::shared::minwindef::{DWORD, LPVOID};
use winapi::um::handleapi::CloseHandle;
use winapi::um::jobapi2::{AssignProcessToJobObject, CreateJobObjectW, SetInformationJobObject};
use winapi::um::winnt::{
    HANDLE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JobObjectExtendedLimitInformation,
};

/// 内部结构体：用于包装 Windows 句柄，实现自动关闭和线程安全
struct JobHandle(HANDLE);

// 必须标记为 Send + Sync，因为我们会把它放在全局静态变量中
unsafe impl Send for JobHandle {}
unsafe impl Sync for JobHandle {}

impl Drop for JobHandle {
    fn drop(&mut self) {
        unsafe {
            // 虽然进程退出时系统会自动回收，但显式关闭是好习惯
            if !self.0.is_null() {
                CloseHandle(self.0);
            }
        }
    }
}

// 全局单例 Job Object
// 使用 OnceLock 确保整个程序生命周期内只创建一个 Job Object
static GLOBAL_JOB: OnceLock<JobHandle> = OnceLock::new();

/// 获取或创建全局 Job Object
fn get_global_job() -> Option<&'static JobHandle> {
    GLOBAL_JOB.get_or_init(|| {
        unsafe {
            // 1. 创建 Job Object
            // Win7 兼容：CreateJobObjectW 自 Windows 2000 起可用
            let handle = CreateJobObjectW(ptr::null_mut(), ptr::null());
            
            if handle.is_null() {
                eprintln!("ProcessGuard: Failed to create Job Object.");
                return JobHandle(ptr::null_mut());
            }

            // 2. 配置 Job：当 Job 句柄关闭时，自动终止所有关联进程
            // Win7 兼容：JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE 完全支持
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;

            let info_ptr = &mut info as *mut _ as LPVOID;
            let info_size = mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as DWORD;

            let result = SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                info_ptr,
                info_size,
            );

            if result == 0 {
                eprintln!("ProcessGuard: Failed to set Job Object extended limit.");
                // 即使设置失败，我们也不返回空，只是这个 Job 可能没有“自动杀进程”的功能
            }

            JobHandle(handle)
        }
    }).into()
}

/// 将进程添加到全局 Job Object 中，确保主程序退出时该进程也会被终止。
///
/// 接受 RawHandle 类型，支持 std::process::Child 和 tokio::process::Child
pub fn add_to_job(handle: RawHandle) {
    let job_wrapper = match get_global_job() {
        Some(wrapper) if !wrapper.0.is_null() => wrapper,
        _ => return, // Job 创建失败，跳过
    };

    unsafe {
        // 获取子进程的原始句柄
        // RawHandle 是 *mut c_void，可以直接转换为 HANDLE
        let process_handle = handle as HANDLE;

        // 3. 将进程关联到 Job
        let result = AssignProcessToJobObject(job_wrapper.0, process_handle);
        
        if result == 0 {
            // 常见失败原因：
            // 1. 程序在 VSCode/Debugger 中运行 (Debugger 已经把进程放入了一个 Job)
            // 2. 权限不足
            // 在生产环境 release 包中通常不会失败。
            // eprintln!("ProcessGuard: Failed to assign process to job.");
        }
    }
}