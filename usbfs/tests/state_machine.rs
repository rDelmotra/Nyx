use std::collections::VecDeque;
use std::os::raw::{c_int, c_void};
use std::os::unix::io::RawFd;
use std::sync::{Arc, Mutex};

use luks_core::error::LuksError;
use luks_core::usb::BulkTransport;
use luks_usbfs::*;

/// Make the next `errno()` the transport reads return `e`.
fn set_errno(e: i32) {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    unsafe {
        *libc::__errno_location() = e;
    }
    #[cfg(target_os = "macos")]
    unsafe {
        *libc::__error() = e;
    }
}

#[derive(Default)]
struct MockState {
    submitted_urbs: Vec<(u8, usize)>, // (endpoint, buffer_length)
    discarded_count: usize,
    poll_events: VecDeque<std::result::Result<libc::c_short, i32>>,
    reap_events: VecDeque<(c_int, usize)>, // (rc, usercontext_token)
    default_poll_err: Option<i32>,
    /// Make SUBMITURB fail with this errno (rc -1 with errno set).
    submit_errno: Option<i32>,
}

struct MockRawUsbFs {
    state: Mutex<MockState>,
}

impl MockRawUsbFs {
    fn new() -> Self {
        Self {
            state: Mutex::new(MockState::default()),
        }
    }

    fn push_poll(&self, res: std::result::Result<libc::c_short, i32>) {
        self.state.lock().unwrap().poll_events.push_back(res);
    }

    fn push_reap(&self, rc: c_int, token: usize) {
        self.state.lock().unwrap().reap_events.push_back((rc, token));
    }

    fn set_default_poll_err(&self, err: i32) {
        self.state.lock().unwrap().default_poll_err = Some(err);
    }

    fn set_submit_errno(&self, e: Option<i32>) {
        self.state.lock().unwrap().submit_errno = e;
    }

    fn submitted_count(&self) -> usize {
        self.state.lock().unwrap().submitted_urbs.len()
    }

    fn discarded_count(&self) -> usize {
        self.state.lock().unwrap().discarded_count
    }
}

impl RawUsbFs for MockRawUsbFs {
    fn ioctl(&self, _fd: RawFd, req: u64, arg: *mut c_void) -> c_int {
        let req_submit = submit_urb_code() as u64;
        let req_discard = discard_urb_code() as u64;
        let req_reap = reap_urb_code() as u64;

        if req == req_submit {
            if let Some(e) = self.state.lock().unwrap().submit_errno {
                set_errno(e);
                return -1;
            }
            let urb = unsafe { &mut *(arg as *mut UsbdevfsUrb) };
            self.state.lock().unwrap().submitted_urbs.push((urb.endpoint, urb.buffer_length as usize));
            0
        } else if req == req_discard {
            self.state.lock().unwrap().discarded_count += 1;
            0
        } else if req == req_reap {
            let mut state = self.state.lock().unwrap();
            if let Some((rc, token)) = state.reap_events.pop_front() {
                if rc >= 0 {
                    let reaped_ptr = arg as *mut *mut c_void;
                    unsafe {
                        *reaped_ptr = token as *mut c_void;
                    }
                }
                rc
            } else {
                -1
            }
        } else {
            0
        }
    }

