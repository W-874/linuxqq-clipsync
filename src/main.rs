use chrono::Utc;
use memfd::MemfdOptions;
use std::env;
use std::fs;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use xxhash_rust::xxh3::xxh3_128;
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::xfixes::{self, ConnectionExt as XfixesConnectionExt, SelectionEventMask};
use x11rb::protocol::xproto::ConnectionExt as XprotoConnectionExt;
use x11rb::rust_connection::RustConnection;

struct SyncState {
    last_dir: String,
    last_time: i64,
    last_sync_hash: u128,
}

struct X11ClipboardSpec {
    source_mime: &'static str,
    sync_mime: &'static str,
    process_mode: &'static str,
}

struct WaylandClipboardSpec {
    sync_mime: &'static str,
    process_mode: &'static str,
}

const EMPTY_HASH: u128 = 0;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(2);

fn log(level: &str, msg: &str) {
    let now = Utc::now().format("%H:%M:%S").to_string();
    println!("[{}] [{}] {}", now, level, msg);
}

fn get_ms() -> i64 {
    Utc::now().timestamp_millis()
}

// 优化 1：零拷贝 Hash，拒绝为大图片分配多余内存
fn calc_hash(data: &[u8], process_mode: &str) -> u128 {
    if data.is_empty() {
        return EMPTY_HASH;
    }

    match process_mode {
        "uri-list" => {
            let s = String::from_utf8_lossy(data);
            let mut result = String::new();
            for line in s.lines() {
                let trimmed = line.trim();
                if trimmed == "copy" || trimmed == "cut" {
                    continue;
                }
                result.push_str(trimmed.trim_start_matches("file://"));
            }
            let processed = result.replace("\n", "").replace("\r", "").into_bytes();
            if processed.is_empty() {
                EMPTY_HASH
            } else {
                xxh3_128(&processed)
            }
        }
        "text" => {
            let s = String::from_utf8_lossy(data);
            let result: String = s
                .chars()
                .filter(|c| *c != '\0' && *c != '\n' && *c != '\r' && *c != ' ' && *c != '\t')
                .collect();
            let processed = result.into_bytes();
            if processed.is_empty() {
                EMPTY_HASH
            } else {
                xxh3_128(&processed)
            }
        }
        _ => xxh3_128(data), // 对于图片直接 Hash 原数组，绝对不 clone！
    }
}

fn detect_x11_clipboard_spec(types_str: &str) -> Option<X11ClipboardSpec> {
    if types_str.contains("x-special/gnome-copied-files") {
        Some(X11ClipboardSpec {
            source_mime: "x-special/gnome-copied-files",
            sync_mime: "text/uri-list",
            process_mode: "uri-list",
        })
    } else if types_str.contains("application/x-qt-image") || types_str.contains("text/uri-list") {
        Some(X11ClipboardSpec {
            source_mime: "text/uri-list",
            sync_mime: "text/uri-list",
            process_mode: "uri-list",
        })
    } else if types_str.contains("image/png") {
        Some(X11ClipboardSpec {
            source_mime: "image/png",
            sync_mime: "image/png",
            process_mode: "raw",
        })
    } else if types_str.contains("image/jpeg") {
        Some(X11ClipboardSpec {
            source_mime: "image/jpeg",
            sync_mime: "image/jpeg",
            process_mode: "raw",
        })
    } else if types_str.contains("text/plain;charset=utf-8") {
        Some(X11ClipboardSpec {
            source_mime: "text/plain;charset=utf-8",
            sync_mime: "text/plain",
            process_mode: "text",
        })
    } else if types_str.contains("UTF8_STRING") {
        Some(X11ClipboardSpec {
            source_mime: "UTF8_STRING",
            sync_mime: "text/plain",
            process_mode: "text",
        })
    } else if types_str.contains("STRING") {
        Some(X11ClipboardSpec {
            source_mime: "STRING",
            sync_mime: "text/plain",
            process_mode: "text",
        })
    } else if types_str.contains("text/plain") {
        Some(X11ClipboardSpec {
            source_mime: "text/plain",
            sync_mime: "text/plain",
            process_mode: "text",
        })
    } else if types_str.contains("text/html") {
        Some(X11ClipboardSpec {
            source_mime: "text/html",
            sync_mime: "text/html",
            process_mode: "raw",
        })
    } else {
        None
    }
}

