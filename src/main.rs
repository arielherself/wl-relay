#![feature(array_chunks)]
#![allow(dead_code)]

use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::CStr;
use std::path::Path;
use std::sync::Arc;

use tokio::io::Interest;
use tokio::net::{UnixListener, UnixSocket};

use nix::sys::socket::Shutdown as NixShutdown;
use nix::sys::socket::{ControlMessage, ControlMessageOwned, MsgFlags, recvmsg, sendmsg};
use nix::unistd::close as nix_close;
use std::io::{IoSlice, IoSliceMut};
use std::os::unix::io::AsRawFd;
use tokio::net::UnixStream;
use tokio::sync::RwLock;

struct WLObjectRegistry {
    wl_display: u32,
    wl_registry: HashSet<u32>,
    wl_seat: HashSet<u32>,
    wl_seat_handle: u32,
    xdg_wm_base: HashSet<u32>,
    xdg_wm_base_handle: u32,
    xdg_surface: HashSet<u32>,
    xdg_toplevel: HashMap<u32, bool>,
    wl_pointer: HashSet<u32>,
    wl_keyboard: HashSet<u32>,
}

impl WLObjectRegistry {
    fn new() -> Self {
        Self {
            wl_display: 1,
            wl_registry: HashSet::new(),
            wl_seat: HashSet::new(),
            wl_seat_handle: 0,
            xdg_wm_base: HashSet::new(),
            xdg_wm_base_handle: 0,
            xdg_surface: HashSet::new(),
            xdg_toplevel: HashMap::new(),
            wl_pointer: HashSet::new(),
            wl_keyboard: HashSet::new(),
        }
    }
}

enum HandlerRole {
    InBound,
    OutBound,
}

enum HandlerState {
    Header,
    Body,
}

struct WLMessageHandler {
    role: HandlerRole,
    registry: Arc<RwLock<WLObjectRegistry>>,
    ptr: usize,
    len: usize,
    state: HandlerState,
    buf: [u8; 65536 + 8],
    output: VecDeque<Vec<u8>>,
}

impl WLMessageHandler {
    fn new(role: HandlerRole, registry: Arc<RwLock<WLObjectRegistry>>) -> Self {
        Self {
            role,
            registry,
            ptr: 0,
            len: 8,
            state: HandlerState::Header,
            buf: [0; _],
            output: VecDeque::new(),
        }
    }

    fn parse_header(x: [u8; 8]) -> (u32, u32, u32) {
        let object_id = u32::from_ne_bytes(x[0..4].try_into().unwrap());
        let metadata = u32::from_ne_bytes(x[4..8].try_into().unwrap());
        let size = metadata >> 16;
        let opcode = metadata << 16 >> 16;
        (object_id, size, opcode)
    }

    fn set_size(&mut self, new_size: u32) {
        let (object_id, _, opcode) = Self::parse_header(self.buf[0..8].try_into().unwrap());
        let metadata = new_size << 16 | opcode;
        self.buf[0..4].copy_from_slice(&object_id.to_ne_bytes());
        self.buf[4..8].copy_from_slice(&metadata.to_ne_bytes());
    }

