//! 唯一允许 `unsafe` 的子模块：对 Win32 API 的薄封装。
//!
//! 本文件顶部 [`#![allow(unsafe_code)]`](unsafe_code) 是本阶段唯一被允许的
//! `#[allow]`。所有 FFI 调用都被封装成安全函数，其它子模块不得直接调用
//! windows-sys。每个 `unsafe` 块上方都有对应的 `// SAFETY:` 注释。

#![allow(unsafe_code)]

use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use windows_sys::Win32::Foundation::{FALSE, HANDLE};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ALLOCATION_INFO, FILE_ATTRIBUTE_SPARSE_FILE, FILE_STANDARD_INFO, FileAllocationInfo,
    FileStandardInfo, FlushFileBuffers, GetCompressedFileSizeW, GetDiskFreeSpaceExW,
    GetFileAttributesW, GetFileInformationByHandleEx, GetVolumeInformationW, GetVolumePathNameW,
    SetFileInformationByHandle,
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
/// 的 `create` 前置检查，也兼容传入已有目录/文件的场景。
pub(crate) fn get_disk_free_space(path: &Path) -> Result<u64> {
    let query = path.parent().unwrap_or(path);
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
pub(crate) fn set_file_allocation(handle: HANDLE, size: u64) -> Result<()> {
    let info = FILE_ALLOCATION_INFO {
        AllocationSize: size as i64,
    };
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
            path: std::path::PathBuf::new(),
            offset: size,
            source: std::io::Error::last_os_error(),
        });
    }
    Ok(())
}

/// 通过 `DeviceIoControl(FSCTL_SET_SPARSE)` 把文件标记为稀疏文件。
pub(crate) fn set_sparse(handle: HANDLE) -> Result<()> {
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
            path: std::path::PathBuf::new(),
            offset: 0,
            source: std::io::Error::last_os_error(),
        });
    }
    Ok(())
}

/// 通过 `GetFileInformationByHandleEx(FileStandardInfo)` 读取物理分配大小。
pub(crate) fn get_allocation_size(handle: HANDLE) -> Result<u64> {
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
            path: std::path::PathBuf::new(),
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
    // SAFETY: `wide` 是 null 结尾的有效 UTF-16 字符串，调用期间存活；`high` 是
    // 有效的 u32 输出参数（容纳大小的高 32 位）。
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

/// 通过 `FlushFileBuffers` 把文件数据刷到磁盘。
pub(crate) fn flush_buffers(handle: HANDLE) -> Result<()> {
    // SAFETY: `handle` 是调用方在调用期间持有的有效文件句柄。
    let ret = unsafe { FlushFileBuffers(handle) };
    if ret == FALSE {
        return Err(Error::Io {
            operation: "FlushFileBuffers",
            path: std::path::PathBuf::new(),
            offset: 0,
            source: std::io::Error::last_os_error(),
        });
    }
    Ok(())
}

/// 查询卷的文件系统类型名（用于判断是否为 NTFS）。
///
/// `GetVolumeInformationW` 需要**卷根**路径（如 `C:\`），而传入的 `path` 通常是
/// 文件或子目录。这里先用 `GetVolumePathNameW` 把任意路径解析为对应的卷根，
/// 再查询文件系统名。
pub(crate) fn get_volume_filesystem(path: &Path) -> Result<String> {
    let wide = to_wide(path);
    let mut vol: [u16; 256] = [0; 256];
    // SAFETY: `wide` 是 null 结尾的有效 UTF-16 字符串；`vol` 容量为 256 个 u16，
    // 长度参数按 256 给出；该 API 只写入 `vol`，不会读取越界。
    let ret = unsafe { GetVolumePathNameW(wide.as_ptr() as PCWSTR, vol.as_mut_ptr(), 256) };
    if ret == FALSE {
        return Err(io_err("GetVolumePathNameW", path, 0));
    }
    // `GetVolumePathNameW` 写入的卷根自带 null 结尾（`vol` 已零初始化），可直接传入。
    let mut buf: [u16; 256] = [0; 256];
    // SAFETY: `vol[..vol_len]` 是 null 结尾的有效 UTF-16 卷根；`buf` 容量为 256
    // 个 u16，对应的大小参数也按 256 给出；其余指针为 null。
    let ret = unsafe {
        GetVolumeInformationW(
            vol.as_ptr() as PCWSTR,
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            buf.as_mut_ptr(),
            256,
        )
    };
    if ret == FALSE {
        return Err(io_err("GetVolumeInformationW", path, 0));
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Ok(String::from_utf16_lossy(&buf[..len]))
}
