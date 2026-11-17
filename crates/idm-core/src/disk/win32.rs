//! 唯一允许 `unsafe` 的子模块：对 Win32 API 的薄封装。
//!
//! 本文件顶部 `#![allow(unsafe_code)]` 是本阶段唯一被允许的 `#[allow]`。所有 FFI
//! 调用都被封装成安全函数，其它子模块不得直接调用 windows-sys。每个 `unsafe` 块上方
//! 都有对应的 `// SAFETY:` 注释。

#![allow(unsafe_code)]

use std::ffi::c_void;
use std::fs::File;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::AsRawHandle;
use std::path::Path;

use windows_sys::Win32::Foundation::{
    FALSE, HANDLE, HANDLE_FLAG_INHERIT, SetHandleInformation, SetLastError,
};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ALLOCATION_INFO, FILE_ATTRIBUTE_SPARSE_FILE, FILE_STANDARD_INFO, FileAllocationInfo,
    FileStandardInfo, GetCompressedFileSizeW, GetDiskFreeSpaceExW, GetFileAttributesW,
    GetFileInformationByHandleEx, GetVolumeInformationW, GetVolumePathNameW, MOVEFILE_COPY_ALLOWED,
    MOVEFILE_WRITE_THROUGH, MoveFileExW, SetFileInformationByHandle,
};
use windows_sys::Win32::System::Console::{
    GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
};
use windows_sys::Win32::System::IO::{DeviceIoControl, OVERLAPPED};
use windows_sys::Win32::System::Ioctl::FSCTL_SET_SPARSE;
use windows_sys::core::PCWSTR;

use crate::error::{Error, Result};

const INVALID_FILE_ATTRIBUTES: u32 = 0xffff_ffff;

/// 把路径编码为 null 结尾的 UTF-16，供 Win32 宽字符 API 使用。
fn to_wide(path: &Path) -> Vec<u16> {
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    wide.push(0);
    wide
}

/// 把 `last_os_error()` 包装成带上下文的 [`Error::Io`]。
fn io_err(operation: &'static str, path: &Path, offset: u64) -> Error {
    Error::Io {
        operation,
        path: path.to_path_buf(),
        offset,
        source: std::io::Error::last_os_error(),
    }
}

/// 查询目标卷可用空间（字节）。创建文件前用于判断空间是否充足。
///
/// `GetDiskFreeSpaceExW` 要求路径必须存在，因此传入尚未创建的文件路径会失败。
/// 这里统一查询 **父目录**（创建场景里父目录必然存在），既能覆盖"文件尚未存在"
/// 的 `create` 前置检查，也兼容传入已有目录/文件的场景。相对路径（如 `x.bin`）的
/// 父目录为空，退化为当前目录 `.`。
pub(crate) fn get_disk_free_space(path: &Path) -> Result<u64> {
    let parent = path.parent().unwrap_or(path);
    let query = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };
    let wide = to_wide(query);
    let mut free: u64 = 0;
    // SAFETY: `wide` 是 null 结尾的有效 UTF-16 字符串，调用期间存活；`free` 是
    // 有效的 u64 输出参数；其余两个指针为 null（该 API 允许传 null）。
    let ret = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr() as PCWSTR,
            &mut free as *mut u64,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if ret == FALSE {
        return Err(io_err("GetDiskFreeSpaceExW", query, 0));
    }
    Ok(free)
}

/// 通过 `SetFileInformationByHandle(FileAllocationInfo)` 预留物理分配大小。
pub(crate) fn set_file_allocation(file: &File, path: &Path, size: u64) -> Result<()> {
    let alloc = i64::try_from(size).map_err(|_| Error::Io {
        operation: "set_file_allocation size overflow",
        path: path.to_path_buf(),
        offset: size,
        source: io::Error::other("size exceeds i64::MAX"),
    })?;
    let info = FILE_ALLOCATION_INFO {
        AllocationSize: alloc,
    };
    let handle = file.as_raw_handle() as HANDLE;
    // SAFETY: `info` 是栈上的固定大小结构，调用期间有效；长度按结构大小给出。
    let ret = unsafe {
        SetFileInformationByHandle(
            handle,
            FileAllocationInfo,
            &info as *const FILE_ALLOCATION_INFO as *const c_void,
            std::mem::size_of::<FILE_ALLOCATION_INFO>() as u32,
        )
    };
    if ret == FALSE {
        return Err(Error::Io {
            operation: "SetFileInformationByHandle(FileAllocationInfo)",
            path: path.to_path_buf(),
            offset: size,
            source: std::io::Error::last_os_error(),
        });
    }
    Ok(())
}

