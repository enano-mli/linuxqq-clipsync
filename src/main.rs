use chrono::Utc;
use memfd::MemfdOptions;
use std::env;
use std::fs;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use xxhash_rust::xxh3::xxh3_128;

mod convert;
mod x11_owner;

struct SyncState {
    last_dir: String,
    last_time: i64,
    last_sync_hash: u128,
}

const EMPTY_HASH: u128 = 0;
const CLIPBOARD_READ_TIMEOUT: Duration = Duration::from_secs(10);
const CLIPBOARD_WRITE_TIMEOUT: Duration = Duration::from_secs(10);

fn log(level: &str, msg: &str) {
    let now = Utc::now().format("%H:%M:%S").to_string();
    println!("[{}] [{}] {}", now, level, msg);
}

fn get_ms() -> i64 {
    Utc::now().timestamp_millis()
}

fn wait_with_timeout(mut child: Child, timeout: Duration) -> Option<ExitStatus> {
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) if start.elapsed() >= timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Ok(None) => thread::sleep(Duration::from_millis(10)),
            Err(_) => return None,
        }
    }
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

// ==========================================
// 核心机制：无管道读取与写入 (基于 memfd)
// ==========================================

fn read_clipboard(cmd: &str, args: &[&str]) -> Vec<u8> {
    let Ok(mfd) = MemfdOptions::default().create("clip_read") else {
        return vec![];
    };
    let file = mfd.into_file();
    let Ok(file_out) = file.try_clone() else {
        return vec![];
    };

    if let Ok(child) = Command::new(cmd)
        .args(args)
        .stdout(Stdio::from(file_out))
        .stderr(Stdio::null())
        .spawn()
    {
        if wait_with_timeout(child, CLIPBOARD_READ_TIMEOUT).is_none() {
            log(
                "WARN",
                &format!("读取剪贴板超时: {} {}", cmd, args.join(" ")),
            );
            return vec![];
        }
    }

    let mut data = Vec::new();
    let mut file_read = file;
    let _ = file_read.seek(SeekFrom::Start(0));
    let _ = file_read.read_to_end(&mut data);
    data
}

fn write_clipboard(cmd: &str, args: &[&str], data: &[u8]) -> bool {
    let Ok(mfd) = MemfdOptions::default().create("clip_write") else {
        return false;
    };
    let mut file = mfd.into_file();
    if file.write_all(data).is_err() {
        return false;
    }
    if file.seek(SeekFrom::Start(0)).is_err() {
        return false;
    }

    if let Ok(child) = Command::new(cmd)
        .args(args)
        .stdin(Stdio::from(file))
        .stderr(Stdio::null())
        .spawn()
    {
        return wait_with_timeout(child, CLIPBOARD_WRITE_TIMEOUT)
            .map(|s| s.success())
            .unwrap_or_else(|| {
                log(
                    "WARN",
                    &format!("写入剪贴板超时: {} {}", cmd, args.join(" ")),
                );
                false
            });
    }
    false
}

fn get_xdg_runtime_dir() -> String {
    if let Ok(dir) = env::var("XDG_RUNTIME_DIR") {
        return dir;
    }
    format!("/run/user/{}", unsafe { libc::getuid() })
}

// CLIPSYNC_DEBUG=1 时输出剪贴板原始字节前缀，用于排查编码类问题
fn debug_enabled() -> bool {
    static DEBUG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *DEBUG.get_or_init(|| env::var("CLIPSYNC_DEBUG").unwrap_or_default() == "1")
}

fn debug_dump(tag: &str, types: &str, data: &[u8]) {
    if !debug_enabled() {
        return;
    }
    let hex: String = data
        .iter()
        .take(48)
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    let clean_types: Vec<&str> = types.lines().map(str::trim).filter(|s| !s.is_empty()).collect();
    log(
        "DEBUG",
        &format!("[{tag}] types=[{}] len={} head={hex}", clean_types.join(","), data.len()),
    );
}

// png → BMP3（wine CF_DIB 需要）。CLIPSYNC_BMP_CONV=magick 时切回
// ImageMagick 管道（与旧企微桥行为逐字节一致的保底路径）。
fn make_bmp(png: &[u8]) -> Option<Vec<u8>> {
    if env::var("CLIPSYNC_BMP_CONV").unwrap_or_default() == "magick" {
        return magick_filter(&["png:-", "BMP3:-"], png);
    }
    match convert::png_to_bmp3(png) {
        Ok(b) => Some(b),
        Err(e) => {
            log("WARN", &format!("png→bmp: {e}"));
            None
        }
    }
}

// bmp → png（X2W 反向同步：企微内复制的 bmp 也能同步出去）
fn make_png(bmp: &[u8]) -> Option<Vec<u8>> {
    if env::var("CLIPSYNC_BMP_CONV").unwrap_or_default() == "magick" {
        return magick_filter(&["BMP:-", "PNG:-"], bmp);
    }
    match convert::bmp_to_png(bmp) {
        Ok(p) => Some(p),
        Err(e) => {
            log("WARN", &format!("bmp→png: {e}"));
            None
        }
    }
}