    async fn append_word(&mut self, x: [u8; 4]) {
        self.buf[self.ptr..self.ptr + 4].copy_from_slice(&x);
        self.ptr += 4;
        if self.ptr == self.len && matches!(self.state, HandlerState::Header) {
            let (_object_id, size, _opcode) =
                Self::parse_header(self.buf[0..8].try_into().unwrap());
            debug_assert!(size >= 8);
            self.len = size as usize;
            self.state = HandlerState::Body;
        }

        if self.ptr == self.len && matches!(self.state, HandlerState::Body) {
            let (object_id, size, opcode) = Self::parse_header(self.buf[0..8].try_into().unwrap());
            let mut to_drop = false;
            match self.role {
                HandlerRole::OutBound => {
                    let wl_display = self.registry.read().await.wl_display;
                    let wl_registry = self.registry.read().await.wl_registry.clone();
                    let wl_seat = self.registry.read().await.wl_seat.clone();
                    let xdg_wm_base = self.registry.read().await.xdg_wm_base.clone();
                    let xdg_surface = self.registry.read().await.xdg_surface.clone();
                    if object_id == wl_display {
                        if opcode == 1 {
                            // get_registry
                            debug_assert!(size == 12);
                            let registry_id =
                                u32::from_ne_bytes(self.buf[8..12].try_into().unwrap());
                            eprintln!("bind registry_id = {}", registry_id);
                            self.registry.write().await.wl_registry.insert(registry_id);
                        }
                    } else if wl_registry.contains(&object_id) {
                        if opcode == 0 {
                            // bind
                            let wl_seat_handle = self.registry.read().await.wl_seat_handle;
                            let xdg_wm_base_handle = self.registry.read().await.xdg_wm_base_handle;
                            let handle = u32::from_ne_bytes(self.buf[8..12].try_into().unwrap());
                            let name_len =
                                (u32::from_ne_bytes(self.buf[12..16].try_into().unwrap()) + 3) / 4
                                    * 4;
                            debug_assert!(24 + name_len == size);
                            let _name =
                                CStr::from_bytes_until_nul(&self.buf[16..16 + name_len as usize])
                                    .unwrap()
                                    .to_str()
                                    .unwrap();
                            let _version = u32::from_ne_bytes(
                                self.buf[16 + name_len as usize..20 + name_len as usize]
                                    .try_into()
                                    .unwrap(),
                            );
                            let id = u32::from_ne_bytes(
                                self.buf[20 + name_len as usize..size as usize]
                                    .try_into()
                                    .unwrap(),
                            );
                            if handle == wl_seat_handle {
                                eprintln!("bind wl_seat_id = {}", id);
                                self.registry.write().await.wl_seat.insert(id);
                            } else if handle == xdg_wm_base_handle {
                                eprintln!("bind xdg_wm_base_id = {}", id);
                                self.registry.write().await.xdg_wm_base.insert(id);
                            }
                        }
                    } else if wl_seat.contains(&object_id) {
                        if opcode == 0 {
                            // get_pointer
                            debug_assert!(size == 12);
                            let id = u32::from_ne_bytes(self.buf[8..12].try_into().unwrap());
                            eprintln!("bind wl_pointer_id = {}", id);
                            self.registry.write().await.wl_pointer.insert(id);
                        } else if opcode == 1 {
                            // get_keyboard
                            debug_assert!(size == 12);
                            let id = u32::from_ne_bytes(self.buf[8..12].try_into().unwrap());
                            eprintln!("bind wl_keyboard_id = {}", id);
                            self.registry.write().await.wl_keyboard.insert(id);
                        }
                    } else if xdg_wm_base.contains(&object_id) {
                        if opcode == 2 {
                            // get_xdg_surface
                            let id = u32::from_ne_bytes(self.buf[8..12].try_into().unwrap());
                            eprintln!("new xdg_surface = {}", id);
                            self.registry.write().await.xdg_surface.insert(id);
                        }
                    } else if xdg_surface.contains(&object_id) {
                        if opcode == 1 {
                            // get_toplevel
                            debug_assert!(size == 12);
                            let id = u32::from_ne_bytes(self.buf[8..12].try_into().unwrap());
                            eprintln!("new xdg_toplevel = {}", id);
                            self.registry.write().await.xdg_toplevel.insert(id, false);
                        }
                    }
                }
                HandlerRole::InBound => {
                    let wl_registry = self.registry.read().await.wl_registry.clone();
                    let wl_pointer = self.registry.read().await.wl_pointer.clone();
                    let wl_keyboard = self.registry.read().await.wl_keyboard.clone();
                    let xdg_toplevel = self.registry.read().await.xdg_toplevel.clone();
                    if wl_registry.contains(&object_id) {
                        if opcode == 0 {
                            // global
                            let handle = u32::from_ne_bytes(self.buf[8..12].try_into().unwrap());
                            // including padding NUL bytes
                            let name_len =
                                (u32::from_ne_bytes(self.buf[12..16].try_into().unwrap()) + 3) / 4
                                    * 4;
                            debug_assert!(size == 20 + name_len);
                            let name =
                                CStr::from_bytes_until_nul(&self.buf[16..16 + name_len as usize])
                                    .unwrap()
                                    .to_str()
                                    .unwrap();
                            let _version = u32::from_ne_bytes(
                                self.buf[16 + name_len as usize..size as usize]
                                    .try_into()
                                    .unwrap(),
                            );
                            if name == "wl_seat" {
                                self.registry.write().await.wl_seat_handle = handle;
                            } else if name == "xdg_wm_base" {
                                self.registry.write().await.xdg_wm_base_handle = handle;
                            }
                        }
                    } else if wl_pointer.contains(&object_id) {
                        if opcode == 0 {
                            // enter
                            // eprintln!("pointer enter");
                        } else if opcode == 1 {
                            // leave
                            // eprintln!("pointer leave");
                        }
                    } else if wl_keyboard.contains(&object_id) {
                        if opcode == 1 {
                            // enter
                            eprintln!("keyboard enter");
                        } else if opcode == 2 {
                            // leave
                            eprintln!("keyboard leave (dropped)");
                            to_drop = true;
                        }
                    } else if xdg_toplevel.contains_key(&object_id) {
                        if opcode == 0 {
                            // configure
                            let width = i32::from_ne_bytes(self.buf[8..12].try_into().unwrap());
                            let height = i32::from_ne_bytes(self.buf[12..16].try_into().unwrap());
                            let array_size =
                                u32::from_ne_bytes(self.buf[16..20].try_into().unwrap());
                            // no need for padding here, since uint are of 4 bytes
                            debug_assert!(size == 20 + array_size);
                            let mut activated = false;
                            for &x in self.buf[20..size as usize].array_chunks::<4>() {
                                let value = u32::from_ne_bytes(x);
                                if value == 4 {
                                    // activated
                                    activated = true;
                                }
                            }
                            eprintln!(
                                "xdg_toplevel.configure: width = {width}, height = {height}, activated = {activated}"
                            );
                            if !activated {
                                to_drop = true;
                                // avoid replaying the same activitity and causing MPV to crash.
                                //
                                // let init = *self
                                //     .registry
                                //     .read()
                                //     .await
                                //     .xdg_toplevel
                                //     .get(&object_id)
                                //     .unwrap();
                                // if init {
                                //     let array_size = array_size + 4;
                                //     self.set_size(size + 4);
                                //     self.buf[16..20].copy_from_slice(&array_size.to_ne_bytes());
                                //     self.buf[size as usize..size as usize + 4]
                                //         .copy_from_slice(&4_u32.to_ne_bytes());
                                // }
                            } else {
                                self.registry
                                    .write()
                                    .await
                                    .xdg_toplevel
                                    .insert(object_id, true);
                            }
                        }
                    }
                }
            }

            if !to_drop {
                self.output.push_back(self.buf[0..self.len].to_vec());
            }

            self.ptr = 0;
            self.len = 8;
            self.state = HandlerState::Header;
        }
    }

