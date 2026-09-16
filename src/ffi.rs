//! Owned JSON messages at the Swift/Rust boundary. All engine work runs off the UI thread.
use crate::app::NativeApp;
use std::{
    ffi::{CStr, CString, c_char},
    path::PathBuf,
    sync::mpsc,
    thread::JoinHandle,
    time::Duration,
};

type Reply = mpsc::Sender<String>;
pub struct ZflowApp {
    commands: mpsc::Sender<(String, Reply)>,
    thread: Option<JoinHandle<()>>,
}

fn response(result: anyhow::Result<serde_json::Value>) -> String {
    match result {
        Ok(snapshot) => serde_json::json!({"snapshot":snapshot}),
        Err(error) => serde_json::json!({"error":format!("{error:#}")}),
    }
    .to_string()
}
fn string(value: String) -> *mut c_char {
    CString::new(value)
        .expect("JSON contains no NUL")
        .into_raw()
}

/// # Safety
/// `path` must be a valid UTF-8 C string; `error` must point to writable pointer storage.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zflow_app_create(
    path: *const c_char,
    error: *mut *mut c_char,
) -> *mut ZflowApp {
    if path.is_null() || error.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: the caller supplies a C string and writable error output.
    unsafe {
        *error = std::ptr::null_mut();
    }
    let result = std::panic::catch_unwind(|| -> anyhow::Result<ZflowApp> {
        // SAFETY: checked nonnull; validity is the caller's contract.
        let path = PathBuf::from(unsafe { CStr::from_ptr(path) }.to_str()?);
        let _ = tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| "warn,zflow=info".into()),
            )
            .with_writer(std::io::stderr)
            .with_ansi(false)
            .try_init();
        let mut app = NativeApp::open(path)?;
        let (commands, requests) = mpsc::channel::<(String, Reply)>();
        let thread = std::thread::Builder::new()
            .name("zflow-app".into())
            .spawn(move || {
                loop {
                    match requests.recv_timeout(Duration::from_millis(12)) {
                        Ok((request, reply)) => {
                            let result = serde_json::from_str(&request)
                                .map_err(anyhow::Error::from)
                                .and_then(|request| app.request(request));
                            let _ = reply.send(response(result));
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                    app.tick();
                }
            })?;
        Ok(ZflowApp {
            commands,
            thread: Some(thread),
        })
    })
    .unwrap_or_else(|_| Err(anyhow::anyhow!("The sharing engine could not start")));
    match result {
        Ok(app) => Box::into_raw(Box::new(app)),
        Err(problem) => {
            // SAFETY: writable output supplied by the caller.
            unsafe {
                *error = string(format!("{problem:#}"));
            }
            std::ptr::null_mut()
        }
    }
}

/// # Safety
/// `app` must be a live handle and `request` a valid UTF-8 C string. Calls must be serialized.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zflow_app_request(
    app: *mut ZflowApp,
    request: *const c_char,
) -> *mut c_char {
    if app.is_null() || request.is_null() {
        return string(response(Err(anyhow::anyhow!("Missing request"))));
    }
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(
        || -> anyhow::Result<String> {
            // SAFETY: live handle and C string are required by the caller contract.
            let (app, request) = unsafe { (&*app, CStr::from_ptr(request).to_str()?) };
            let (send, receive) = mpsc::channel();
            app.commands.send((request.to_owned(), send))?;
            Ok(receive.recv_timeout(Duration::from_secs(15))?)
        },
    ))
    .unwrap_or_else(|_| Err(anyhow::anyhow!("The sharing engine stopped unexpectedly")));
    string(result.unwrap_or_else(|error| response(Err(error))))
}

/// # Safety
/// Free each string returned by this API exactly once, or pass null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zflow_string_free(value: *mut c_char) {
    if !value.is_null() {
        unsafe {
            drop(CString::from_raw(value));
        }
    }
}

/// # Safety
/// Destroy a live handle exactly once after all requests finish, or pass null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zflow_app_destroy(app: *mut ZflowApp) {
    if app.is_null() {
        return;
    }
    // SAFETY: caller transfers its handle back and guarantees no outstanding calls.
    let mut app = unsafe { Box::from_raw(app) };
    let thread = app.thread.take();
    drop(app);
    if let Some(thread) = thread {
        let _ = thread.join();
    }
}
