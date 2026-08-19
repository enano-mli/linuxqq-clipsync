// x11_owner.rs — 自研 X11 CLIPBOARD 持有者
//
// 为什么不用 xclip -i（历史缺陷，2026-08-18 实测确认）：
//   1. 单进程只能服务一种 target，企微桥必须另起持有者提供 image/bmp，
//      两者互抢所有权（排障文档 2.4/2.5 的竞态根源）；
//   2. 对请求方请求的任何 target（含 COMPOUND_TEXT/STRING）都原样返回
//      UTF-8 缓冲区，wine 按 COMPOUND_TEXT 解析非法序列 → 中文全部变 '?'。
//
// 本模块：同一 owner 同时提供多种 target；TARGETS 只列出能无损服务的格式；
// 未列出的请求按 ICCCM 以 property=None 拒绝，请求方会回退到它支持的格式
// （wine → UTF8_STRING）。大负载走 INCR 流式传输。

use std::collections::HashMap;
use std::fs::File;
use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use x11rb::connection::{Connection, RequestConnection};
use x11rb::errors::ConnectionError;
use x11rb::protocol::xproto::{
    AtomEnum, ChangeWindowAttributesAux, ConnectionExt, CreateWindowAux, EventMask, PropMode,
    Property, SelectionNotifyEvent, WindowClass, SELECTION_NOTIFY_EVENT,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;

type Atom = u32;

enum OwnerCmd {
    /// 接管 CLIPBOARD 所有权并服务给定 targets（名称 → 数据）
    Assert(Vec<(String, Vec<u8>)>),
}

struct Payload {
    atoms: Vec<Atom>,             // TARGETS 应答，顺序即优先级
    data: HashMap<Atom, Vec<u8>>, // 原子 → 字节
    acquired_at: u32,             // 获得所有权的服务器时间戳
}

struct PendingIncr {
    requestor: u32,
    property: Atom,
    target_type: Atom,
    data: Vec<u8>,
    offset: usize,
}

pub struct X11Owner {
    tx: Sender<OwnerCmd>,
    /// 写端唤醒管道：命令入队后唤醒阻塞在 poll 上的事件线程
    wake: File,
}

impl X11Owner {
    /// 连接 DISPLAY 并启动持有者事件线程。失败返回 None，调用方回退 xclip 路径。
    pub fn spawn() -> Option<X11Owner> {
        let (tx, rx) = mpsc::channel();
        let mut fds = [0i32; 2];
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_NONBLOCK | libc::O_CLOEXEC) } != 0 {
            return None;
        }
        let (rfd, wfd) = (fds[0], fds[1]);
        thread::Builder::new()
            .name("x11-owner".into())
            .spawn(move || match OwnerThread::new(rfd) {
                Some(t) => t.run(rx),
                None => log("WARN", "x11 持有者线程启动失败，W2X 将回退 xclip"),
            })
            .ok()?;
        Some(X11Owner {
            tx,
            wake: unsafe { File::from_raw_fd(wfd) },
        })
    }

    /// 非阻塞断言所有权；线程已死（发送失败）时返回 false，调用方回退 xclip。
    pub fn assert(&self, targets: Vec<(&str, Vec<u8>)>) -> bool {
        let sent = self
            .tx
            .send(OwnerCmd::Assert(
                targets
                    .into_iter()
                    .map(|(n, d)| (n.to_string(), d))
                    .collect(),
            ))
            .is_ok();
        if sent {
            let _ = (&self.wake).write(&[1]);
        }
        sent
    }
}

fn log(level: &str, msg: &str) {
    println!(
        "[{}] [{}] {}",
        chrono::Utc::now().format("%H:%M:%S"),
        level,
        msg
    );
}

// X 协议字节序为小端，本机 x86_64 即小端，to_ne_bytes 可直接用
fn le_words(words: &[u32]) -> Vec<u8> {
    words.iter().flat_map(|w| w.to_ne_bytes()).collect()
}