/// 通过 `DeviceIoControl(FSCTL_SET_SPARSE)` 把文件标记为稀疏文件。
pub(crate) fn set_sparse(file: &File, path: &Path) -> Result<()> {
    let handle = file.as_raw_handle() as HANDLE;
    let mut bytes_returned: u32 = 0;
    // SAFETY: FSCTL_SET_SPARSE 不需要输入/输出缓冲区；`lpOverlapped` 传 null 表示
    // 同步完成。所有指针均为 null 或有效的栈变量。
    let ret = unsafe {
        DeviceIoControl(
            handle,
            FSCTL_SET_SPARSE,
            std::ptr::null(),
            0,
            std::ptr::null_mut(),
            0,
            &mut bytes_returned as *mut u32,
            std::ptr::null_mut::<OVERLAPPED>(),
        )
    };
    if ret == FALSE {
        return Err(Error::Io {
            operation: "DeviceIoControl(FSCTL_SET_SPARSE)",
            path: path.to_path_buf(),
            offset: 0,
            source: std::io::Error::last_os_error(),
        });
    }
    Ok(())
}

/// 通过 `GetFileInformationByHandleEx(FileStandardInfo)` 读取物理分配大小。
pub(crate) fn get_allocation_size(file: &File, path: &Path) -> Result<u64> {
    let handle = file.as_raw_handle() as HANDLE;
    // SAFETY: FILE_STANDARD_INFO 是固定大小结构，零初始化后由下面的调用填充。
    let mut info: FILE_STANDARD_INFO = unsafe { std::mem::zeroed() };
    // SAFETY: `info` 是栈上的固定大小结构，调用期间有效；长度按结构大小给出。
    let ret = unsafe {
        GetFileInformationByHandleEx(
            handle,
            FileStandardInfo,
            &mut info as *mut FILE_STANDARD_INFO as *mut c_void,
            std::mem::size_of::<FILE_STANDARD_INFO>() as u32,
        )
    };
    if ret == FALSE {
        return Err(Error::Io {
            operation: "GetFileInformationByHandleEx(FileStandardInfo)",
            path: path.to_path_buf(),
            offset: 0,
            source: std::io::Error::last_os_error(),
        });
    }
    Ok(info.AllocationSize as u64)
}

/// 通过 `GetCompressedFileSizeW` 读取文件实际占用的磁盘字节数（含压缩/稀疏）。
pub(crate) fn get_compressed_file_size(path: &Path) -> Result<u64> {
    let wide = to_wide(path);
    let mut high: u32 = 0;
    // SAFETY: `SetLastError(0)` 仅设置线程局部的 Win32 错误码，不涉及任何指针操作。
    unsafe { SetLastError(0) };
    // SAFETY: `wide` 是 null 结尾的有效 UTF-16 字符串，调用期间存活；`high` 是有效的
    // u32 输出参数（容纳大小的高 32 位）。按 MSDN：若返回 `INVALID_FILE_SIZE`
    // （0xffffffff）且 `GetLastError()` 不为 0 才表示失败，否则 0xffffffff 也可能是合法大小。
    let low = unsafe { GetCompressedFileSizeW(wide.as_ptr() as PCWSTR, &mut high as *mut u32) };
    if low == 0xffff_ffff && std::io::Error::last_os_error().raw_os_error() != Some(0) {
        return Err(io_err("GetCompressedFileSizeW", path, 0));
    }
    Ok((high as u64) << 32 | low as u64)
}

/// 判断文件是否为稀疏文件（检查 `FILE_ATTRIBUTE_SPARSE_FILE`）。
pub(crate) fn is_sparse(path: &Path) -> Result<bool> {
    let wide = to_wide(path);
    // SAFETY: `wide` 是 null 结尾的有效 UTF-16 字符串，调用期间存活。
    let attrs = unsafe { GetFileAttributesW(wide.as_ptr() as PCWSTR) };
    if attrs == INVALID_FILE_ATTRIBUTES {
        return Err(io_err("GetFileAttributesW", path, 0));
    }
    Ok(attrs & FILE_ATTRIBUTE_SPARSE_FILE != 0)
}

/// `FILE_SUPPORTS_SPARSE_FILES` 文件系统能力标志位。
const FILE_SUPPORTS_SPARSE_FILES: u32 = 0x40;

/// 安全地把文件从 `from` 移动到 `to`，**不覆盖**已存在的目标。
///
/// 使用 `MoveFileExW`，带 `MOVEFILE_COPY_ALLOWED`（允许跨卷复制）与 `MOVEFILE_WRITE_THROUGH`
/// （完成前不返回），但**不带** `MOVEFILE_REPLACE_EXISTING`：若 `to` 已存在，Win32 返回
/// `ERROR_ALREADY_EXISTS`，调用方据此转为错误，从而不会覆盖用户已有的同名文件。
pub(crate) fn move_file(from: &Path, to: &Path) -> Result<()> {
    let from_w = to_wide(from);
    let to_w = to_wide(to);
    // SAFETY: `from_w` 与 `to_w` 都是 null 结尾的有效 UTF-16 字符串，且在本调用期间存活；
    // 两个 `PCWSTR` 仅被同步读取；`dwFlags` 为编译期常量，无输出参数。
    let ret = unsafe {
        MoveFileExW(
            from_w.as_ptr() as PCWSTR,
            to_w.as_ptr() as PCWSTR,
            MOVEFILE_COPY_ALLOWED | MOVEFILE_WRITE_THROUGH,
        )
    };
    if ret == FALSE {
        return Err(io_err("MoveFileExW", to, 0));
    }
    Ok(())
}

