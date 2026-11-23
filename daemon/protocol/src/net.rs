// Small socket helpers both bridges need: waiting for data with a timeout
// (so the input loop can wake up regularly without a read timeout, which
// would cut a package in half and break the stream's decryption), and TCP
// keepalive (so a peer that vanished, e.g. across a sleep, is noticed in
// seconds rather than when a read finally times out).

use std::net::TcpStream;
use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

/// Waits up to `timeout` for `stream` to have data to read (or to be
/// closed: a following read then returns the error). True if it has.
pub fn wait_readable(stream: &impl AsRawFd, timeout: Duration) -> std::io::Result<bool> {
    let mut fds = libc::pollfd { fd: stream.as_raw_fd(), events: libc::POLLIN, revents: 0 };
    let ms = timeout.as_millis().min(i32::MAX as u128) as libc::c_int;
    loop {
        let n = unsafe { libc::poll(&mut fds, 1, ms) };
        if n >= 0 {
            return Ok(n > 0);
        }
        let err = std::io::Error::last_os_error();
        if err.kind() != std::io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
}

/// The input loop's wait for the next package: up to `tick`, true once
/// data (or a close) is there. Since poll never trips the socket's read
/// timeout, this applies it instead: an error once nothing at all has
/// arrived for `idle_limit` since `last_data`, which it moves on whenever
/// data arrives. TCP keepalive usually notices a dead peer much sooner,
/// this covers the case where it couldn't be turned on.
pub fn wait_for_package(stream: &impl AsRawFd, tick: Duration, last_data: &mut Instant, idle_limit: Duration) -> std::io::Result<bool> {
    if wait_readable(stream, tick)? {
        *last_data = Instant::now();
        return Ok(true);
    }
    if last_data.elapsed() >= idle_limit {
        return Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            format!("nothing received for {}s", idle_limit.as_secs()),
        ));
    }
    Ok(false)
}

/// Keepalive for a connection to Windows' message server: a peer that
/// vanished (sleep, cable, a crash) is noticed in about half a minute.
pub fn keep_session_alive(stream: &TcpStream) -> std::io::Result<()> {
    set_keepalive(stream, Duration::from_secs(15), Duration::from_secs(5), 3)
}

/// Turns on TCP keepalive: probes after `idle` without traffic, then every
/// `interval`, giving up (and failing reads and writes) after `count`
/// unanswered probes.
pub fn set_keepalive(stream: &TcpStream, idle: Duration, interval: Duration, count: u32) -> std::io::Result<()> {
    let fd = stream.as_raw_fd();
    let set = |level: libc::c_int, name: libc::c_int, value: libc::c_int| -> std::io::Result<()> {
        let r = unsafe {
            libc::setsockopt(fd, level, name, &value as *const _ as *const libc::c_void, std::mem::size_of::<libc::c_int>() as libc::socklen_t)
        };
        if r == 0 { Ok(()) } else { Err(std::io::Error::last_os_error()) }
    };
    #[cfg(target_os = "macos")]
    const IDLE: libc::c_int = libc::TCP_KEEPALIVE;
    #[cfg(not(target_os = "macos"))]
    const IDLE: libc::c_int = libc::TCP_KEEPIDLE;
    set(libc::SOL_SOCKET, libc::SO_KEEPALIVE, 1)?;
    set(libc::IPPROTO_TCP, IDLE, idle.as_secs().max(1) as libc::c_int)?;
    set(libc::IPPROTO_TCP, libc::TCP_KEEPINTVL, interval.as_secs().max(1) as libc::c_int)?;
    set(libc::IPPROTO_TCP, libc::TCP_KEEPCNT, count as libc::c_int)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::net::TcpListener;
    use std::os::unix::net::UnixStream;

    #[test]
    fn wait_readable_times_out_then_sees_data() {
        let (mut a, b) = UnixStream::pair().unwrap();
        assert!(!wait_readable(&b, Duration::from_millis(20)).unwrap());
        a.write_all(b"x").unwrap();
        assert!(wait_readable(&b, Duration::from_millis(1000)).unwrap());
    }

    #[test]
    fn a_closed_peer_counts_as_readable() {
        let (a, b) = UnixStream::pair().unwrap();
        drop(a);
        assert!(wait_readable(&b, Duration::from_millis(1000)).unwrap());
    }

    #[test]
    fn a_silent_connection_times_out_after_the_idle_limit() {
        let (mut a, b) = UnixStream::pair().unwrap();
        let tick = Duration::from_millis(10);
        let limit = Duration::from_millis(200);
        let mut last = Instant::now();
        assert!(!wait_for_package(&b, tick, &mut last, limit).unwrap(), "quiet, but not for long yet");
        a.write_all(b"x").unwrap();
        assert!(wait_for_package(&b, tick, &mut last, limit).unwrap());
        let mut last = Instant::now() - limit;
        let (_a2, quiet) = UnixStream::pair().unwrap();
        let err = wait_for_package(&quiet, tick, &mut last, limit).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
    }

    #[test]
    fn keepalive_is_switched_on() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let stream = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        set_keepalive(&stream, Duration::from_secs(10), Duration::from_secs(5), 3).unwrap();
        let mut value: libc::c_int = 0;
        let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
        let r = unsafe {
            libc::getsockopt(stream.as_raw_fd(), libc::SOL_SOCKET, libc::SO_KEEPALIVE, &mut value as *mut _ as *mut libc::c_void, &mut len)
        };
        assert_eq!(r, 0);
        assert_ne!(value, 0);
    }
}