// 经 ImageMagick stdin/stdout 过滤数据（memfd 无管道，带超时强杀）
fn magick_filter(args: &[&str], input: &[u8]) -> Option<Vec<u8>> {
    let out_mfd = MemfdOptions::default().create("magick_out").ok()?;
    let mut out_file = out_mfd.into_file();
    let out_dup = out_file.try_clone().ok()?;
    let in_mfd = MemfdOptions::default().create("magick_in").ok()?;
    let mut in_file = in_mfd.into_file();
    in_file.write_all(input).ok()?;
    let _ = in_file.seek(SeekFrom::Start(0));

    let child = Command::new("magick")
        .args(args)
        .stdin(Stdio::from(in_file))
        .stdout(Stdio::from(out_dup))
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    if wait_with_timeout(child, CLIPBOARD_WRITE_TIMEOUT).is_none() {
        log("WARN", &format!("magick 超时: {}", args.join(" ")));
        return None;
    }

    let mut data = Vec::new();
    let _ = out_file.seek(SeekFrom::Start(0));
    out_file.read_to_end(&mut data).ok()?;
    if data.is_empty() {
        None
    } else {
        Some(data)
    }
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
        last_dir: String::new(),
        last_time: 0,
        last_sync_hash: EMPTY_HASH,
    }));

    // W2X 写入通道：自研 x11 持有者（多 target + ICCCM 合规拒绝），
    // CLIPSYNC_W2X_OWNER=xclip 可整体回退旧 xclip 单 target 路径
    let x11_owner = if env::var("CLIPSYNC_W2X_OWNER").unwrap_or_default() == "xclip" {
        log("INIT", "W2X 写入模式: xclip（环境变量指定回退）");
        None
    } else {
        let owner = x11_owner::X11Owner::spawn();
        log(
            "INIT",
            if owner.is_some() {
                "W2X 写入模式: x11-owner（多 target）"
            } else {
                "W2X 写入模式: xclip（owner 启动失败，自动回退）"
            },
        );
        owner
    };

    // ==========================================
    // X2W 线程
    // ==========================================
    let state_x2w = Arc::clone(&shared_state);
    thread::spawn(move || {
        log("INFO", "=== [X2W] 线程启动 ===");
        loop {
            let _ = Command::new("clipnotify").status();
            thread::sleep(Duration::from_millis(30));

            {
                let state = state_x2w.lock().unwrap();
                let now = get_ms();
                if state.last_dir == "W2X" && (now - state.last_time < 1000) {
                    continue;
                }
            }

            let types_raw =
                read_clipboard("xclip", &["-selection", "clipboard", "-t", "TARGETS", "-o"]);
            let types_str = String::from_utf8_lossy(&types_raw);

            let (source_mime, sync_mime, process_mode) =
                // 图片优先于 uri-list：QQ 等应用复制图片时同时提供临时文件路径
                // (uri-list) 和图像本体，若先匹配 uri-list 会把"复制图片"变成
                // "复制路径"（wine 还会优先拿 uri-list 映射成 HDROP 文件粘贴）
                if types_str.contains("image/png") {
                    ("image/png", "image/png", "raw")
                } else if types_str.contains("image/jpeg") {
                    ("image/jpeg", "image/jpeg", "raw")
                } else if types_str.contains("image/bmp") {
                    // 企微（wine）复制的 bmp：转 png 后再同步到 Wayland
                    ("image/bmp", "image/png", "bmp")
                } else if types_str.contains("x-special/gnome-copied-files") {
                    ("x-special/gnome-copied-files", "text/uri-list", "uri-list")
                } else if types_str.contains("application/x-qt-image")
                    || types_str.contains("text/uri-list")
                {
                    ("text/uri-list", "text/uri-list", "uri-list")
                } else if types_str.contains("text/plain;charset=utf-8") {
                    ("text/plain;charset=utf-8", "text/plain", "text")
                } else if types_str.contains("UTF8_STRING") {
                    ("UTF8_STRING", "text/plain", "text")
                } else if types_str.contains("text/plain") {
                    ("text/plain", "text/plain", "text")
                } else if types_str.contains("text/html") {
                    ("text/html", "text/html", "raw")
                } else {
                    continue;
                };

            let x_data = read_clipboard("xclip", &["-sel", "clip", "-o", "-t", source_mime]);
            debug_dump("X2W-read", &types_str, &x_data);
            // bmp → png 转换放在 hash 之前：hash 落在转换产物上，
            // W2X 后续读到同一 png 时 hash 命中、不会二次回写
            let x_data = if process_mode == "bmp" {
                match make_png(&x_data) {
                    Some(png) => png,
                    None => {
                        log("WARN", "bmp→png 转换失败，跳过本次 X2W 同步");
                        continue;
                    }
                }
            } else {
                x_data
            };
            let process_mode = if process_mode == "bmp" {
                "raw"
            } else {
                process_mode
            };
            let current_hash = calc_hash(&x_data, process_mode);
            if current_hash == EMPTY_HASH {
                continue;
            }

            // 优化 3：移除极其冗余的二次目标查壳（w_check_data），直接依靠记录的 hash 防环
            {
                let state = state_x2w.lock().unwrap();
                if current_hash == state.last_sync_hash {
                    continue;
                }
            }

            log(
                "X2W",
                &format!(
                    "写入 Wayland... (Hash: {:08x})",
                    (current_hash >> 96) as u32
                ),
            );
            let write_data = if process_mode == "uri-list" {
                let s = String::from_utf8_lossy(&x_data);
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
            } else {
                x_data
            };

            // 在写入前就记录方向/时间/hash，避免写入触发的对向回环在状态更新前到达，
            // 导致对向线程回读大图（xclip 的 INCR 并发缺陷会丢弃并发的粘贴请求）。
            {
                let mut state = state_x2w.lock().unwrap();
                state.last_dir = "X2W".to_string();
                state.last_time = get_ms();
                state.last_sync_hash = current_hash;
            }

            write_clipboard("wl-copy", &["-t", sync_mime], &write_data);
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

        let types_raw = read_clipboard("wl-paste", &["--list-types"]);
        let types_str = String::from_utf8_lossy(&types_raw);

        let (sync_mime, process_mode) =
            // 与 X2W 同理：图像本体优先于 uri-list（临时文件路径）
            if types_str.contains("image/png") {
                ("image/png", "raw")
            } else if types_str.contains("image/jpeg") {
                ("image/jpeg", "raw")
            } else if types_str.contains("application/x-qt-image")
                || types_str.contains("text/uri-list")
            {
                ("text/uri-list", "uri-list")
            } else if types_str.contains("text/plain;charset=utf-8") {
            ("text/plain;charset=utf-8", "text")
        } else if types_str.contains("text/plain") {
            ("text/plain", "text")
        } else if types_str.contains("text/html") {
            ("text/html", "raw")
        } else {
            continue;
        };

        let w_data = read_clipboard("wl-paste", &["-n", "-t", sync_mime]);
        debug_dump("W2X-read", &types_str, &w_data);
        let current_hash = calc_hash(&w_data, process_mode);
        if current_hash == EMPTY_HASH {
            continue;
        }

        // 优化 3：移除 x_check_data 的大量多余 IO。
        {
            let state = shared_state.lock().unwrap();
            if current_hash == state.last_sync_hash {
                continue;
            }
        }

        log(
            "W2X",
            &format!("写入 X11... (Hash: {:08x})", (current_hash >> 96) as u32),
        );
        let write_data = if process_mode == "uri-list" {
            let s = String::from_utf8_lossy(&w_data);
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
        } else {
            w_data
        };

        let target_t = match sync_mime {
            "text/plain;charset=utf-8" | "text/plain" => "UTF8_STRING",
            other => other,
        };

        // 在写入前就记录方向/时间/hash，避免写入触发的对向回环在状态更新前到达，
        // 导致对向线程回读大图（xclip 的 INCR 并发缺陷会丢弃并发的粘贴请求）。
        {
            let mut state = shared_state.lock().unwrap();
            state.last_dir = "W2X".to_string();
            state.last_time = get_ms();
            state.last_sync_hash = current_hash;
        }

        // owner 路径：同一持有者同时服务多种 target（文本 3 种 / png+bmp 双格式）。
        // 文本不提供 COMPOUND_TEXT/STRING——xclip 对任意 target 倒原始 UTF-8 缓冲，
        // wine 按 COMPOUND_TEXT 解析非法序列导致中文变 '?'（2026-08-18 实测根因）；
        // 只列无损 target + 拒绝其余请求，请求方会回退到 UTF8_STRING。
        let asserted = if let Some(owner) = &x11_owner {
            let mut targets: Vec<(&str, Vec<u8>)> = Vec::new();
            if process_mode == "text" {
                targets.push(("UTF8_STRING", write_data.clone()));
                targets.push(("text/plain;charset=utf-8", write_data.clone()));
                targets.push(("text/plain", write_data.clone()));
            } else if sync_mime == "image/png" {
                targets.push(("image/png", write_data.clone()));
                match make_bmp(&write_data) {
                    Some(bmp) => targets.push(("image/bmp", bmp)),
                    None => log("WARN", "png→bmp 转换失败，本次仅提供 image/png"),
                }
            } else {
                targets.push((sync_mime, write_data.clone()));
            }
            owner.assert(targets)
        } else {
            false
        };
        if !asserted {
            write_clipboard(
                "xclip",
                &["-sel", "clip", "-i", "-t", target_t],
                &write_data,
            );
        }
    }

    log("ERROR", "W2X 监听意外终止，触发退出...");
    std::process::exit(1);
}
