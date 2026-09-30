//! CLI 界面语言：`--lang`、`TERMBRIDGE_LANG`、locale 环境变量与帮助文本。

use std::process::{Command, Output};

/// 在隔离的配置目录里运行 CLI，并清掉会影响语言选择的环境变量。
fn run(args: &[&str], envs: &[(&str, &str)]) -> Output {
    let config = std::env::temp_dir().join(format!("termbridge-lang-{}", std::process::id()));
    std::fs::create_dir_all(&config).unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_termbridge"));
    cmd.args(args)
        .env("XDG_CONFIG_HOME", &config)
        .env("LOCALAPPDATA", &config)
        .env_remove("TERMBRIDGE_LANG")
        .env_remove("LC_ALL")
        .env_remove("LC_MESSAGES")
        .env_remove("LANG");
    for (key, value) in envs {
        cmd.env(key, value);
    }
    cmd.output().expect("failed to run termbridge")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn defaults_to_english() {
    let out = run(&["host", "status"], &[]);
    assert!(out.status.success());
    assert!(
        stdout(&out).contains("The host is not initialized"),
        "{}",
        stdout(&out)
    );
}

#[test]
fn lang_flag_selects_language() {
    let zh = run(&["--lang", "zh", "host", "status"], &[]);
    assert!(stdout(&zh).contains("接收端未初始化"), "{}", stdout(&zh));
    // 全局选项放在子命令之后同样有效。
    let en = run(
        &["host", "status", "--lang=en"],
        &[("TERMBRIDGE_LANG", "zh")],
    );
    assert!(
        stdout(&en).contains("The host is not initialized"),
        "{}",
        stdout(&en)
    );
}

#[test]
fn environment_selects_language() {
    let by_app_var = run(&["host", "status"], &[("TERMBRIDGE_LANG", "zh")]);
    assert!(stdout(&by_app_var).contains("接收端未初始化"));
    let by_locale = run(&["host", "status"], &[("LANG", "zh_CN.UTF-8")]);
    assert!(stdout(&by_locale).contains("接收端未初始化"));
    let explicit_beats_locale = run(
        &["--lang", "en", "host", "status"],
        &[("LANG", "zh_CN.UTF-8")],
    );
    assert!(stdout(&explicit_beats_locale).contains("The host is not initialized"));
}

#[test]
fn help_follows_language() {
    let en = run(&["--help"], &[]);
    assert!(stdout(&en).contains("Host: run this machine as the side being connected to"));
    let zh = run(&["--lang", "zh", "--help"], &[]);
    assert!(
        stdout(&zh).contains("接收端（本机作为被连接方）"),
        "{}",
        stdout(&zh)
    );
    let zh_sub = run(&["host", "enable", "--help", "--lang", "zh"], &[]);
    assert!(
        stdout(&zh_sub).contains("停用而不是启用"),
        "{}",
        stdout(&zh_sub)
    );
}

#[test]
fn rejects_unknown_language() {
    let out = run(&["--lang", "fr", "host", "status"], &[]);
    assert!(!out.status.success());
}
