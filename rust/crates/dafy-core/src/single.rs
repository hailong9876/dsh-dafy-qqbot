//! 单实例：同一时间只允许一个守护进程，避免重复拉起子服务造成端口冲突。

#[cfg(windows)]
mod imp {
    use std::ffi::c_void;
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS};
    use windows_sys::Win32::System::Threading::CreateMutexW;

    pub struct SingleInstance {
        handle: *mut c_void,
    }

    // 句柄只用于持有互斥量，跨线程移动是安全的。
    unsafe impl Send for SingleInstance {}

    impl SingleInstance {
        /// 尝试获取命名互斥量；返回 None 表示已有实例在运行。
        pub fn acquire(name: &str) -> anyhow::Result<Option<Self>> {
            let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
            let handle = unsafe { CreateMutexW(std::ptr::null(), 0, wide.as_ptr()) };
            if handle.is_null() {
                anyhow::bail!("创建互斥量失败: {}", std::io::Error::last_os_error());
            }
            let err = unsafe { GetLastError() };
            if err == ERROR_ALREADY_EXISTS {
                unsafe { CloseHandle(handle) };
                return Ok(None);
            }
            Ok(Some(Self { handle }))
        }
    }

    impl Drop for SingleInstance {
        fn drop(&mut self) {
            if !self.handle.is_null() {
                unsafe { CloseHandle(self.handle) };
            }
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use std::fs::OpenOptions;

    pub struct SingleInstance {
        path: std::path::PathBuf,
    }

    impl SingleInstance {
        pub fn acquire(name: &str) -> anyhow::Result<Option<Self>> {
            let path = std::env::temp_dir().join(format!("{name}.lock"));
            match OpenOptions::new().create_new(true).write(true).open(&path) {
                Ok(_) => Ok(Some(Self { path })),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(None),
                Err(e) => Err(e.into()),
            }
        }
    }

    impl Drop for SingleInstance {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

pub use imp::SingleInstance;