    fn pop(&mut self) -> Option<Vec<u8>> {
        self.output.pop_front()
    }
}

async fn forward(
    r: &UnixStream,
    w: &UnixStream,
    role: HandlerRole,
    registry: Arc<RwLock<WLObjectRegistry>>,
) -> std::io::Result<()> {
    let mut buf = vec![0u8; 64 * 1024];
    let mut cspace = nix::cmsg_space!([std::os::unix::io::RawFd; 32]);

    let mut handler = WLMessageHandler::new(role, registry);

    loop {
        r.readable().await?;
        let mut iov = [IoSliceMut::new(&mut buf)];
        let mut rights: Vec<std::os::unix::io::RawFd> = Vec::new();
        let n;
        {
            let msg = match r.try_io(Interest::READABLE, || {
                recvmsg::<()>(
                    r.as_raw_fd(),
                    &mut iov,
                    Some(&mut cspace),
                    MsgFlags::MSG_CMSG_CLOEXEC,
                )
                .map_err(|e| std::io::Error::from_raw_os_error(e as i32))
            }) {
                Ok(m) => m,
                Err(e) => {
                    if e.kind() == std::io::ErrorKind::WouldBlock {
                        continue;
                    } else {
                        return Err(std::io::Error::from(e));
                    }
                }
            };

            n = msg.bytes;
            for c in msg.cmsgs().unwrap() {
                if let ControlMessageOwned::ScmRights(fds) = c {
                    rights.extend(fds);
                }
            }

            if n == 0 && rights.is_empty() {
                let _ = nix::sys::socket::shutdown(w.as_raw_fd(), NixShutdown::Write);
                return Ok(());
            }
        }

        debug_assert!(n % 4 == 0);
        for &x in buf[..n].array_chunks::<4>() {
            handler.append_word(x).await;
        }

        let mut response = Vec::<u8>::new();
        while let Some(msg) = handler.pop() {
            response.extend(msg.into_iter());
        }

        w.writable().await?;
        let iov = [IoSlice::new(&response)];
        let mut cmsgs = Vec::new();
        if !rights.is_empty() {
            cmsgs.push(ControlMessage::ScmRights(&rights));
        }

        match w.try_io(Interest::WRITABLE, || {
            sendmsg::<()>(w.as_raw_fd(), &iov, &cmsgs, MsgFlags::MSG_NOSIGNAL, None)
                .map_err(|e| std::io::Error::from_raw_os_error(e as i32))
        }) {
            Ok(_) => {}
            Err(e) => {
                if e.kind() == std::io::ErrorKind::WouldBlock {
                    continue;
                } else {
                    for fd in rights {
                        let _ = nix_close(fd);
                    }
                    return Err(std::io::Error::from(e));
                }
            }
        }

        for fd in rights {
            let _ = nix_close(fd);
        }
    }
}