fn detect_wayland_clipboard_spec(types_str: &str) -> Option<WaylandClipboardSpec> {
    if types_str.contains("application/x-qt-image") || types_str.contains("text/uri-list") {
        Some(WaylandClipboardSpec {
            sync_mime: "text/uri-list",
            process_mode: "uri-list",
        })
    } else if types_str.contains("image/png") {
        Some(WaylandClipboardSpec {
            sync_mime: "image/png",
            process_mode: "raw",
        })
    } else if types_str.contains("image/jpeg") {
        Some(WaylandClipboardSpec {
            sync_mime: "image/jpeg",
            process_mode: "raw",
        })
    } else if types_str.contains("text/plain;charset=utf-8") {
        Some(WaylandClipboardSpec {
            sync_mime: "text/plain;charset=utf-8",
            process_mode: "text",
        })
    } else if types_str.contains("UTF8_STRING") {
        Some(WaylandClipboardSpec {
            sync_mime: "UTF8_STRING",
            process_mode: "text",
        })
    } else if types_str.contains("STRING") {
        Some(WaylandClipboardSpec {
            sync_mime: "STRING",
            process_mode: "text",
        })
    } else if types_str.contains("text/plain") {
        Some(WaylandClipboardSpec {
            sync_mime: "text/plain",
            process_mode: "text",
        })
    } else if types_str.contains("text/html") {
        Some(WaylandClipboardSpec {
            sync_mime: "text/html",
            process_mode: "raw",
        })
    } else {
        None
    }
}

fn normalize_uri_list(data: &[u8]) -> Vec<u8> {
    let s = String::from_utf8_lossy(data);
    let mut res = String::new();
    for line in s.lines() {
        if line == "copy" || line == "cut" {
            continue;
        }
        if line.starts_with('/') {
            res.push_str("file:///");
            res.push_str(&line[1..]);
        } else {
            res.push_str(line);
        }
        res.push('\n');
    }
    res.into_bytes()
}

// ==========================================
// 核心机制：无管道读取与写入 (基于 memfd)
// ==========================================

fn wait_child_with_timeout(child: &mut std::process::Child, cmd: &str) -> Result<(), String> {
    let deadline = Instant::now() + COMMAND_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if status.success() {
                    return Ok(());
                }
                return Err(format!("{cmd} 退出失败: {status}"));
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("{cmd} 执行超时"));
                }
                thread::sleep(Duration::from_millis(10));
            }
            Err(err) => return Err(format!("{cmd} 等待失败: {err}")),
        }
    }
}

fn read_clipboard(cmd: &str, args: &[&str]) -> Result<Vec<u8>, String> {
    let Ok(mfd) = MemfdOptions::default().create("clip_read") else {
        return Err("创建 memfd 失败".to_string());
    };
    let file = mfd.into_file();
    let Ok(file_out) = file.try_clone() else {
        return Err("复制 memfd 句柄失败".to_string());
    };

    let mut child = Command::new(cmd)
        .args(args)
        .stdout(Stdio::from(file_out))
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("启动 {cmd} 失败: {e}"))?;
    wait_child_with_timeout(&mut child, cmd)?;

    let mut data = Vec::new();
    let mut file_read = file;
    file_read
        .seek(SeekFrom::Start(0))
        .map_err(|e| format!("重置 {cmd} 输出游标失败: {e}"))?;
    file_read
        .read_to_end(&mut data)
        .map_err(|e| format!("读取 {cmd} 输出失败: {e}"))?;
    Ok(data)
}

fn write_clipboard(cmd: &str, args: &[&str], data: &[u8]) -> Result<(), String> {
    let Ok(mfd) = MemfdOptions::default().create("clip_write") else {
        return Err("创建 memfd 失败".to_string());
    };
    let mut file = mfd.into_file();
    file.write_all(data)
        .map_err(|e| format!("写入 {cmd} 输入失败: {e}"))?;
    file.seek(SeekFrom::Start(0))
        .map_err(|e| format!("重置 {cmd} 输入游标失败: {e}"))?;

    let mut child = Command::new(cmd)
        .args(args)
        .stdin(Stdio::from(file))
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("启动 {cmd} 失败: {e}"))?;
    wait_child_with_timeout(&mut child, cmd)
}

fn get_xdg_runtime_dir() -> String {
    if let Ok(dir) = env::var("XDG_RUNTIME_DIR") {
        return dir;
    }
    format!("/run/user/{}", unsafe { libc::getuid() })
}

fn connect_x11_clipboard_watcher(display: &str) -> Result<RustConnection, String> {
    let (conn, screen_num) =
        x11rb::connect(Some(display)).map_err(|e| format!("X11 连接失败: {e}"))?;
    init_xfixes_clipboard_watch(&conn, screen_num)?;
    Ok(conn)
}

fn wait_for_x11_clipboard_event(conn: &RustConnection) -> Result<(), String> {
    loop {
        let event = conn
            .wait_for_event()
            .map_err(|e| format!("等待 X11 事件失败: {e}"))?;

        if let x11rb::protocol::Event::XfixesSelectionNotify(_) = event {
            return Ok(());
        }
    }
}

