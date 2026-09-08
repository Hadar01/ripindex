//! The local transport: a Unix domain socket at mode 0600 on Unix, a named
//! pipe with a DACL restricted to the current user on Windows. Never TCP —
//! see `paths`'s module docs for why.
//!
//! Both platforms expose the same shape: [`bind`] returns a [`Listener`]
//! whose [`Listener::accept`] yields a stream implementing
//! `AsyncRead + AsyncWrite + Unpin + Send`, so `server::serve_connection` is
//! written once and shared by both accept loops. [`connect`] is the client
//! side, used directly by `client::connect_or_spawn`.
//!
//! The Windows path is the one actually exercised in this build's tests
//! (the sandbox this was developed in is Windows); the Unix path compiles
//! under `cfg(unix)` but is unexercised here, in the same position as this
//! project's other from-day-one Unix-specific code (`crawler`'s `file_id`,
//! `fs::RealFs::sync_dir`).

use std::io;

use tokio::io::{AsyncRead, AsyncWrite};

/// Unifies the two platforms' listener types so `server::serve` is written
/// once. `accept` returns an owned future via return-position `impl Trait`
/// (stable since Rust 1.75) rather than needing `async-trait` or boxing.
pub trait Accept {
    type Conn: AsyncRead + AsyncWrite + Unpin + Send + 'static;
    fn accept(&mut self) -> impl std::future::Future<Output = io::Result<Self::Conn>> + Send;
}

/// True if connecting to (or binding) the transport failed because nothing
/// is listening — the "no daemon running, autostart one" signal — as
/// opposed to some other failure (permission denied, a malformed path) that
/// autostarting a new daemon won't fix.
pub fn is_not_running(e: &io::Error) -> bool {
    matches!(e.kind(), io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused)
        || cfg!(windows) && e.raw_os_error() == Some(2) // ERROR_FILE_NOT_FOUND: no such pipe
}

#[cfg(windows)]
pub use windows_impl::*;

