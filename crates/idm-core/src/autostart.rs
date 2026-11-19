//! 开机启动项管理（仅 Windows）：读写 HKCU Run 键。
//!
//! 从 idm-cli 的 runner 模块上移，统一由桌面程序与 CLI 共用。
//! 只用 `reg.exe` 子进程，不引入任何注册表 crate。

use std::process::{Command, Output};

/// HKCU 自动启动项注册表路径。
pub const REG_RUN: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";

fn reg(args: &[&str]) -> Result<Output, String> {
    Command::new("reg")
        .args(args)
        .output()
        .map_err(|e| e.to_string())
}

/// 值名只允许字母数字、`-`、`_`，防止注入 reg.exe 参数。
pub fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("value name must not be empty".to_string());
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err("value name may contain only letters, digits, '-' and '_'".to_string());
    }
    Ok(())
}

/// 写入开机启动项（已存在则覆盖）。
pub fn install(value_name: &str, command_line: &str) -> Result<(), String> {
    validate_name(value_name)?;
    let out = reg(&[
        "add",
        REG_RUN,
        "/v",
        value_name,
        "/t",
        "REG_SZ",
        "/d",
        command_line,
        "/f",
    ])?;
    if !out.status.success() {
        return Err(format!(
            "reg add failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

/// 删除开机启动项；值不存在时幂等成功。
pub fn uninstall(value_name: &str) -> Result<(), String> {
    validate_name(value_name)?;
    let out = reg(&["delete", REG_RUN, "/v", value_name, "/f"])?;
    // 值本来就不存在时 reg 返回非零，但卸载语义上是幂等成功。
    let _ = out;
    Ok(())
}

/// 查询开机启动项的值内容；不存在返回 None。
pub fn query(value_name: &str) -> Option<String> {
    if validate_name(value_name).is_err() {
        return None;
    }
    let out = reg(&["query", REG_RUN, "/v", value_name]).ok()?;
    if !out.status.success() {
        return None;
    }
    // reg query 输出形如：
    //   HKEY_CURRENT_USER\...\Run
    //       name    REG_SZ    <value>
    let text = String::from_utf8_lossy(&out.stdout);
    for line in text.lines() {
        if line.contains(value_name) && line.contains("REG_SZ") {
            let parts: Vec<&str> = line.split_whitespace().collect();
            // 形如 [name, REG_SZ, value...]
            if let Some(pos) = parts.iter().position(|p| *p == "REG_SZ")
                && pos + 1 < parts.len()
            {
                return Some(parts[pos + 1..].join(" "));
            }
        }
    }
    Some(String::new())
}

/// 查询开机启动项的原始 `reg query` 输出（成功时返回 trim 后的 stdout）。
/// 供 `runner status` 保持迁移前的输出格式。
pub fn raw_query(value_name: &str) -> Option<String> {
    if validate_name(value_name).is_err() {
        return None;
    }
    let out = reg(&["query", REG_RUN, "/v", value_name]).ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_name_rules() {
        assert!(validate_name("abc-123_X").is_ok());
        assert!(validate_name("").is_err());
        assert!(validate_name("has space").is_err());
        assert!(validate_name("a/b").is_err());
        assert!(validate_name("a\"b").is_err());
    }
}