    fn poll(&self, _fd: RawFd, _events: libc::c_short, _timeout_ms: i32) -> std::result::Result<libc::c_short, i32> {
        let mut state = self.state.lock().unwrap();
        if let Some(res) = state.poll_events.pop_front() {
            return res;
        }
        if let Some(err) = state.default_poll_err {
            return Err(err);
        }
        Ok(libc::POLLOUT)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn normal_transfer_submits_and_reaps_cleanly() {
    let mock = Arc::new(MockRawUsbFs::new());
    // 64 KiB buffer -> 1 chunk -> slot 0 (token = 1)
    mock.push_poll(Ok(libc::POLLOUT));
    mock.push_reap(0, 1);

    let transport = unsafe { UsbFsTransport::from_raw_fd(10, 0x81, 0x02, 0) }
        .with_raw_usbfs(mock.clone())
        .with_timeout_ms(100);

    let data = vec![0x42u8; 64 * 1024];
    let _written = transport.write(&data).expect("write");
    assert_eq!(mock.submitted_count(), 1);
    assert_eq!(mock.discarded_count(), 0);
    assert_eq!(transport.state(), TransportState::Healthy);
}

#[test]
fn timeout_on_reap_drains_discarded_urbs_and_recovers_to_healthy() {
    let mock = Arc::new(MockRawUsbFs::new());

    // 1st poll times out
    mock.push_poll(Err(libc::ETIMEDOUT));
    // 2nd poll (during drain) succeeds and reaps slot 0 with token = 1
    mock.push_poll(Ok(libc::POLLOUT));
    mock.push_reap(0, 1);

    let transport = unsafe { UsbFsTransport::from_raw_fd(10, 0x81, 0x02, 0) }
        .with_raw_usbfs(mock.clone())
        .with_timeout_ms(50);

    let data = vec![0x42u8; 64 * 1024];
    let err = transport.write(&data).unwrap_err();
    assert!(
        matches!(err, LuksError::UsbErrno { errno, .. } if errno == libc::ETIMEDOUT),
        "expected a timed out error, got: {err}"
    );

    // Assert DISCARDURB was issued
    assert_eq!(mock.discarded_count(), 1);
    // Assert all URBs were drained, so state recovered to Healthy!
    assert_eq!(transport.state(), TransportState::Healthy);

    // A subsequent transfer can proceed normally
    mock.push_poll(Ok(libc::POLLOUT));
    mock.push_reap(0, 1);
    let _ = transport.write(&data);
    assert_eq!(transport.state(), TransportState::Healthy);
}

#[test]
fn timeout_with_unreaped_urbs_latches_to_dead_and_fast_refuses() {
    let mock = Arc::new(MockRawUsbFs::new());

    // Poll always times out (kernel never answers)
    mock.set_default_poll_err(libc::ETIMEDOUT);

    let transport = unsafe { UsbFsTransport::from_raw_fd(10, 0x81, 0x02, 0) }
        .with_raw_usbfs(mock.clone())
        .with_timeout_ms(20); // short timeout so test runs fast

    let data = vec![0x42u8; 64 * 1024];
    let err = transport.write(&data).unwrap_err();
    assert!(
        matches!(err, LuksError::UsbErrno { errno, .. } if errno == libc::ETIMEDOUT || errno == 108),
        "expected timeout/dead error, got: {err}"
    );

    // Because drain deadline expired without reaping, state MUST be Dead
    assert_eq!(transport.state(), TransportState::Dead);

    // Subsequent operations MUST fast-refuse immediately without issuing ioctls
    let prev_submitted = mock.submitted_count();
    let second_err = transport.write(&data).unwrap_err();
    assert!(
        second_err.is_disconnected(),
        "a Dead transport must classify as disconnected, got: {second_err:?}"
    );
    assert!(second_err.to_string().contains("dead"), "{second_err}");
    assert_eq!(mock.submitted_count(), prev_submitted, "no new submissions when Dead");

    let mut read_buf = [0u8; 64];
    let read_err = transport.read(&mut read_buf).unwrap_err();
    assert!(read_err.is_disconnected(), "{read_err:?}");

    let clear_err = transport.clear_halt(true).unwrap_err();
    assert!(clear_err.is_disconnected(), "{clear_err:?}");

    let reset_err = transport.reset().unwrap_err();
    assert!(reset_err.is_disconnected(), "{reset_err:?}");
}

#[test]
fn delayed_stale_completion_from_earlier_generation_does_not_corrupt_or_panic() {
    let mock = Arc::new(MockRawUsbFs::new());

    // Transfer 1: 1 chunk -> slot 0 (token = 1). Times out, drained with token = 1.
    mock.push_poll(Err(libc::ETIMEDOUT));
    mock.push_poll(Ok(libc::POLLOUT));
    mock.push_reap(0, 1); // reaps slot 0 (generation 1)

    let transport = unsafe { UsbFsTransport::from_raw_fd(10, 0x81, 0x02, 0) }
        .with_raw_usbfs(mock.clone())
        .with_timeout_ms(50);

    let data = vec![0x42u8; 64 * 1024];
    let _ = transport.write(&data);
    assert_eq!(transport.state(), TransportState::Healthy);

    // Transfer 2: Starts with generation 2, uses slot 0 (token = 1).
    mock.push_poll(Ok(libc::POLLOUT));
    mock.push_reap(0, 1);

    let _ = transport.write(&data);
    assert_eq!(transport.state(), TransportState::Healthy);
}

#[test]
fn invalid_or_corrupt_usercontext_from_kernel_is_safely_ignored() {
    let mock = Arc::new(MockRawUsbFs::new());

    // Kernel returns garbage usercontext pointers (0xDEADBEEF, 0x0) before the real token (1)
    mock.push_poll(Ok(libc::POLLOUT));
    mock.push_reap(0, 0xDEADBEEF);
    mock.push_poll(Ok(libc::POLLOUT));
    mock.push_reap(0, 0);
    mock.push_poll(Ok(libc::POLLOUT));
    mock.push_reap(0, 1); // valid token

    let transport = unsafe { UsbFsTransport::from_raw_fd(10, 0x81, 0x02, 0) }
        .with_raw_usbfs(mock.clone())
        .with_timeout_ms(50);

    let data = vec![0x42u8; 64 * 1024];
    let _ = transport.write(&data);
    assert_eq!(transport.state(), TransportState::Healthy);
}

// ---------------------------------------------------------------------------
// F1.4 (T5 / P-5): what may shrink max_transfer, and what gives it back
// ---------------------------------------------------------------------------

/// One transfer that completes cleanly: slot 0's URB reaps with token 1.
fn ok_round(mock: &MockRawUsbFs) {
    mock.push_poll(Ok(libc::POLLOUT));
    mock.push_reap(0, 1);
}

/// One transfer that times out on the poll, then drains cleanly.
fn timeout_round(mock: &MockRawUsbFs) {
    mock.push_poll(Err(libc::ETIMEDOUT));
    mock.push_poll(Ok(libc::POLLOUT));
    mock.push_reap(0, 1);
}

fn transport(mock: &Arc<MockRawUsbFs>) -> UsbFsTransport {
    unsafe { UsbFsTransport::from_raw_fd(10, 0x81, 0x02, 0) }
        .with_raw_usbfs(mock.clone())
        .with_timeout_ms(50)
}

#[test]
fn a_cbw_or_csw_timeout_does_not_shrink_max_transfer() {
    let mock = Arc::new(MockRawUsbFs::new());
    let t = transport(&mock);
    let before = t.max_transfer();
    assert_eq!(before, UsbFsTransport::DEFAULT_MAX_TRANSFER);

    // A 31-byte CBW out, then a 13-byte CSW in: both time out. Neither has
    // anything to do with transfer size.
    timeout_round(&mock);
    let err = t.write(&[0u8; 31]).unwrap_err();
    assert!(matches!(err, LuksError::UsbErrno { errno, .. } if errno == libc::ETIMEDOUT), "{err}");
    assert_eq!(t.max_transfer(), before, "CBW timeout shrank the limit");

    timeout_round(&mock);
    let err = t.read(&mut [0u8; 13]).unwrap_err();
    assert!(matches!(err, LuksError::UsbErrno { errno, .. } if errno == libc::ETIMEDOUT), "{err}");
    assert_eq!(t.max_transfer(), before, "CSW timeout shrank the limit");
}

#[test]
fn a_data_phase_timeout_shrinks_to_half_the_attempt_and_stays_shrunk() {
    let mock = Arc::new(MockRawUsbFs::new());
    let t = transport(&mock);

    // A 128 KiB data phase times out: the next attempt must be smaller than
    // the one that failed, i.e. 64 KiB.
    timeout_round(&mock);
    t.write(&vec![0u8; 128 * 1024]).unwrap_err();
    assert_eq!(t.max_transfer(), 64 * 1024, "a data-phase timeout must halve what was attempted");

    // The limit only goes down within a session: a long run of clean
    // transfers must not probe upward (a probe is a user write).
    for _ in 0..1024 {
        ok_round(&mock);
        t.write(&vec![0u8; 64 * 1024]).unwrap();
    }
    assert_eq!(t.max_transfer(), 64 * 1024, "the limit rose without a re-open");

    // A fresh transport starts at the original value.
    let fresh = transport(&mock);
    assert_eq!(fresh.max_transfer(), UsbFsTransport::DEFAULT_MAX_TRANSFER);
}

#[test]
fn a_cbw_sized_kernel_refusal_does_not_shrink_max_transfer() {
    let mock = Arc::new(MockRawUsbFs::new());
    let t = transport(&mock);
    mock.set_submit_errno(Some(libc::EINVAL));
    let err = t.write(&[0u8; 31]).unwrap_err();
    assert!(matches!(err, LuksError::UsbErrno { errno, .. } if errno == libc::EINVAL), "{err}");
    assert_eq!(t.max_transfer(), UsbFsTransport::DEFAULT_MAX_TRANSFER);
}

#[test]
fn a_size_the_kernel_refused_is_not_retried_and_the_limit_shrinks_from_the_attempt() {
    let mock = Arc::new(MockRawUsbFs::new());
    let t = transport(&mock);

    // The kernel rejects every submit with EINVAL: the limit walks down from
    // the attempted size (64 KiB), not from the 1 MiB limit, to the floor.
    mock.set_submit_errno(Some(libc::EINVAL));
    let err = t.write(&vec![0u8; 64 * 1024]).unwrap_err();
    assert!(matches!(err, LuksError::UsbErrno { errno, .. } if errno == libc::EINVAL), "{err}");
    assert_eq!(t.max_transfer(), UsbFsTransport::MIN_MAX_TRANSFER);

    // Nothing raises it again, however many transfers succeed.
    mock.set_submit_errno(None);
    for _ in 0..768 {
        ok_round(&mock);
        t.write(&vec![0u8; 4 * 1024]).unwrap();
    }
    assert_eq!(t.max_transfer(), UsbFsTransport::MIN_MAX_TRANSFER);
}