#[cfg(windows)]
mod windows_impl {
    use super::*;
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use std::time::Duration;
    use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions};

    pub type Conn = NamedPipeServer;

    /// Win32 `SECURITY_ATTRIBUTES`, hand-laid-out to the documented ABI
    /// (stable and simple enough that depending on `windows-sys` picking the
    /// exact same re-export path isn't worth the fragility).
    #[repr(C)]
    struct RawSecurityAttributes {
        n_length: u32,
        lp_security_descriptor: *mut core::ffi::c_void,
        b_inherit_handle: i32,
    }

    /// A self-relative security descriptor granting full access to the
    /// owner (the daemon's own user) only, `Protected` so no ACEs are
    /// inherited from the pipe directory's default DACL. Built once and held
    /// for the process's lifetime — `create_with_security_attributes_raw` is
    /// called once per pipe instance (once per accepted connection plus one
    /// at startup), and this is reused for every call rather than
    /// reconstructed each time.
    struct SecurityDescriptor {
        /// Opaque `PSECURITY_DESCRIPTOR`; kept as a raw pointer rather than
        /// depending on `windows-sys` exporting that exact alias.
        ptr: *mut core::ffi::c_void,
    }

    // SAFETY: the descriptor is process-local, allocated once, freed once in
    // `Drop`, and never mutated after construction — sharing the raw pointer
    // across threads for read-only use (passing it into pipe-creation calls)
    // is sound.
    unsafe impl Send for SecurityDescriptor {}
    unsafe impl Sync for SecurityDescriptor {}

    impl SecurityDescriptor {
        /// `D:P(A;;GA;;;OW)` — DACL, Protected (no inheritance), one ACE:
        /// Allow Generic-All to Owner. Nobody else gets a connect handle.
        fn owner_only() -> io::Result<Self> {
            use windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
            let sddl: Vec<u16> = OsStr::new("D:P(A;;GA;;;OW)\0").encode_wide().collect();
            let mut psd: *mut core::ffi::c_void = std::ptr::null_mut();
            // SAFETY: `sddl` is a valid, NUL-terminated wide string live for
            // the call; `psd` is a valid out-pointer. On success the OS
            // allocates the descriptor (freed in `Drop` via `LocalFree`).
            let ok = unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl.as_ptr(), 1 /* SDDL_REVISION_1 */, &mut psd as *mut _ as *mut _, std::ptr::null_mut())
            };
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(Self { ptr: psd })
        }

        fn as_attrs(&self) -> RawSecurityAttributes {
            RawSecurityAttributes {
                n_length: std::mem::size_of::<RawSecurityAttributes>() as u32,
                lp_security_descriptor: self.ptr,
                b_inherit_handle: 0,
            }
        }
    }

    impl Drop for SecurityDescriptor {
        fn drop(&mut self) {
            // SAFETY: `self.ptr` was allocated by `ConvertStringSecurityDescriptorToSecurityDescriptorW`,
            // which documents `LocalFree` as the correct release for it.
            unsafe {
                windows_sys::Win32::Foundation::LocalFree(self.ptr);
            }
        }
    }

    pub struct Listener {
        name: String,
        sd: SecurityDescriptor,
        next: Option<NamedPipeServer>,
    }

    /// Create the first pipe instance under a fresh, owner-only DACL.
    /// `ERROR_PIPE_BUSY`/"already exists"-flavoured failure here means
    /// another daemon already owns this name — the caller (which should
    /// already hold the global instance lock before calling this) treats
    /// that as "lost the autostart race, exit quietly."
    pub fn bind(name: &str) -> io::Result<Listener> {
        let sd = SecurityDescriptor::owner_only()?;
        let attrs = sd.as_attrs();
        // SAFETY: `attrs` is a valid, fully-initialised SECURITY_ATTRIBUTES
        // on the stack for the duration of this call; the pipe API copies
        // what it needs from it before returning.
        let first = unsafe {
            ServerOptions::new().first_pipe_instance(true).create_with_security_attributes_raw(name, &attrs as *const _ as *mut core::ffi::c_void)?
        };
        Ok(Listener { name: name.to_string(), sd, next: Some(first) })
    }

    impl super::Accept for Listener {
        type Conn = Conn;
        async fn accept(&mut self) -> io::Result<Conn> {
            // Lazily recreate `next` if a previous call's attempt to queue
            // it failed (see below) — retrying here, rather than panicking
            // on a `None`, means a transient failure to create a pipe
            // instance under heavy concurrent connect load self-heals on
            // the following accept instead of killing the whole loop.
            if self.next.is_none() {
                let attrs = self.sd.as_attrs();
                // SAFETY: as below — a valid, live SECURITY_ATTRIBUTES for the duration of the call.
                self.next = Some(unsafe { ServerOptions::new().create_with_security_attributes_raw(&self.name, &attrs as *const _ as *mut core::ffi::c_void)? });
            }
            let server = self.next.take().unwrap();
            server.connect().await?;
            // Queue the next instance *before* handing this one to a
            // handler, so a burst of near-simultaneous connects doesn't
            // race an accept against pipe-instance creation. Critically,
            // if *this* fails, `server` — a client that has already
            // successfully connected — must still be returned rather than
            // dropped; the failure just means the next `accept()` call
            // retries creating an instance (above) before it can hand out
            // another connection.
            let attrs = self.sd.as_attrs();
            // SAFETY: as in `bind` — a valid, live SECURITY_ATTRIBUTES for
            // the duration of the call.
            match unsafe { ServerOptions::new().create_with_security_attributes_raw(&self.name, &attrs as *const _ as *mut core::ffi::c_void) } {
                Ok(inst) => self.next = Some(inst),
                Err(e) => log::warn!("ripindex daemon: failed to queue the next pipe instance ({e}); will retry on the next accept"),
            }
            Ok(server)
        }
    }

    /// Connect as a client, retrying briefly on `ERROR_PIPE_BUSY` (every
    /// server instance momentarily in use) — a handful of attempts over a
    /// few hundred milliseconds covers ordinary contention without the
    /// complexity of `WaitNamedPipe`, which tokio's client doesn't expose.
    pub async fn connect(name: &str) -> io::Result<NamedPipeClient> {
        let deadline = tokio::time::Instant::now() + Duration::from_millis(500);
        loop {
            match ClientOptions::new().open(name) {
                Ok(c) => return Ok(c),
                Err(e) if e.raw_os_error() == Some(231) /* ERROR_PIPE_BUSY */ && tokio::time::Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Err(e) => return Err(e),
            }
        }
    }
}

