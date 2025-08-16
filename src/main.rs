use std::path::Path;

use tokio::io::Interest;
use tokio::net::{UnixListener, UnixSocket};

use nix::sys::socket::Shutdown as NixShutdown;
use nix::sys::socket::{ControlMessage, ControlMessageOwned, MsgFlags, recvmsg, sendmsg};
use nix::unistd::close as nix_close;
use std::io::{IoSlice, IoSliceMut};
use std::os::unix::io::AsRawFd;
use tokio::net::UnixStream;

async fn forward(r: &UnixStream, w: &UnixStream) -> std::io::Result<()> {
    let mut buf = vec![0u8; 64 * 1024];
    let mut cspace = nix::cmsg_space!([std::os::unix::io::RawFd; 32]);

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

        w.writable().await?;
        let iov = [IoSlice::new(&buf[..n])];
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
                let outbound = async { forward(&server_stream, &client_stream).await };
                let inbound = async { forward(&client_stream, &server_stream).await };
                tokio::try_join!(inbound, outbound).unwrap();
            }
        });
    }
}