/// 解析 `path` 所在的卷根，并查询其文件系统名与能力标志位。
///
/// `GetVolumeInformationW` 需要**卷根**路径（如 `C:\`），而传入的 `path` 通常是
/// 文件或子目录。这里先用 `GetVolumePathNameW` 把任意路径解析为对应的卷根。
fn volume_info(path: &Path) -> Result<(String, u32)> {
    let wide = to_wide(path);
    // 缓冲区长度取「输入路径 UTF-16 长度 + 1」与 261 的较大值（`wide` 已含 null 结尾）。
    let buf_len = wide.len().max(261);
    let mut vol: Vec<u16> = vec![0; buf_len];
    let vol_ptr = vol.as_mut_ptr();
    // SAFETY: `wide` 是 null 结尾的有效 UTF-16；`vol` 容量为 buf_len 个 u16，长度参数按
    // buf_len 给出；该 API 只写入 `vol`，不会读取越界。
    let ret = unsafe { GetVolumePathNameW(wide.as_ptr() as PCWSTR, vol_ptr, buf_len as u32) };
    if ret == FALSE {
        return Err(io_err("GetVolumePathNameW", path, 0));
    }
    // `GetVolumePathNameW` 写入的卷根自带 null 结尾（`vol` 已零初始化），可直接传入。
    let mut name_buf: [u16; 256] = [0; 256];
    let mut flags: u32 = 0;
    // SAFETY: `vol` 是 null 结尾的有效 UTF-16 卷根；`name_buf` 容量 256 个 u16、长度 256；
    // `flags` 是有效的 u32 输出参数；其余指针为 null。
    let ret = unsafe {
        GetVolumeInformationW(
            vol.as_ptr() as PCWSTR,
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut flags as *mut u32,
            name_buf.as_mut_ptr(),
            256,
        )
    };
    if ret == FALSE {
        return Err(io_err("GetVolumeInformationW", path, 0));
    }
    let len = name_buf
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(name_buf.len());
    Ok((String::from_utf16_lossy(&name_buf[..len]), flags))
}

/// 查询卷的文件系统类型名（用于判断是否为 NTFS）。
pub(crate) fn get_volume_filesystem(path: &Path) -> Result<String> {
    Ok(volume_info(path)?.0)
}

/// 把当前进程的 stdin/stdout/stderr 句柄标记为**不可继承**。
///
/// Windows 上 `CreateProcessW` 在 `bInheritHandles=TRUE` 时会把父进程所有可继承句柄
/// 复制给子进程（Rust std 的 `Command::spawn()` 内部即如此）。即使为孙进程设置
/// `Stdio::null()`，它仍会继承本进程从父 shell 得到的 stdout/stderr 管道句柄，导致
/// 捕获该管道的调用方（如 PowerShell `2>&1 | Out-String`）一直等到孙进程退出才返回。
/// 在派生常驻运行器之前清除这三个标准句柄的 `HANDLE_FLAG_INHERIT`，即可切断继承链。
///
/// 返回 `Err` 仅表示某句柄设置失败；调用方可以选择忽略（失败不影响功能，只影响继承语义）。
pub fn set_std_handles_non_inheritable() -> io::Result<()> {
    for which in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        // SAFETY: `GetStdHandle` 不接受参数，返回的是有效的标准句柄或
        // INVALID_HANDLE_VALUE / null；`SetHandleInformation` 只修改该句柄的标志，
        // 不会解引用它，也不会关闭它。传入的 dwMask/dwFlags 是有效位标志。
        let handle = unsafe { GetStdHandle(which) };
        if handle.is_null() || handle == usize::MAX as HANDLE {
            continue;
        }
        // SAFETY: `handle` 上一步已确认非 null / 非 INVALID_HANDLE_VALUE；
        // `HANDLE_FLAG_INHERIT` 作为 mask、0 作为 flags 是合法的清除继承位操作。
        let ok = unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) };
        if ok == FALSE {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// 查询卷是否支持稀疏文件（`FILE_SUPPORTS_SPARSE_FILES` 能力位）。
/// `AllocationStrategy::Auto` 据此决定实际策略；公开以便上层报告实际使用的策略。
pub fn volume_supports_sparse(path: &Path) -> Result<bool> {
    Ok(volume_info(path)?.1 & FILE_SUPPORTS_SPARSE_FILES != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disk_free_space_handles_relative_path() {
        // 相对路径 `x.bin` 的 parent 为空，应退化为当前目录 `.` 并查询成功。
        let r = get_disk_free_space(Path::new("x.bin"));
        assert!(r.is_ok(), "relative path must resolve to cwd: {r:?}");
    }
}
