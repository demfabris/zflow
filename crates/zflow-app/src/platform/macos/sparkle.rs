//! Sparkle, which owns the Mac's update schedule, download, install and
//! relaunch. Its framework is loaded from the bundle at run time, so a build
//! without it simply cannot update.
#![allow(
    unsafe_code,
    reason = "Sparkle is an Objective-C framework loaded at run time, so its messages have no typed binding"
)]

use async_channel::Sender;
use objc2::{
    DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send,
    rc::Retained,
    runtime::{AnyClass, AnyObject, Bool, NSObject, NSObjectProtocol},
};
use objc2_foundation::{NSBundle, NSString};

use crate::platform::AppLocation;

/// What Sparkle tells the app.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// A newer version, and whether Sparkle shows it itself. One it does not
    /// show is from a background check, which deserves a notification.
    Found { version: String, shown: bool },
    /// The update session ended: installed, skipped or dismissed.
    Finished,
}

pub struct Sparkle {
    controller: Retained<AnyObject>,
    _delegate: Retained<Delegate>,
}

struct Ivars {
    events: Sender<Event>,
}

define_class!(
    // SAFETY:
    // - `NSObject` has no subclassing requirements.
    // - `Delegate` does not implement `Drop`.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = Ivars]
    struct Delegate;

    impl Delegate {
        #[unsafe(method(supportsGentleScheduledUpdateReminders))]
        fn gentle(&self) -> Bool {
            Bool::YES
        }

        // A background check adds a menu bar reminder without interrupting
        // shared input.
        #[unsafe(method(standardUserDriverShouldHandleShowingScheduledUpdate:andInImmediateFocus:))]
        fn should_show(&self, _update: &AnyObject, immediate: Bool) -> Bool {
            immediate
        }

        #[unsafe(method(standardUserDriverWillHandleShowingUpdate:forUpdate:state:))]
        fn will_show(&self, handles: Bool, update: &AnyObject, _state: &AnyObject) {
            let version: Option<Retained<NSString>> = unsafe { msg_send![update, displayVersionString] };
            if let Some(version) = version {
                let _ = self.ivars().events.try_send(Event::Found {
                    version: version.to_string(),
                    shown: handles.as_bool(),
                });
            }
        }

        #[unsafe(method(standardUserDriverWillFinishUpdateSession))]
        fn finished(&self) {
            let _ = self.ivars().events.try_send(Event::Finished);
        }
    }

    unsafe impl NSObjectProtocol for Delegate {}
);

impl Delegate {
    fn new(events: Sender<Event>, mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(Ivars { events });
        // SAFETY: `NSObject`'s designated initializer, on a freshly
        // allocated instance whose ivars are in place.
        unsafe { msg_send![super(this), init] }
    }
}

/// Why this copy cannot update itself, from its Info.plist and where it
/// runs from.
pub fn configuration_issue(location: &AppLocation) -> Option<String> {
    let bundle = NSBundle::mainBundle();
    let text = |key: &str| {
        bundle
            .objectForInfoDictionaryKey(&NSString::from_str(key))
            .and_then(|value| value.downcast::<NSString>().ok())
            .map(|value| value.to_string())
    };
    let key_ok = text("SUPublicEDKey").is_some_and(|key| base64_len(&key) == Some(32));
    let feed_ok = text("SUFeedURL").is_some_and(|feed| {
        feed.strip_prefix("https://")
            .is_some_and(|rest| rest.split('/').next().is_some_and(|host| !host.is_empty()))
    });
    if !key_ok || !feed_ok {
        return Some("Updates are available in release builds of zflow.".into());
    }
    location
        .needs_move()
        .then(|| "Move zflow to Applications, then open it there to check for updates.".into())
}

/// How many bytes a base64 text decodes to, if it is base64.
fn base64_len(text: &str) -> Option<usize> {
    let text = text.trim();
    if text.is_empty()
        || !text.len().is_multiple_of(4)
        || !text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/' | b'='))
    {
        return None;
    }
    let padding = text.bytes().rev().take_while(|byte| *byte == b'=').count();
    (padding <= 2).then(|| text.len() / 4 * 3 - padding)
}