struct OwnerThread {
    conn: RustConnection,
    win: u32,
    wake_fd: RawFd,
    atom_clipboard: Atom,
    atom_targets: Atom,
    atom_timestamp: Atom,
    atom_atom: Atom,
    atom_incr: Atom,
    atom_png: Atom,
    atom_bmp: Atom,
    atom_urilist: Atom,
    atom_wm_class: Atom,
    interned: HashMap<String, Atom>,
    /// 请求方窗口 → 是否 wine 客户端（按 WM_CLASS 识别，缓存）
    wine_cache: HashMap<u32, bool>,
    dummy_prop: Atom,
    payload: Option<Payload>,
    pending: Vec<PendingIncr>,
    chunk: usize,
    waiting_ts: bool,
}

impl OwnerThread {
    fn new(wake_fd: RawFd) -> Option<OwnerThread> {
        let (conn, screen_num) = RustConnection::connect(None).ok()?;
        let win = conn.generate_id().ok()?;
        let root = conn.setup().roots.get(screen_num)?.root;
        conn.create_window(
            x11rb::COPY_DEPTH_FROM_PARENT,
            win,
            root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::INPUT_OUTPUT,
            x11rb::COPY_FROM_PARENT,
            &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
        )
        .ok()?
        .check()
        .ok()?;

        let mut t = OwnerThread {
            conn,
            win,
            wake_fd,
            atom_clipboard: 0,
            atom_targets: 0,
            atom_timestamp: 0,
            atom_atom: AtomEnum::ATOM.into(),
            atom_incr: 0,
            atom_png: 0,
            atom_bmp: 0,
            atom_urilist: 0,
            atom_wm_class: 0,
            interned: HashMap::new(),
            wine_cache: HashMap::new(),
            dummy_prop: 0,
            payload: None,
            pending: Vec::new(),
            chunk: 0,
            waiting_ts: false,
        };
        t.atom_clipboard = t.intern("CLIPBOARD")?;
        t.atom_targets = t.intern("TARGETS")?;
        t.atom_timestamp = t.intern("TIMESTAMP")?;
        t.atom_incr = t.intern("INCR")?;
        t.atom_png = t.intern("image/png")?;
        t.atom_bmp = t.intern("image/bmp")?;
        t.atom_urilist = t.intern("text/uri-list")?;
        t.atom_wm_class = t.intern("WM_CLASS")?;
        t.dummy_prop = t.intern("CLIPSYNC_TS")?;

        // INCR 分块阈值：尽量用满服务器最大请求长度（BIG-REQUESTS 通常给到
        // 很大），让常规截图走单次属性直传、完全绕开 INCR 协议交互
        t.chunk = t.conn.maximum_request_bytes().saturating_sub(512) & !3usize;
        log("INIT", &format!("[X11-Owner] 单次直传上限 {} 字节", t.chunk));
        Some(t)
    }

    fn intern(&mut self, name: &str) -> Option<Atom> {
        if let Some(&a) = self.interned.get(name) {
            return Some(a);
        }
        let r = self
            .conn
            .intern_atom(false, name.as_bytes())
            .ok()?
            .reply()
            .ok()?;
        self.interned.insert(name.to_string(), r.atom);
        Some(r.atom)
    }

    /// 发送 format=8 的属性写入（fire-and-forget，调用方负责 flush）
    fn prop8(&self, win: u32, prop: Atom, type_: Atom, data: &[u8]) -> bool {
        self.conn
            .change_property(
                PropMode::REPLACE,
                win,
                prop,
                type_,
                8,
                data.len() as u32,
                data,
            )
            .is_ok()
    }

    /// 发送 format=32 的属性写入（fire-and-forget，调用方负责 flush）
    fn prop32(&self, win: u32, prop: Atom, type_: Atom, words: &[u32]) -> bool {
        let bytes = le_words(words);
        self.conn
            .change_property(
                PropMode::REPLACE,
                win,
                prop,
                type_,
                32,
                words.len() as u32,
                &bytes,
            )
            .is_ok()
    }

