use peon_burrow_ipc_types::ServiceStatus;

/// 一行成功输出（`--json` 时给结构化结果）。
pub(super) fn say(json: bool, human: &str, value: serde_json::Value) {
    if json {
        print_json(&value);
    } else {
        println!("{human}");
    }
}

/// 人话版服务状态。
pub(super) fn print_service_status(status: &ServiceStatus) {
    if status.installed {
        println!("服务：已安装（{}）", status.name);
        println!("  运行中：{}", if status.running { "是" } else { "否" });
        println!("  级别：{:?}", status.level);
        println!("  自启：{:?}", status.autostart);
        println!(
            "  失败重启策略：{}",
            if status.restart_policy_configured {
                "已配置"
            } else {
                "未配置（服务崩了不会自己回来）"
            }
        );
        if let Some(path) = &status.binary_path {
            println!("  路径：{path}");
        }
    } else {
        println!("服务：未安装（{}）", status.name);
        println!("下一步：`burrow service install`（默认用户级、零提权）");
    }
}

/// 打印人话版状态。
pub(super) fn print_status(value: &serde_json::Value) {
    let process = &value["process"];
    let service = &value["service"];
    println!("中继：运行中");
    println!(
        "  地址：ws://{}:{}/",
        process["host"].as_str().unwrap_or("?"),
        process["port"].as_u64().unwrap_or_default()
    );
    println!("  版本：{}", process["version"].as_str().unwrap_or("?"));
    println!(
        "  连接：{}（其中 watch {}）",
        process["connections"].as_u64().unwrap_or_default(),
        process["watchConnections"].as_u64().unwrap_or_default()
    );
    println!("  启动于：{}", process["startedAt"].as_str().unwrap_or("?"));
    if let Some(error) = process["lastError"].as_str() {
        println!("  最近错误：{error}");
    }
    println!(
        "服务：{}",
        if service["installed"].as_bool().unwrap_or(false) {
            "已安装"
        } else {
            "未安装"
        }
    );
}

/// 打印 JSON。
pub(super) fn print_json(value: &serde_json::Value) {
    match serde_json::to_string_pretty(value) {
        Ok(text) => println!("{text}"),
        Err(error) => eprintln!("JSON 输出失败：{error}"),
    }
}