fn init_xfixes_clipboard_watch(conn: &RustConnection, screen_num: usize) -> Result<(), String> {
    conn.extension_information(xfixes::X11_EXTENSION_NAME)
        .map_err(|e| format!("查询 XFixes 扩展失败: {e}"))?
        .ok_or_else(|| "X11 服务器未提供 XFixes 扩展".to_string())?;

    conn.xfixes_query_version(5, 0)
        .map_err(|e| format!("XFixes 版本协商失败: {e}"))?
        .reply()
        .map_err(|e| format!("获取 XFixes 版本回复失败: {e}"))?;

    let clipboard = conn
        .intern_atom(false, b"CLIPBOARD")
        .map_err(|e| format!("查询 CLIPBOARD atom 失败: {e}"))?
        .reply()
        .map_err(|e| format!("获取 CLIPBOARD atom 回复失败: {e}"))?
        .atom;

    let screen = &conn.setup().roots[screen_num];
    conn.xfixes_select_selection_input(
        screen.root,
        clipboard,
        SelectionEventMask::SET_SELECTION_OWNER
            | SelectionEventMask::SELECTION_WINDOW_DESTROY
            | SelectionEventMask::SELECTION_CLIENT_CLOSE,
    )
    .map_err(|e| format!("注册 X11 剪贴板监听失败: {e}"))?;

    conn.flush()
        .map_err(|e| format!("刷新 X11 请求失败: {e}"))?;
    Ok(())
}

