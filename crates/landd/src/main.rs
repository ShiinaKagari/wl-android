//! wl-android 守护进程 (landd)
//!
//! Magisk 模块的一部分。开机自启，常驻内存。
//! 监听 `/dev/socket/land.sock`，双向转发：
//! - 容器侧 DMA-BUF fd → App 侧
//! - App 侧触摸事件 → 容器侧

mod forwarder;
mod socket_server;

use std::io;
use std::os::unix::fs::PermissionsExt;

use land_common::types::default_socket_path;

fn main() -> io::Result<()> {
    let socket_path = default_socket_path();

    eprintln!("[landd] starting, socket={}", socket_path.display());

    // 确保父目录存在 (tmpfs /dev/socket)
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent)?;
        // tmpfs 上权限 0755
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o755))?;
    }

    // 清理残留 socket 文件（非强制）
    let _ = std::fs::remove_file(&socket_path);

    let mut server = socket_server::SocketServer::bind(&socket_path)?;

    // socket 权限 0666
    std::fs::set_permissions(&socket_path, PermissionsExt::from_mode(0o666))?;

    eprintln!("[landd] listening on {}", socket_path.display());

    server.run()?;

    Ok(())
}
