#![cfg(windows)]

//! v0.3.0a：更新检查纯函数与安装流程编排的测试。
//! 不访问网络、不读取私钥、不执行任何真实安装。

use std::cell::RefCell;
use std::cmp::Ordering;
use std::rc::Rc;

use idm_desktop::service::ServiceError;
use idm_desktop::update::{UpdateCheckView, compare_versions, evaluate_update, run_install_flow};

#[test]
fn compare_versions_handles_prefix_and_numeric_segments() {
    assert_eq!(compare_versions("0.2.0", "0.3.0"), Ordering::Less);
    assert_eq!(compare_versions("v0.3.0", "0.3.0"), Ordering::Equal);
    assert_eq!(compare_versions("v1.2.3", "1.2.3"), Ordering::Equal);
    assert_eq!(compare_versions("1.10.0", "1.9.9"), Ordering::Greater);
    assert_eq!(compare_versions("0.2.0", "0.2.0"), Ordering::Equal);
}

#[test]
fn evaluate_update_reports_availability_and_notes() {
    let available = evaluate_update("0.2.0", Some(("0.3.0", Some("release notes"))));
    assert_eq!(
        available,
        UpdateCheckView {
            current_version: "0.2.0".to_string(),
            available: true,
            new_version: Some("0.3.0".to_string()),
            notes: "release notes".to_string(),
        }
    );

    let same = evaluate_update("0.2.0", Some(("v0.2.0", None)));
    assert!(!same.available, "相同版本不应提示更新");
    assert_eq!(same.new_version, None);

    let none = evaluate_update("0.2.0", None);
    assert!(!none.available, "服务端无更新时不应提示更新");
}

#[tokio::test]
async fn install_flow_stops_then_restores_when_download_fails() {
    let log = Rc::new(RefCell::new(Vec::<String>::new()));
    let stop_log = Rc::clone(&log);
    let download_log = Rc::clone(&log);
    let restore_log = Rc::clone(&log);

    let result = run_install_flow(
        move || {
            let stop_log = Rc::clone(&stop_log);
            async move {
                stop_log.borrow_mut().push("stop".to_string());
                Ok(())
            }
        },
        move || {
            let download_log = Rc::clone(&download_log);
            async move {
                download_log.borrow_mut().push("download".to_string());
                Err(ServiceError::Internal("bad signature".to_string()))
            }
        },
        move || {
            let restore_log = Rc::clone(&restore_log);
            async move {
                restore_log.borrow_mut().push("restore".to_string());
                Ok(())
            }
        },
    )
    .await;

    assert!(result.is_err(), "下载/校验失败应返回错误");
    assert_eq!(*log.borrow(), vec!["stop", "download", "restore"]);
}

#[tokio::test]
async fn install_flow_success_keeps_runner_stopped_for_installer() {
    let log = Rc::new(RefCell::new(Vec::<String>::new()));
    let stop_log = Rc::clone(&log);
    let download_log = Rc::clone(&log);
    let restore_log = Rc::clone(&log);

    let result = run_install_flow(
        move || {
            let stop_log = Rc::clone(&stop_log);
            async move {
                stop_log.borrow_mut().push("stop".to_string());
                Ok(())
            }
        },
        move || {
            let download_log = Rc::clone(&download_log);
            async move {
                download_log.borrow_mut().push("download".to_string());
                Ok(())
            }
        },
        move || {
            let restore_log = Rc::clone(&restore_log);
            async move {
                restore_log.borrow_mut().push("restore".to_string());
                Ok(())
            }
        },
    )
    .await;

    assert!(result.is_ok(), "下载安装成功应返回 Ok");
    assert_eq!(*log.borrow(), vec!["stop", "download"]);
}
