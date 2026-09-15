//! Descriptor-relative managed paths. Components are never followed through symlinks.
use crate::{Error, Result, error::integrity, sha256};
use std::{
    ffi::CString,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

pub struct Dir {
    pub file: File,
    pub path: PathBuf,
}
fn name(s: &str) -> Result<CString> {
    if s.is_empty() || s.contains('/') || s == "." || s == ".." {
        return Err(integrity("Invalid managed path component."));
    }
    CString::new(s).map_err(|_| integrity("Invalid path component."))
}
fn cvt_fd(fd: i32) -> Result<File> {
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: a successful open/openat returns a new, owned descriptor.
    Ok(unsafe { File::from_raw_fd(fd) })
}
impl Dir {
    pub fn open(path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)?;
        Ok(Self {
            file,
            path: fs::canonicalize(path)?,
        })
    }
    pub fn create_root(path: &Path) -> Result<Self> {
        match fs::DirBuilder::new().mode(0o700).create(path) {
            Ok(()) => {
                Self::open(
                    path.parent()
                        .filter(|p| !p.as_os_str().is_empty())
                        .unwrap_or(Path::new(".")),
                )?
                .sync()?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => (),
            Err(e) => return Err(e.into()),
        }
        Self::open(path)
    }
    pub fn child(&self, component: &str, create: bool) -> Result<Self> {
        let n = name(component)?;
        if create {
            // SAFETY: descriptor is live; n is a NUL-terminated component.
            let rc = unsafe { libc::mkdirat(self.file.as_raw_fd(), n.as_ptr(), 0o700) };
            if rc == 0 {
                self.sync()?;
            } else {
                let e = std::io::Error::last_os_error();
                if e.kind() != std::io::ErrorKind::AlreadyExists {
                    return Err(e.into());
                }
            }
        }
        let fd = unsafe {
            libc::openat(
                self.file.as_raw_fd(),
                n.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        Ok(Self {
            file: cvt_fd(fd)?,
            path: self.path.join(component),
        })
    }
    pub fn open_file(&self, component: &str) -> Result<File> {
        let n = name(component)?;
        let file = cvt_fd(unsafe {
            libc::openat(
                self.file.as_raw_fd(),
                n.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
            )
        })?;
        if !file.metadata()?.is_file() {
            return Err(integrity("Expected a regular managed file."));
        }
        Ok(file)
    }
    pub fn new_file(&self, component: &str) -> Result<File> {
        let n = name(component)?;
        cvt_fd(unsafe {
            libc::openat(
                self.file.as_raw_fd(),
                n.as_ptr(),
                libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        })
    }
    pub fn sync(&self) -> Result<()> {
        self.file.sync_all().map_err(Into::into)
    }
    pub fn lock(&self, exclusive: bool) -> Result<()> {
        let start = Instant::now();
        loop {
            let rc = unsafe {
                libc::flock(
                    self.file.as_raw_fd(),
                    (if exclusive {
                        libc::LOCK_EX
                    } else {
                        libc::LOCK_SH
                    }) | libc::LOCK_NB,
                )
            };
            if rc == 0 {
                return Ok(());
            }
            let e = std::io::Error::last_os_error();
            if e.kind() != std::io::ErrorKind::WouldBlock {
                return Err(e.into());
            }
            if start.elapsed() >= Duration::from_secs(5) {
                return Err(Error::new("E_BUSY", "Store initialization lock is busy."));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    pub fn check_sqlite_paths(&self) -> Result<()> {
        for n in [
            "index.sqlite3",
            "index.sqlite3-wal",
            "index.sqlite3-shm",
            "index.sqlite3-journal",
        ] {
            match fs::symlink_metadata(self.path.join(n)) {
                Ok(m) if !m.is_file() || m.nlink() != 1 => {
                    return Err(integrity("Unsafe SQLite file path."));
                }
                Ok(_) => (),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }
}

pub struct Temp<'a> {
    pub file: File,
    dir: &'a Dir,
    name: String,
}
impl<'a> Temp<'a> {
    pub fn new(dir: &'a Dir) -> Result<Self> {
        let name = format!(".vstore-{}.tmp", uuid::Uuid::new_v4());
        Ok(Self {
            file: dir.new_file(&name)?,
            dir,
            name,
        })
    }
    pub fn publish(&self, to: &Dir, component: &str) -> Result<bool> {
        self.file.sync_all()?;
        let (old, new) = (name(&self.name)?, name(component)?);
        let rc = unsafe {
            libc::linkat(
                self.dir.file.as_raw_fd(),
                old.as_ptr(),
                to.file.as_raw_fd(),
                new.as_ptr(),
                0,
            )
        };
        if rc != 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::AlreadyExists {
                return Ok(false);
            }
            return Err(e.into());
        }
        to.sync()?;
        Ok(true)
    }
}
impl Drop for Temp<'_> {
    fn drop(&mut self) {
        if let Ok(n) = name(&self.name) {
            unsafe {
                libc::unlinkat(self.dir.file.as_raw_fd(), n.as_ptr(), 0);
            }
        }
    }
}

pub fn read_bounded(file: &mut File, max: usize) -> Result<Vec<u8>> {
    if file.metadata()?.len() > max as u64 {
        return Err(Error::new(
            "E_LIMIT_EXCEEDED",
            "File exceeds configured size limit.",
        ));
    }
    let mut bytes = Vec::new();
    file.take((max as u64).saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() > max {
        return Err(Error::new(
            "E_LIMIT_EXCEEDED",
            "File exceeds configured size limit.",
        ));
    }
    Ok(bytes)
}
pub fn snapshot(path: &Path, tmp: &Dir, max: usize) -> Result<Vec<u8>> {
    let mut input = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)?;
    let before = input.metadata()?;
    if !before.is_file() {
        return Err(crate::error::invalid("Input must be a regular local file."));
    }
    let bytes = read_bounded(&mut input, max)?;
    let mut frozen = Temp::new(tmp)?;
    frozen.file.write_all(&bytes)?;
    frozen.file.sync_all()?;
    #[cfg(feature = "fault-injection")]
    if std::env::var("VSTORE_TEST_MUTATE_SOURCE").as_deref() == Ok("1") {
        OpenOptions::new()
            .append(true)
            .open(path)?
            .write_all(b"changed")?;
    }
    crate::fault("source_snapshot")?;
    let after = input.metadata()?;
    let path_after = fs::metadata(path)
        .map_err(|_| Error::new("E_SOURCE_CHANGED", "Input changed during capture."))?;
    let signature = |m: &fs::Metadata| {
        (
            m.dev(),
            m.ino(),
            m.len(),
            m.mtime(),
            m.mtime_nsec(),
            m.ctime(),
            m.ctime_nsec(),
        )
    };
    if signature(&before) != signature(&after)
        || signature(&before) != signature(&path_after)
        || before.len() != bytes.len() as u64
    {
        return Err(Error::new(
            "E_SOURCE_CHANGED",
            "Input changed during capture.",
        ));
    }
    Ok(bytes)
}

pub fn check_hash(file: &mut File, length: u64, hash: &str, limit: usize) -> Result<Vec<u8>> {
    let data = read_bounded(file, limit)?;
    if data.len() as u64 != length || sha256(&data) != hash {
        return Err(integrity("Blob size or SHA-256 mismatch."));
    }
    Ok(data)
}

pub fn external_destination(path: &Path) -> Result<(Dir, String)> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let dir = Dir::open(parent)?;
    let component = path
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| crate::error::invalid("Output filename must be UTF-8."))?
        .to_owned();
    name(&component)?;
    Ok((dir, component))
}

pub fn path_json(path: &Path) -> Result<&str> {
    path.to_str()
        .ok_or_else(|| crate::error::invalid("JSON output paths must be UTF-8."))
}

pub fn ensure_absent(dir: &Dir, component: &str) -> Result<()> {
    let n = name(component)?;
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    let rc = unsafe {
        libc::fstatat(
            dir.file.as_raw_fd(),
            n.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if rc == 0 {
        return Err(Error::new("E_OUTPUT_EXISTS", "Output already exists."));
    }
    let e = std::io::Error::last_os_error();
    if e.kind() != std::io::ErrorKind::NotFound {
        return Err(e.into());
    }
    Ok(())
}