impl Sparkle {
    /// Loads Sparkle from the bundle and starts its updater.
    pub fn start(events: Sender<Event>, mtm: MainThreadMarker) -> Result<Self, String> {
        let unavailable = || "Updates are available in release builds of zflow.".to_owned();
        let frameworks = NSBundle::mainBundle()
            .privateFrameworksPath()
            .ok_or_else(unavailable)?;
        let path = NSString::from_str(&format!("{frameworks}/Sparkle.framework"));
        let framework = NSBundle::bundleWithPath(&path).ok_or_else(unavailable)?;
        // SAFETY: loading a framework from inside this app's own bundle.
        let loaded: Bool = unsafe { msg_send![&framework, load] };
        if !loaded.as_bool() {
            return Err(unavailable());
        }
        let class = AnyClass::get(c"SPUStandardUpdaterController").ok_or_else(unavailable)?;
        let delegate = Delegate::new(events, mtm);
        let none: Option<&AnyObject> = None;
        // SAFETY: Sparkle 2's designated initializer, which keeps the
        // delegate weakly; `_delegate` keeps it alive.
        let controller: Option<Retained<AnyObject>> = unsafe {
            let allocated: objc2::rc::Allocated<AnyObject> = msg_send![class, alloc];
            msg_send![allocated, initWithStartingUpdater: Bool::NO, updaterDelegate: none, userDriverDelegate: &*delegate]
        };
        let controller = controller.ok_or_else(unavailable)?;
        let sparkle = Self {
            controller,
            _delegate: delegate,
        };
        let updater = sparkle.updater();
        let mut error: *mut AnyObject = std::ptr::null_mut();
        // SAFETY: `startUpdater:` takes an `NSError **`.
        let started: Bool = unsafe { msg_send![&updater, startUpdater: &mut error] };
        if !started.as_bool() {
            let reason = (!error.is_null())
                .then(|| {
                    // SAFETY: a non-null error Sparkle wrote back.
                    let description: Option<Retained<NSString>> =
                        unsafe { msg_send![&*error, localizedDescription] };
                    description.map(|text| text.to_string())
                })
                .flatten();
            return Err(reason.unwrap_or_else(unavailable));
        }
        Ok(sparkle)
    }

    fn updater(&self) -> Retained<AnyObject> {
        // SAFETY: `SPUStandardUpdaterController.updater` is never nil once
        // initialized.
        unsafe { msg_send![&self.controller, updater] }
    }

    pub fn can_check(&self) -> bool {
        let updater = self.updater();
        // SAFETY: a BOOL property of `SPUUpdater`.
        let can: Bool = unsafe { msg_send![&updater, canCheckForUpdates] };
        can.as_bool()
    }

    pub fn checks_automatically(&self) -> bool {
        let updater = self.updater();
        // SAFETY: a BOOL property of `SPUUpdater`.
        let on: Bool = unsafe { msg_send![&updater, automaticallyChecksForUpdates] };
        on.as_bool()
    }

    pub fn set_checks_automatically(&self, on: bool) {
        let updater = self.updater();
        // SAFETY: the BOOL property's setter.
        let () = unsafe { msg_send![&updater, setAutomaticallyChecksForUpdates: Bool::new(on)] };
    }

    pub fn downloads_automatically(&self) -> bool {
        let updater = self.updater();
        // SAFETY: a BOOL property of `SPUUpdater`.
        let on: Bool = unsafe { msg_send![&updater, automaticallyDownloadsUpdates] };
        on.as_bool()
    }

    pub fn set_downloads_automatically(&self, on: bool) {
        let updater = self.updater();
        // SAFETY: the BOOL property's setter.
        let () = unsafe { msg_send![&updater, setAutomaticallyDownloadsUpdates: Bool::new(on)] };
    }

    /// Opens Sparkle's own check, which shows what it finds.
    pub fn check(&self) {
        let none: Option<&AnyObject> = None;
        // SAFETY: `checkForUpdates:` takes a sender, which may be nil.
        let () = unsafe { msg_send![&self.controller, checkForUpdates: none] };
    }
}

#[cfg(test)]
mod tests {
    use super::base64_len;

    #[test]
    fn a_sparkle_key_is_32_bytes_of_base64() {
        assert_eq!(
            base64_len("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="),
            Some(32)
        );
        assert_eq!(base64_len("not base64!"), None);
        assert_eq!(base64_len(""), None);
    }
}
