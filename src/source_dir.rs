//! Read-only, descriptor-relative directory metadata and bounded regular-file reads.
use serde::Serialize;
use std::{
    io,
    path::{Path, PathBuf},
};

pub const SCAN_LIMIT: usize = 10_000;
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    pub name: String,
    pub path: String,
    pub kind: &'static str,
    pub indexed_path: Option<String>,
    pub method_count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unindexed_reason: Option<String>,
}
pub fn valid_path(path: &str) -> bool {
    path.len() <= 8192
        && !path.contains(['\0', '\\', ':'])
        && (path.is_empty()
            || path
                .split('/')
                .all(|p| !p.is_empty() && p != "." && p != ".."))
}

/// The directory handle pins the configured root; client paths never select a root.
pub struct SourceDir {
    pub root: PathBuf,
    #[cfg(unix)]
    directory: std::fs::File,
}
impl SourceDir {
    pub fn open(root: &Path) -> io::Result<Self> {
        let root = root.canonicalize()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            let directory = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(&root)?;
            Ok(Self { root, directory })
        }
        #[cfg(not(unix))]
        {
            let _ = root;
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "secure directory browsing requires Unix",
            ))
        }
    }
    /// Read one regular UTF-8 file beneath the pinned root, without following symlinks.
    /// The bound applies before allocation and during reading, including concurrent growth.
    pub fn read_file(&self, path: &str) -> io::Result<String> {
        const MAX_BYTES: u64 = 2 * 1024 * 1024;
        if path.is_empty() || !valid_path(path) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid file path",
            ));
        }
        #[cfg(unix)]
        {
            use std::{
                ffi::CString,
                io::Read,
                os::fd::{AsRawFd, FromRawFd, OwnedFd},
            };
            let components: Vec<_> = path.split('/').collect();
            let mut directory = None::<OwnedFd>;
            for component in &components[..components.len() - 1] {
                let name = CString::new(*component).unwrap();
                let parent = directory
                    .as_ref()
                    .map_or(self.directory.as_raw_fd(), AsRawFd::as_raw_fd);
                let fd = unsafe {
                    libc::openat(
                        parent,
                        name.as_ptr(),
                        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                    )
                };
                if fd < 0 {
                    return Err(io::Error::last_os_error());
                }
                directory = Some(unsafe { OwnedFd::from_raw_fd(fd) });
            }
            let name = CString::new(*components.last().unwrap()).unwrap();
            let parent = directory
                .as_ref()
                .map_or(self.directory.as_raw_fd(), AsRawFd::as_raw_fd);
            // NONBLOCK prevents a malicious FIFO from hanging before the regular-file check.
            let fd = unsafe {
                libc::openat(
                    parent,
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
                )
            };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            let file = unsafe { std::fs::File::from_raw_fd(fd) };
            let metadata = file.metadata()?;
            if !metadata.is_file() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "not a regular file",
                ));
            }
            if metadata.len() > MAX_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::FileTooLarge,
                    "file exceeds limit",
                ));
            }
            let mut bytes = Vec::new();
            file.take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
            if bytes.len() as u64 > MAX_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::FileTooLarge,
                    "file exceeds limit",
                ));
            }
            String::from_utf8(bytes)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "file is not UTF-8"))
        }
        #[cfg(not(unix))]
        {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "secure file reads require Unix",
            ))
        }
    }
    pub fn list(
        &self,
        path: &str,
        offset: usize,
        limit: usize,
    ) -> io::Result<(Vec<Entry>, Option<usize>, bool)> {
        if !valid_path(path) || !(1..=200).contains(&limit) || offset > SCAN_LIMIT {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid directory path or pagination",
            ));
        }
        #[cfg(unix)]
        {
            self.list_unix(path, offset, limit)
        }
        #[cfg(not(unix))]
        {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "secure directory browsing requires Unix",
            ))
        }
    }
    #[cfg(unix)]
    fn list_unix(
        &self,
        path: &str,
        offset: usize,
        limit: usize,
    ) -> io::Result<(Vec<Entry>, Option<usize>, bool)> {
        use std::{
            ffi::{CStr, CString},
            os::fd::{AsRawFd, FromRawFd, OwnedFd},
        };
        // Opening "." gives each request its own directory offset (dup would share it).
        let mut directory = None::<OwnedFd>;
        for component in std::iter::once(".").chain(path.split('/').filter(|s| !s.is_empty())) {
            let name = CString::new(component).unwrap();
            let parent = directory
                .as_ref()
                .map_or(self.directory.as_raw_fd(), AsRawFd::as_raw_fd);
            let fd = unsafe {
                libc::openat(
                    parent,
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            directory = Some(unsafe { OwnedFd::from_raw_fd(fd) });
        }
        use std::os::fd::IntoRawFd;
        let fd = directory.unwrap().into_raw_fd();
        let stream = unsafe { libc::fdopendir(fd) };
        if stream.is_null() {
            let error = io::Error::last_os_error();
            unsafe {
                libc::close(fd);
            }
            return Err(error);
        }
        struct Stream(*mut libc::DIR);
        impl Drop for Stream {
            fn drop(&mut self) {
                unsafe {
                    libc::closedir(self.0);
                }
            }
        }
        let stream = Stream(stream);
        let mut entries = Vec::new();
        let mut scanned = 0;
        let mut truncated = false;
        loop {
            // readdir uses errno to distinguish EOF from failure.
            #[cfg(target_os = "macos")]
            unsafe {
                *libc::__error() = 0;
            }
            #[cfg(target_os = "linux")]
            unsafe {
                *libc::__errno_location() = 0;
            }
            let raw = unsafe { libc::readdir(stream.0) };
            if raw.is_null() {
                let error = io::Error::last_os_error();
                if error.raw_os_error().unwrap_or(0) != 0 {
                    return Err(error);
                }
                break;
            }
            let raw_name = unsafe { CStr::from_ptr((*raw).d_name.as_ptr()) };
            if raw_name.to_bytes() == b"." || raw_name.to_bytes() == b".." {
                continue;
            }
            if scanned == SCAN_LIMIT {
                truncated = true;
                break;
            }
            scanned += 1;
            // Lossy names can alias another entry. Report truncation rather than invent paths.
            let Ok(name) = raw_name.to_str() else {
                truncated = true;
                continue;
            };
            if !valid_path(name) {
                truncated = true;
                continue;
            }
            let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
            if unsafe {
                libc::fstatat(
                    fd,
                    raw_name.as_ptr(),
                    metadata.as_mut_ptr(),
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            } != 0
            {
                return Err(io::Error::last_os_error());
            }
            let mode = unsafe { metadata.assume_init() }.st_mode & libc::S_IFMT;
            let kind = match mode {
                libc::S_IFDIR => "directory",
                libc::S_IFREG => "file",
                libc::S_IFLNK => "symlink",
                _ => "other",
            };
            entries.push(Entry {
                name: name.into(),
                path: if path.is_empty() {
                    name.into()
                } else {
                    format!("{path}/{name}")
                },
                kind,
                indexed_path: None,
                method_count: None,
                unindexed_reason: None,
            });
        }
        entries.sort_by(|a, b| {
            (a.kind != "directory", &a.name).cmp(&(b.kind != "directory", &b.name))
        });
        let next = (offset.saturating_add(limit) < entries.len()).then_some(offset + limit);
        Ok((
            entries.into_iter().skip(offset).take(limit).collect(),
            next,
            truncated,
        ))
    }
}