#[tokio::main]
async fn main() {
    let xdg_runtime_dir = std::env::var("XDG_RUNTIME_DIR").expect("set XDG_RUNTIME_DIR first");
    let wayland_display = std::env::var("WAYLAND_DISPLAY").expect("set WAYLAND_DISPLAY first");
    let sock_path = Path::new(xdg_runtime_dir.as_str()).join("wayland-100");
    let wl_path = Path::new(xdg_runtime_dir.as_str()).join(wayland_display.as_str());
    tokio::fs::remove_file(&sock_path).await.unwrap();
    let server_socket = UnixListener::bind(&sock_path).unwrap();
    loop {
        let (server_stream, addr) = server_socket.accept().await.unwrap();
        eprintln!("incoming connection from {:?}", addr);
        tokio::spawn({
            let wl_path = wl_path.clone();
            async move {
                let client_socket = UnixSocket::new_stream().unwrap();
                let client_stream = client_socket.connect(wl_path).await.unwrap();
                let registry = Arc::new(RwLock::new(WLObjectRegistry::new()));
                let outbound = async {
                    forward(
                        &server_stream,
                        &client_stream,
                        HandlerRole::OutBound,
                        registry.clone(),
                    )
                    .await
                };
                let inbound = async {
                    forward(
                        &client_stream,
                        &server_stream,
                        HandlerRole::InBound,
                        registry.clone(),
                    )
                    .await
                };
                tokio::try_join!(inbound, outbound).unwrap();
            }
        });
    }
}
