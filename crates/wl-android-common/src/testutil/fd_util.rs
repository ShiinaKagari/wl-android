use std::os::fd::AsRawFd;
use std::os::unix::io::OwnedFd;

pub fn memfd_fake_dmabuf(len: usize) -> OwnedFd {
    let name = c"fake_dmabuf";
    let fd = nix::sys::memfd::memfd_create(name, nix::sys::memfd::MemFdCreateFlag::MFD_ALLOW_SEALING)
        .expect("memfd_create for fake dmabuf");
    nix::unistd::ftruncate(&fd, len as nix::libc::off_t).expect("ftruncate fake dmabuf");
    let seals = nix::fcntl::SealFlag::F_SEAL_SHRINK
        | nix::fcntl::SealFlag::F_SEAL_GROW
        | nix::fcntl::SealFlag::F_SEAL_WRITE;
    nix::fcntl::fcntl(
        fd.as_raw_fd(),
        nix::fcntl::FcntlArg::F_ADD_SEALS(seals),
    )
    .expect("seal fake dmabuf");
    fd
}

pub struct FdCountGuard {
    initial: usize,
    label: &'static str,
}

impl FdCountGuard {
    pub fn new(label: &'static str) -> Self {
        let initial = count_open_fds();
        Self { initial, label }
    }

    pub fn check(&self) {
        let current = count_open_fds();
        if current != self.initial {
            panic!(
                "FdCountGuard [{label}]: fd count changed: {current} != {initial} (leak detected)",
                label = self.label,
                current = current,
                initial = self.initial
            );
        }
    }
}

impl Drop for FdCountGuard {
    fn drop(&mut self) {
        self.check();
    }
}

fn count_open_fds() -> usize {
    std::fs::read_dir("/proc/self/fd")
        .map(|entries| entries.count())
        .unwrap_or(0)
}

/// Read valid fd count from /proc/self/fd, returning None if unavailable.
pub fn try_count_open_fds() -> Option<usize> {
    match std::fs::read_dir("/proc/self/fd") {
        Ok(entries) => Some(entries.count()),
        Err(_) => None,
    }
}
