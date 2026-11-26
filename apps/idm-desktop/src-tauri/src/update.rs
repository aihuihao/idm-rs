//! v0.3.0a：与 Tauri 无关的更新检查数据结构、纯函数与安装流程编排。
//!
//! 本模块不引用 tauri，也不访问网络；Rust 端命令只负责把真实 Updater 的结果
//! 转成这里的数据结构，再交给纯函数判断是否提示更新。

use std::cmp::Ordering;
use std::future::Future;

use serde::Serialize;

use crate::service::ServiceError;

/// 更新检查结果（供 Tauri 命令序列化给前端）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateCheckView {
    pub current_version: String,
    pub available: bool,
    pub new_version: Option<String>,
    pub notes: String,
}

/// 下载进度事件载荷。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateProgress {
    pub downloaded: usize,
    pub total: Option<u64>,
}

fn version_parts(raw: &str) -> Vec<u64> {
    let trimmed = raw.trim();
    let without_v = trimmed
        .strip_prefix('v')
        .or_else(|| trimmed.strip_prefix('V'))
        .unwrap_or(trimmed);
    without_v
        .split('.')
        .map(|part| part.parse::<u64>().unwrap_or(0))
        .collect()
}

/// 版本号比较：忽略首尾空白与前导 v/V，按点分数字段逐段比较，缺失字段按 0。
pub fn compare_versions(left: &str, right: &str) -> Ordering {
    let a = version_parts(left);
    let b = version_parts(right);
    for index in 0..a.len().max(b.len()) {
        let left_part = a.get(index).copied().unwrap_or(0);
        let right_part = b.get(index).copied().unwrap_or(0);
        match left_part.cmp(&right_part) {
            Ordering::Equal => {}
            ordering => return ordering,
        }
    }
    Ordering::Equal
}

/// 把「服务端发布版本」整理成不依赖 Tauri 的检查结果。
/// `remote` 为 None 表示服务端没有可用发布（例如 204）；版本不高于当前版本时视为已是最新。
pub fn evaluate_update(current: &str, remote: Option<(&str, Option<&str>)>) -> UpdateCheckView {
    let current_version = current.trim().to_string();
    match remote {
        Some((announced, notes)) if compare_versions(announced, current) == Ordering::Greater => {
            UpdateCheckView {
                current_version,
                available: true,
                new_version: Some(announced.trim().to_string()),
                notes: notes.unwrap_or("").trim().to_string(),
            }
        }
        _ => UpdateCheckView {
            current_version,
            available: false,
            new_version: None,
            notes: String::new(),
        },
    }
}

/// 自动检查失败只写日志；手动检查通过命令返回此中文原因。
pub fn check_error_text(err: &str) -> String {
    format!("检查更新失败：{err}")
}

/// 安装更新的固定顺序：先安全停止运行器，再下载安装；失败时恢复运行器。
///
/// 三个参数均为可注入闭包，便于测试记录调用顺序并模拟下载/签名校验失败。
/// 成功时不恢复运行器：Windows 安装器接管并自动退出/重启程序。
pub async fn run_install_flow<S, SFut, D, DFut, R, RFut>(
    stop: S,
    download: D,
    restore: R,
) -> Result<(), ServiceError>
where
    S: FnOnce() -> SFut,
    SFut: Future<Output = Result<(), ServiceError>>,
    D: FnOnce() -> DFut,
    DFut: Future<Output = Result<(), ServiceError>>,
    R: FnOnce() -> RFut,
    RFut: Future<Output = Result<(), ServiceError>>,
{
    stop().await?;
    match download().await {
        Ok(()) => Ok(()),
        Err(err) => match restore().await {
            Ok(()) => Err(err),
            Err(restore_err) => Err(ServiceError::Internal(format!(
                "{err}；恢复运行器失败：{restore_err}"
            ))),
        },
    }
}