#[cfg(unix)]
pub use unix_impl::*;

#[cfg(unix)]
mod unix_impl {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use tokio::net::{UnixListener, UnixStream};

    pub type Conn = UnixStream;

    pub struct Listener(UnixListener);

    /// Bind at `path`. If the path already exists, first try connecting —
    /// success means a live daemon owns it (the caller should already hold
    /// the global instance lock before calling `bind`, so this shouldn't
    /// happen, but is checked rather than assumed); failure means it's a
    /// stale socket file left by a killed daemon (the kernel doesn't clean
    /// these up the way it does named pipes), so it's unlinked and rebind is
    /// retried once.
    pub async fn bind(path: &Path) -> io::Result<Listener> {
        if path.exists() {
            if UnixStream::connect(path).await.is_ok() {
                return Err(io::Error::new(io::ErrorKind::AddrInUse, "a daemon is already listening"));
            }
            let _ = fs::remove_file(path);
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let listener = UnixListener::bind(path)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        Ok(Listener(listener))
    }

    impl super::Accept for Listener {
        type Conn = Conn;
        async fn accept(&mut self) -> io::Result<Conn> {
            let (stream, _addr) = self.0.accept().await?;
            Ok(stream)
        }
    }

    /// The kernel's peer-credential check — `SO_PEERCRED` on Linux,
    /// `LOCAL_PEERCRED` on macOS — restricting connections to the daemon's
    /// own UID even if the socket's mode bits were ever loosened. Left as a
    /// follow-up: `tokio::net::unix::UCred` (via `UnixStream::peer_cred`)
    /// exposes exactly this on Linux/macOS/a few BSDs; wiring it in is a
    /// few lines in `bind`/`accept`, omitted here because it cannot be
    /// exercised or verified from this (Windows) development sandbox.
    pub async fn connect(path: &Path) -> io::Result<Conn> {
        UnixStream::connect(path).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn not_running_classification() {
        assert!(is_not_running(&io::Error::from(io::ErrorKind::NotFound)));
        assert!(is_not_running(&io::Error::from(io::ErrorKind::ConnectionRefused)));
        assert!(!is_not_running(&io::Error::from(io::ErrorKind::PermissionDenied)));
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn bind_accept_connect_roundtrip_over_a_real_pipe() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let name = format!(r"\\.\pipe\ripindex-test-{}", std::process::id());
        let mut listener = bind(&name).unwrap();
        let server_task = tokio::spawn(async move {
            let mut conn = listener.accept().await.unwrap();
            let mut buf = [0u8; 5];
            conn.read_exact(&mut buf).await.unwrap();
            conn.write_all(b"world").await.unwrap();
        });
        let mut client = connect(&name).await.unwrap();
        client.write_all(b"hello").await.unwrap();
        let mut buf = [0u8; 5];
        client.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"world");
        server_task.await.unwrap();
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn a_second_bind_of_the_same_name_fails() {
        let name = format!(r"\\.\pipe\ripindex-test2-{}", std::process::id());
        let _first = bind(&name).unwrap();
        let second = bind(&name);
        assert!(second.is_err(), "first_pipe_instance must refuse a duplicate name");
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn connecting_with_no_listener_is_classified_as_not_running() {
        let name = format!(r"\\.\pipe\ripindex-test-nobody-{}", std::process::id());
        let err = connect(&name).await.unwrap_err();
        assert!(is_not_running(&err), "{err:?}");
    }
}