fn main() {
    let xdg_runtime_dir = get_xdg_runtime_dir();
    env::set_var("XDG_RUNTIME_DIR", &xdg_runtime_dir);

    if let Ok(home) = env::var("HOME") {
        env::set_var("XAUTHORITY", format!("{}/.Xauthority", home));
    }

    let mut wayland_display = env::var("WAYLAND_DISPLAY").unwrap_or_default();
    let mut display = env::var("DISPLAY").unwrap_or_default();

    if wayland_display.is_empty() {
        if let Ok(entries) = fs::read_dir(&xdg_runtime_dir) {
            for entry in entries.flatten() {
                let file_name = entry.file_name().into_string().unwrap_or_default();
                if file_name.starts_with("wayland-") && !file_name.contains('.') {
                    if Command::new("wl-paste")
                        .env("WAYLAND_DISPLAY", &file_name)
                        .arg("--list-types")
                        .output()
                        .is_ok()
                    {
                        wayland_display = file_name;
                        break;
                    }
                }
            }
        }
    }

    if display.is_empty() {
        if let Ok(entries) = fs::read_dir("/tmp/.X11-unix") {
            for entry in entries.flatten() {
                let file_name = entry.file_name().into_string().unwrap_or_default();
                if file_name.starts_with('X') {
                    let test_d = format!(":{}", &file_name[1..]);
                    if Command::new("xclip")
                        .env("DISPLAY", &test_d)
                        .args(["-selection", "clipboard", "-t", "TARGETS", "-o"])
                        .output()
                        .is_ok()
                    {
                        display = test_d;
                        break;
                    }
                }
            }
        }
    }

    env::set_var("WAYLAND_DISPLAY", &wayland_display);
    env::set_var("DISPLAY", &display);

    log(
        "INIT",
        &format!(
            "探测结果: DISPLAY={}, WAYLAND_DISPLAY={}",
            display, wayland_display
        ),
    );

    if wayland_display.is_empty() || display.is_empty() {
        log("FATAL", "找不到存活的图形界面，退出...");
        std::process::exit(1);
    }

    let shared_state = Arc::new(Mutex::new(SyncState {
        last_dir: String::new(),
        last_time: 0,
        last_sync_hash: EMPTY_HASH,
    }));

    // ==========================================
    // X2W 线程
    // ==========================================
    let state_x2w = Arc::clone(&shared_state);
    let display_x2w = display.clone();
    thread::spawn(move || {
        log("INFO", "=== [X2W] 线程启动 ===");
        loop {
            let conn = match connect_x11_clipboard_watcher(&display_x2w) {
                Ok(conn) => conn,
                Err(err) => {
                    log("ERROR", &format!("X11 监听初始化失败: {}", err));
                    thread::sleep(Duration::from_secs(1));
                    continue;
                }
            };

            loop {
                if let Err(err) = wait_for_x11_clipboard_event(&conn) {
                    log("ERROR", &format!("X11 监听失败: {}", err));
                    break;
                }
                thread::sleep(Duration::from_millis(30));

                {
                    let state = state_x2w.lock().unwrap();
                    let now = get_ms();
                    if state.last_dir == "W2X" && (now - state.last_time < 1000) {
                        continue;
                    }
                }

                let types_raw = match read_clipboard(
                    "xclip",
                    &["-selection", "clipboard", "-t", "TARGETS", "-o"],
                ) {
                    Ok(data) => data,
                    Err(err) => {
                        log("WARN", &format!("读取 X11 TARGETS 失败: {}", err));
                        continue;
                    }
                };
                let types_str = String::from_utf8_lossy(&types_raw);

                let Some(spec) = detect_x11_clipboard_spec(&types_str) else {
                    continue;
                };

                let x_data = match read_clipboard("xclip", &["-sel", "clip", "-o", "-t", spec.source_mime]) {
                    Ok(data) => data,
                    Err(err) => {
                        log("WARN", &format!("读取 X11 剪贴板失败: {}", err));
                        continue;
                    }
                };
                let current_hash = calc_hash(&x_data, spec.process_mode);
                if current_hash == EMPTY_HASH {
                    continue;
                }

                {
                    let mut state = state_x2w.lock().unwrap();
                    let now = get_ms();
                    if state.last_dir == "W2X" && (now - state.last_time < 1000) {
                        continue;
                    }
                    if current_hash == state.last_sync_hash {
                        continue;
                    }
                    state.last_dir = "X2W".to_string();
                    state.last_time = now;
                }

                log(
                    "X2W",
                    &format!(
                        "写入 Wayland... (Hash: {:08x})",
                        (current_hash >> 96) as u32
                    ),
                );

                let write_data = if spec.process_mode == "uri-list" {
                    normalize_uri_list(&x_data)
                } else {
                    x_data
                };

                match write_clipboard("wl-copy", &["-t", spec.sync_mime], &write_data) {
                    Ok(()) => {
                        let mut state = state_x2w.lock().unwrap();
                        state.last_sync_hash = current_hash;
                    }
                    Err(err) => {
                        log("WARN", &format!("写入 Wayland 剪贴板失败: {}", err));
                    }
                }
            }

            thread::sleep(Duration::from_secs(1));
        }
    });

    log("INFO", "=== [W2X] 线程启动 ===");
    log("SYS", "双向剪贴板同步服务已准备就绪！");

    // ==========================================
    // W2X 主线程
    // ==========================================
    let mut wl_watch = Command::new("wl-paste")
        .args(["--watch", "echo"])
        .stdout(Stdio::piped())
        .spawn()
        .expect("Failed to start wl-paste --watch");

    let stdout = wl_watch.stdout.take().unwrap();
    let reader = BufReader::new(stdout);

    for _line in reader.lines() {
        thread::sleep(Duration::from_millis(30));

        {
            let state = shared_state.lock().unwrap();
            let now = get_ms();
            if state.last_dir == "X2W" && (now - state.last_time < 1000) {
                continue;
            }
        }

        let types_raw = match read_clipboard("wl-paste", &["--list-types"]) {
            Ok(data) => data,
            Err(err) => {
                log("WARN", &format!("读取 Wayland TARGETS 失败: {}", err));
                continue;
            }
        };
        let types_str = String::from_utf8_lossy(&types_raw);

        let Some(spec) = detect_wayland_clipboard_spec(&types_str) else {
            continue;
        };

        let w_data = match read_clipboard("wl-paste", &["-t", spec.sync_mime]) {
            Ok(data) => data,
            Err(err) => {
                log("WARN", &format!("读取 Wayland 剪贴板失败: {}", err));
                continue;
            }
        };
        let current_hash = calc_hash(&w_data, spec.process_mode);
        if current_hash == EMPTY_HASH {
            continue;
        }

        {
            let mut state = shared_state.lock().unwrap();
            let now = get_ms();
            if state.last_dir == "X2W" && (now - state.last_time < 1000) {
                continue;
            }
            if current_hash == state.last_sync_hash {
                continue;
            }
            state.last_dir = "W2X".to_string();
            state.last_time = now;
        }

        log(
            "W2X",
            &format!("写入 X11... (Hash: {:08x})", (current_hash >> 96) as u32),
        );

        let write_data = if spec.process_mode == "uri-list" {
            normalize_uri_list(&w_data)
        } else {
            w_data
        };

        let target_t = match spec.sync_mime {
            "text/plain;charset=utf-8" | "text/plain" => "UTF8_STRING",
            other => other,
        };

        match write_clipboard(
            "xclip",
            &["-sel", "clip", "-i", "-t", target_t],
            &write_data,
        ) {
            Ok(()) => {
                let mut state = shared_state.lock().unwrap();
                state.last_sync_hash = current_hash;
            }
            Err(err) => {
                log("WARN", &format!("写入 X11 剪贴板失败: {}", err));
            }
        }
    }

    log("ERROR", "W2X 监听意外终止，触发退出...");
    std::process::exit(1);
}
