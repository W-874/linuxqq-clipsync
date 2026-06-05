use memfd::MemfdOptions;
use std::env;
use std::fs;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use xxhash_rust::xxh3::xxh3_128;
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::xfixes::{self, ConnectionExt as XfixesConnectionExt, SelectionEventMask};
use x11rb::protocol::xproto::ConnectionExt as XprotoConnectionExt;
use x11rb::rust_connection::RustConnection;

struct SyncState {
    last_dir: Option<SyncDir>,
    last_time: i64,
    last_sync_hash: u128,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProcessMode {
    UriList,
    Text,
    Raw,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SyncDir {
    X2W,
    W2X,
}

#[derive(Clone, Copy)]
struct X11ClipboardSpec {
    source_mime: &'static str,
    sync_mime: &'static str,
    process_mode: ProcessMode,
}

#[derive(Clone, Copy)]
struct WaylandClipboardSpec {
    sync_mime: &'static str,
    process_mode: ProcessMode,
}

const EMPTY_HASH: u128 = 0;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(2);
const X11_CLIPBOARD_RULES: &[(&str, X11ClipboardSpec)] = &[
    (
        "x-special/gnome-copied-files",
        X11ClipboardSpec {
            source_mime: "x-special/gnome-copied-files",
            sync_mime: "text/uri-list",
            process_mode: ProcessMode::UriList,
        },
    ),
    (
        "application/x-qt-image",
        X11ClipboardSpec {
            source_mime: "text/uri-list",
            sync_mime: "text/uri-list",
            process_mode: ProcessMode::UriList,
        },
    ),
    (
        "text/uri-list",
        X11ClipboardSpec {
            source_mime: "text/uri-list",
            sync_mime: "text/uri-list",
            process_mode: ProcessMode::UriList,
        },
    ),
    (
        "image/png",
        X11ClipboardSpec {
            source_mime: "image/png",
            sync_mime: "image/png",
            process_mode: ProcessMode::Raw,
        },
    ),
    (
        "image/jpeg",
        X11ClipboardSpec {
            source_mime: "image/jpeg",
            sync_mime: "image/jpeg",
            process_mode: ProcessMode::Raw,
        },
    ),
    (
        "text/plain;charset=utf-8",
        X11ClipboardSpec {
            source_mime: "text/plain;charset=utf-8",
            sync_mime: "text/plain",
            process_mode: ProcessMode::Text,
        },
    ),
    (
        "UTF8_STRING",
        X11ClipboardSpec {
            source_mime: "UTF8_STRING",
            sync_mime: "text/plain",
            process_mode: ProcessMode::Text,
        },
    ),
    (
        "STRING",
        X11ClipboardSpec {
            source_mime: "STRING",
            sync_mime: "text/plain",
            process_mode: ProcessMode::Text,
        },
    ),
    (
        "text/plain",
        X11ClipboardSpec {
            source_mime: "text/plain",
            sync_mime: "text/plain",
            process_mode: ProcessMode::Text,
        },
    ),
    (
        "text/html",
        X11ClipboardSpec {
            source_mime: "text/html",
            sync_mime: "text/html",
            process_mode: ProcessMode::Raw,
        },
    ),
];
const WAYLAND_CLIPBOARD_RULES: &[(&str, WaylandClipboardSpec)] = &[
    (
        "application/x-qt-image",
        WaylandClipboardSpec {
            sync_mime: "text/uri-list",
            process_mode: ProcessMode::UriList,
        },
    ),
    (
        "text/uri-list",
        WaylandClipboardSpec {
            sync_mime: "text/uri-list",
            process_mode: ProcessMode::UriList,
        },
    ),
    (
        "image/png",
        WaylandClipboardSpec {
            sync_mime: "image/png",
            process_mode: ProcessMode::Raw,
        },
    ),
    (
        "image/jpeg",
        WaylandClipboardSpec {
            sync_mime: "image/jpeg",
            process_mode: ProcessMode::Raw,
        },
    ),
    (
        "text/plain;charset=utf-8",
        WaylandClipboardSpec {
            sync_mime: "text/plain;charset=utf-8",
            process_mode: ProcessMode::Text,
        },
    ),
    (
        "UTF8_STRING",
        WaylandClipboardSpec {
            sync_mime: "UTF8_STRING",
            process_mode: ProcessMode::Text,
        },
    ),
    (
        "STRING",
        WaylandClipboardSpec {
            sync_mime: "STRING",
            process_mode: ProcessMode::Text,
        },
    ),
    (
        "text/plain",
        WaylandClipboardSpec {
            sync_mime: "text/plain",
            process_mode: ProcessMode::Text,
        },
    ),
    (
        "text/html",
        WaylandClipboardSpec {
            sync_mime: "text/html",
            process_mode: ProcessMode::Raw,
        },
    ),
];

fn log(level: &str, msg: &str) {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() % 86_400)
        .unwrap_or(0);
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    println!("[{h:02}:{m:02}:{s:02}] [{level}] {msg}");
}

fn get_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// 优化 1：零拷贝 Hash，拒绝为大图片分配多余内存
fn calc_hash(data: &[u8], process_mode: ProcessMode) -> u128 {
    if data.is_empty() {
        return EMPTY_HASH;
    }

    match process_mode {
        ProcessMode::UriList => {
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
        ProcessMode::Text => {
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
        ProcessMode::Raw => xxh3_128(data),
    }
}

fn detect_x11_clipboard_spec(types_str: &str) -> Option<X11ClipboardSpec> {
    X11_CLIPBOARD_RULES
        .iter()
        .find_map(|(needle, spec)| types_str.contains(needle).then_some(*spec))
}

fn detect_wayland_clipboard_spec(types_str: &str) -> Option<WaylandClipboardSpec> {
    WAYLAND_CLIPBOARD_RULES
        .iter()
        .find_map(|(needle, spec)| types_str.contains(needle).then_some(*spec))
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

fn needs_uri_normalization(mode: ProcessMode) -> bool {
    mode == ProcessMode::UriList
}

fn should_skip_recent(state: &SyncState, dir: SyncDir, now: i64) -> bool {
    state.last_dir == Some(match dir {
        SyncDir::X2W => SyncDir::W2X,
        SyncDir::W2X => SyncDir::X2W,
    }) && now - state.last_time < 1000
}

fn precheck_sync(state: &Arc<Mutex<SyncState>>, dir: SyncDir) -> bool {
    let state = state.lock().unwrap();
    should_skip_recent(&state, dir, get_ms())
}

fn begin_sync(state: &Arc<Mutex<SyncState>>, dir: SyncDir, hash: u128) -> bool {
    let mut state = state.lock().unwrap();
    let now = get_ms();
    if should_skip_recent(&state, dir, now) || state.last_sync_hash == hash {
        return false;
    }
    state.last_dir = Some(dir);
    state.last_time = now;
    true
}

fn finish_sync(state: &Arc<Mutex<SyncState>>, hash: u128) {
    state.lock().unwrap().last_sync_hash = hash;
}

fn read_or_log(cmd: &str, args: &[&str], context: &str) -> Option<Vec<u8>> {
    match read_clipboard(cmd, args) {
        Ok(data) => Some(data),
        Err(err) => {
            log("WARN", &format!("{context}: {err}"));
            None
        }
    }
}

fn write_or_log(cmd: &str, args: &[&str], data: &[u8], context: &str) -> bool {
    match write_clipboard(cmd, args, data) {
        Ok(()) => true,
        Err(err) => {
            log("WARN", &format!("{context}: {err}"));
            false
        }
    }
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

    if env::var("XAUTHORITY").is_err() {
        if let Ok(home) = env::var("HOME") {
            let candidate = format!("{}/.Xauthority", home);
            if std::path::Path::new(&candidate).exists() {
                env::set_var("XAUTHORITY", candidate);
            }
        }
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
        last_dir: None,
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
                if precheck_sync(&state_x2w, SyncDir::X2W) {
                    continue;
                }

                let Some(types_raw) = read_or_log(
                    "xclip",
                    &["-selection", "clipboard", "-t", "TARGETS", "-o"],
                    "读取 X11 TARGETS 失败",
                ) else {
                    continue;
                };
                let types_str = String::from_utf8_lossy(&types_raw);
                let Some(spec) = detect_x11_clipboard_spec(&types_str) else {
                    continue;
                };
                let Some(x_data) =
                    read_or_log("xclip", &["-sel", "clip", "-o", "-t", spec.source_mime], "读取 X11 剪贴板失败")
                else {
                    continue;
                };
                let current_hash = calc_hash(&x_data, spec.process_mode);
                if current_hash == EMPTY_HASH || !begin_sync(&state_x2w, SyncDir::X2W, current_hash)
                {
                    continue;
                }

                log(
                    "X2W",
                    &format!(
                        "写入 Wayland... (Hash: {:08x})",
                        (current_hash >> 96) as u32
                    ),
                );
                let write_data = if needs_uri_normalization(spec.process_mode) {
                    normalize_uri_list(&x_data)
                } else {
                    x_data
                };
                if write_or_log(
                    "wl-copy",
                    &["-t", spec.sync_mime],
                    &write_data,
                    "写入 Wayland 剪贴板失败",
                ) {
                    finish_sync(&state_x2w, current_hash);
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
        if precheck_sync(&shared_state, SyncDir::W2X) {
            continue;
        }
        let Some(types_raw) = read_or_log("wl-paste", &["--list-types"], "读取 Wayland TARGETS 失败")
        else {
            continue;
        };
        let types_str = String::from_utf8_lossy(&types_raw);
        let Some(spec) = detect_wayland_clipboard_spec(&types_str) else {
            continue;
        };
        let Some(w_data) = read_or_log("wl-paste", &["-t", spec.sync_mime], "读取 Wayland 剪贴板失败")
        else {
            continue;
        };
        let current_hash = calc_hash(&w_data, spec.process_mode);
        if current_hash == EMPTY_HASH || !begin_sync(&shared_state, SyncDir::W2X, current_hash) {
            continue;
        }

        log(
            "W2X",
            &format!("写入 X11... (Hash: {:08x})", (current_hash >> 96) as u32),
        );
        let write_data = if needs_uri_normalization(spec.process_mode) {
            normalize_uri_list(&w_data)
        } else {
            w_data
        };

        let target_t = match spec.sync_mime {
            "text/plain;charset=utf-8" | "text/plain" => "UTF8_STRING",
            other => other,
        };
        if write_or_log(
            "xclip",
            &["-sel", "clip", "-i", "-t", target_t],
            &write_data,
            "写入 X11 剪贴板失败",
        ) {
            finish_sync(&shared_state, current_hash);
        }
    }

    log("ERROR", "W2X 监听意外终止，触发退出...");
    std::process::exit(1);
}
