//! Windows Job Object：把 kimi.exe 绑定进带 KILL_ON_JOB_CLOSE 的作业，
//! 使壳进程以任何方式退出（正常关闭、被强杀、崩溃）时，系统都会级联回收
//! job 内的 kimi.exe 及其启动的所有 MCP 孙进程，避免孤儿进程残留
#![cfg(windows)]

use std::os::windows::io::AsRawHandle;
use std::process::Child;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};

pub(crate) struct Job(HANDLE);

// HANDLE 仅是内核对象的标识值，跨线程传递与共享不涉及数据竞争
unsafe impl Send for Job {}
unsafe impl Sync for Job {}

impl Job {
    pub(crate) fn new() -> Result<Self, String> {
        unsafe {
            let handle = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if handle.is_null() {
                return Err(format!(
                    "CreateJobObjectW 失败: {}",
                    std::io::Error::last_os_error()
                ));
            }
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const _,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            ) == 0
            {
                let err = std::io::Error::last_os_error();
                CloseHandle(handle);
                return Err(format!("SetInformationJobObject 失败: {err}"));
            }
            Ok(Self(handle))
        }
    }

    pub(crate) fn assign(&self, child: &Child) -> Result<(), String> {
        unsafe {
            if AssignProcessToJobObject(self.0, child.as_raw_handle() as HANDLE) == 0 {
                return Err(format!(
                    "AssignProcessToJobObject 失败: {}",
                    std::io::Error::last_os_error()
                ));
            }
        }
        Ok(())
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}