    fn run(mut self, rx: Receiver<OwnerCmd>) {
        log("INFO", "=== [X11-Owner] 持有者线程就绪 ===");
        let xfd = self.conn.stream().as_raw_fd();
        loop {
            // 1) 处理待处理命令
            loop {
                match rx.try_recv() {
                    Ok(OwnerCmd::Assert(targets)) => self.begin_assert(targets),
                    Err(_) => break,
                }
            }
            // 2) 处理所有已到达事件（poll_for_event 会非阻塞读 socket）
            let mut had_error = false;
            while let Ok(Some(ev)) = self.conn.poll_for_event() {
                self.handle_event(ev);
            }
            if let Err(e) = self.conn.poll_for_event() {
                if matches!(e, ConnectionError::IoError(_)) {
                    log("WARN", &format!("[X11-Owner] X 连接断开: {e}，线程退出"));
                    return;
                }
                had_error = true;
            }
            if had_error {
                // 短暂退避避免错误风暴
                thread::sleep(std::time::Duration::from_millis(50));
            }
            // 3) 阻塞等待 X socket 或唤醒管道（关键：应答延迟必须为毫秒级，
            //    xclip 等客户端等待 SelectionNotify 的窗口只有几十毫秒）
            let mut fds = [
                libc::pollfd {
                    fd: xfd,
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: self.wake_fd,
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            let r = unsafe { libc::poll(fds.as_mut_ptr(), 2, 60_000) };
            if r < 0 {
                // EINTR 等信号打断，直接重试
                continue;
            }
            if fds[1].revents & (libc::POLLIN as i16) != 0 {
                let mut buf = [0u8; 64];
                let _ = unsafe {
                    libc::read(self.wake_fd, buf.as_mut_ptr().cast(), buf.len())
                };
            }
        }
    }

    fn begin_assert(&mut self, targets: Vec<(String, Vec<u8>)>) {
        let mut atoms = Vec::new();
        let mut data = HashMap::new();
        for (name, bytes) in &targets {
            if let Some(a) = self.intern(name) {
                atoms.push(a);
                data.insert(a, bytes.clone());
            }
        }
        if atoms.is_empty() {
            return;
        }
        // 数据先就位再取时间戳/所有权，保证所有权生效瞬间即可服务
        self.payload = Some(Payload {
            atoms,
            data,
            acquired_at: 0,
        });
        // 在自家窗口做哑属性变更，用 PropertyNotify 拿服务器时间戳后再
        // SetSelectionOwner（事件与命令都在本线程串行处理，顺序有保证）
        self.waiting_ts = true;
        let _ = self.prop32(self.win, self.dummy_prop, self.atom_atom, &[0]);
        let _ = self.conn.flush();
    }

    fn take_ownership(&mut self, ts: u32) {
        if let Some(p) = self.payload.as_mut() {
            p.acquired_at = ts;
        }
        if self
            .conn
            .set_selection_owner(self.win, self.atom_clipboard, ts)
            .is_err()
        {
            return;
        }
        let _ = self.conn.flush();
        // 确认真的拿到了所有权（防竞态：别人同一刻也断言）
        let got = self
            .conn
            .get_selection_owner(self.atom_clipboard)
            .ok()
            .and_then(|c| c.reply().ok());
        match got {
            Some(r) if r.owner == self.win => {
                let n = self.payload.as_ref().map(|p| p.atoms.len()).unwrap_or(0);
                log(
                    "X11-Owner",
                    &format!("已接管 CLIPBOARD（{n} targets, ts={ts}）"),
                );
            }
            _ => {
                log("WARN", "[X11-Owner] 接管失败（被抢先），本次断言作废");
                self.payload = None;
            }
        }
    }

    fn handle_event(&mut self, ev: Event) {
        match ev {
            Event::PropertyNotify(e) => {
                if e.window == self.win
                    && e.atom == self.dummy_prop
                    && e.state == Property::NEW_VALUE
                    && self.waiting_ts
                {
                    self.waiting_ts = false;
                    self.take_ownership(e.time);
                } else if e.state == Property::DELETE {
                    // INCR：请求方已读走上一块，发下一块
                    self.advance_incr(e.window, e.atom);
                }
            }
            Event::SelectionClear(_) => {
                if self.payload.is_some() {
                    log("X11-Owner", "失去 CLIPBOARD 所有权");
                }
                self.payload = None;
                // ICCCM：已开始的 INCR 传输继续服务完（保留 pending）
            }
            Event::SelectionRequest(e) => self.handle_request(e),
            Event::Error(err) => {
                // 未检查请求的服务端错误（BadAtom/BadWindow/BadValue 等）会以
                // 错误事件形式到达——必须暴露出来，否则属性写入失败无声无息
                log("WARN", &format!("[X11-Owner] X 协议错误: {err:?}"));
            }
            _ => {}
        }
    }

    fn handle_request(&mut self, e: x11rb::protocol::xproto::SelectionRequestEvent) {
        if e.selection != self.atom_clipboard {
            log(
                "X11-Owner",
                &format!("拒绝非 CLIPBOARD 请求 target=0x{:x}", e.target),
            );
            self.notify(e.requestor, e.selection, e.target, 0, e.time);
            return;
        }
        // ICCCM：property=None 时以 target 原子作为属性名（xclip 等客户端的惯例）
        let property = if e.property == 0 { e.target } else { e.property };

        // 一次性提取所需数据并结束不可变借用（后续要调用 &mut self 的方法）
        let payload_info = self.payload.as_ref().map(|p| {
            (
                p.atoms.clone(),
                p.acquired_at,
                p.data.get(&e.target).cloned(),
                p.data.contains_key(&self.atom_png) && p.data.contains_key(&self.atom_bmp),
            )
        });
        let Some((atoms_list, acquired_at, target_data, has_dual)) = payload_info else {
            // 已不持有：拒绝
            self.notify(e.requestor, e.selection, e.target, 0, e.time);
            return;
        };

        // TARGETS / TIMESTAMP 元 target
        if e.target == self.atom_targets {
            // wine（企微/微信）按 WM_CLASS 识别（res_class 形如 "Wxwork.exe"）。
            // wine 的格式优先级：image/png → 自定义 "PNG" 格式（应用不认），
            // text/uri-list → HDROP 文件粘贴，都会抢在 image/bmp(CF_DIB) 之前。
            // 因此对 wine 隐藏 png 和 uri-list，只让它看到 bmp 和文本 target
            // （仅在 bmp 确实可用时过滤，否则保留 png 兜底）。
            let mut atoms = atoms_list;
            if has_dual && self.requestor_is_wine(e.requestor) {
                let before = atoms.len();
                atoms.retain(|a| *a != self.atom_png && *a != self.atom_urilist);
                log(
                    "X11-Owner",
                    &format!(
                        "wine 请求方 0x{:x}：TARGETS 过滤 {}→{}（隐藏 png/uri-list，保 bmp）",
                        e.requestor,
                        before,
                        atoms.len()
                    ),
                );
            }
            let ok = self.prop32(e.requestor, property, self.atom_atom, &atoms);
            let _ = self.conn.flush();
            log(
                "X11-Owner",
                &format!(
                    "元请求 {} → 0x{:x} {}",
                    self.atom_name(e.target),
                    e.requestor,
                    if ok { "已应答" } else { "写入失败" }
                ),
            );
            self.notify(
                e.requestor,
                e.selection,
                e.target,
                if ok { property } else { 0 },
                e.time,
            );
            return;
        }
        if e.target == self.atom_timestamp {
            let ok = self.prop32(e.requestor, property, self.atom_timestamp, &[acquired_at]);
            let _ = self.conn.flush();
            self.notify(
                e.requestor,
                e.selection,
                e.target,
                if ok { property } else { 0 },
                e.time,
            );
            return;
        }

        // wine 直连过滤：wine 可能凭缓存的格式信息跳过 TARGETS 直接请求
        // image/png（拿到会映射成应用不认的自定义 PNG 格式）或 text/uri-list
        // （HDROP 文件粘贴）。bmp 可用时对 wine 拒绝这两者，逼其回退 bmp。
        if has_dual
            && self.requestor_is_wine(e.requestor)
            && (e.target == self.atom_png || e.target == self.atom_urilist)
        {
            log(
                "X11-Owner",
                &format!(
                    "wine 直连请求 {} 已拒绝（回退 bmp）→ 0x{:x}",
                    self.atom_name(e.target),
                    e.requestor
                ),
            );
            self.notify(e.requestor, e.selection, e.target, 0, e.time);
            return;
        }

        // 数据 target：只服务已列出的（ICCCM 合规拒绝，请求方回退其他格式）
        let Some(data) = target_data else {
            log(
                "X11-Owner",
                &format!(
                    "拒绝未提供的目标 {} → 0x{:x}",
                    self.atom_name(e.target),
                    e.requestor
                ),
            );
            self.notify(e.requestor, e.selection, e.target, 0, e.time);
            return;
        };

        if data.len() <= self.chunk {
            let ok = self.prop8(e.requestor, property, e.target, &data);
            let _ = self.conn.flush();
            log(
                "X11-Owner",
                &format!(
                    "直传 {} {} 字节 → 0x{:x}",
                    self.atom_name(e.target),
                    data.len(),
                    e.requestor
                ),
            );
            self.notify(
                e.requestor,
                e.selection,
                e.target,
                if ok { property } else { 0 },
                e.time,
            );
        } else {
            // INCR 流式：声明总长度，等请求方逐块来取
            let ok = self
                .conn
                .change_window_attributes(
                    e.requestor,
                    &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE),
                )
                .is_ok();
            let declared = self.prop32(e.requestor, property, self.atom_incr, &[data.len() as u32]);
            let _ = self.conn.flush();
            if ok && declared {
                log(
                    "X11-Owner",
                    &format!(
                        "INCR 传输开始: {} 字节 → 0x{:x}",
                        data.len(),
                        e.requestor
                    ),
                );
                self.pending.push(PendingIncr {
                    requestor: e.requestor,
                    property,
                    target_type: e.target,
                    data,
                    offset: 0,
                });
                self.notify(e.requestor, e.selection, e.target, property, e.time);
            } else {
                self.notify(e.requestor, e.selection, e.target, 0, e.time);
            }
        }
    }

    /// 请求方是否 wine 客户端：读其窗口 WM_CLASS（形如 "wxwork\0Wxwork.exe"），
    /// 结果缓存。wine 的所有窗口 res_class 均带 .exe 后缀。
    fn requestor_is_wine(&mut self, win: u32) -> bool {
        if let Some(&v) = self.wine_cache.get(&win) {
            return v;
        }
        let any_type: Atom = AtomEnum::ANY.into();
        let class = self
            .conn
            .get_property(false, win, self.atom_wm_class, any_type, 0, 64)
            .ok()
            .and_then(|c| c.reply().ok())
            .map(|r| String::from_utf8_lossy(&r.value).to_lowercase())
            .unwrap_or_default();
        let is_wine = class.contains(".exe") || class.contains("wine");
        if !class.is_empty() {
            log(
                "X11-Owner",
                &format!("请求方 0x{win:x} WM_CLASS=\"{class}\" wine={is_wine}"),
            );
        }
        self.wine_cache.insert(win, is_wine);
        is_wine
    }

    fn atom_name(&self, atom: Atom) -> String {
        let known = [
            (self.atom_targets, "TARGETS"),
            (self.atom_timestamp, "TIMESTAMP"),
            (self.atom_incr, "INCR"),
            (self.atom_clipboard, "CLIPBOARD"),
        ];
        for (a, n) in known {
            if a == atom {
                return n.to_string();
            }
        }
        for (name, a) in &self.interned {
            if *a == atom {
                return name.clone();
            }
        }
        format!("atom-{atom}")
    }

    fn advance_incr(&mut self, requestor: u32, property: Atom) {
        let Some(idx) = self
            .pending
            .iter()
            .position(|p| p.requestor == requestor && p.property == property)
        else {
            return;
        };
        let p = &mut self.pending[idx];
        let end = (p.offset + self.chunk).min(p.data.len());
        let bytes = p.data[p.offset..end].to_vec();
        let target_type = p.target_type;
        p.offset = end;

        let ok = self.prop8(requestor, property, target_type, &bytes);
        let _ = self.conn.flush();
        if !ok || bytes.is_empty() {
            if bytes.is_empty() {
                log("X11-Owner", "INCR 传输完成");
            }
            self.pending.remove(idx);
        }
    }

    /// SelectionNotify：property=0 表示拒绝
    fn notify(&mut self, requestor: u32, selection: Atom, target: Atom, property: Atom, time: u32) {
        // 注意：SelectionNotify 线格式字段顺序是 time/requestor/selection/target/property，
        // 与其他事件不同，必须用 x11rb 官方序列化（手写顺序踩过坑）
        let ev = SelectionNotifyEvent {
            response_type: SELECTION_NOTIFY_EVENT,
            sequence: 0,
            requestor,
            selection,
            target,
            property,
            time,
        };
        if self
            .conn
            .send_event(false, requestor, EventMask::NO_EVENT, ev)
            .is_ok()
        {
            let _ = self.conn.flush();
        }
    }
}
