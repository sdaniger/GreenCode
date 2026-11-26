//! AVKit owns decoding and playback controls; GPUI owns the preview panel layout.

use gpui_kit::*;
use objc2::{class, msg_send, rc::Retained, runtime::AnyObject, MainThreadMarker};
use objc2_app_kit::NSView;
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString, NSURL};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::{path::Path, rc::Rc};

#[link(name = "AVFoundation", kind = "framework")]
#[link(name = "AVKit", kind = "framework")]
unsafe extern "C" {}

pub(super) struct Player {
    view: Retained<NSView>,
    player: Retained<AnyObject>,
}

impl Player {
    pub(super) fn new(path: &Path) -> Option<Self> {
        let _main = MainThreadMarker::new()?;
        let url = NSURL::fileURLWithPath_isDirectory(&NSString::from_str(path.to_str()?), false);
        // AVPlayerView is an NSView subclass. All calls are on the UI thread.
        unsafe {
            let player: Retained<AnyObject> = msg_send![class!(AVPlayer), playerWithURL: &*url];
            let view: Retained<NSView> = msg_send![class!(AVPlayerView), new];
            let _: () = msg_send![&*view, setPlayer: &*player];
            let _: () = msg_send![&*view, setShowsFullScreenToggleButton: true];
            Some(Self { view, player })
        }
    }

    pub(super) fn toggle_playback(&self) {
        unsafe {
            let rate: f32 = msg_send![&*self.player, rate];
            if rate == 0. {
                let _: () = msg_send![&*self.player, play];
            } else {
                let _: () = msg_send![&*self.player, pause];
            }
        }
    }

    pub(super) fn hide(&self) {
        if !self.view.isHidden() {
            unsafe { let _: () = msg_send![&*self.player, pause]; }
            self.view.setHidden(true);
        }
    }

    pub(super) fn element(self: Rc<Self>) -> AnyElement {
        canvas(|_, _, _| {}, move |bounds, (), window, _| {
            let Ok(handle) = HasWindowHandle::window_handle(window) else {
                return;
            };
            let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
                return;
            };
            // The borrowed window handle keeps the host NSView valid for this paint.
            let host = unsafe { &*handle.ns_view.as_ptr().cast::<NSView>() };
            let x = f64::from(bounds.origin.x);
            let top = f64::from(bounds.origin.y);
            let width = f64::from(bounds.size.width);
            let height = f64::from(bounds.size.height);
            if width <= 0. || height <= 0. {
                self.hide();
                return;
            }
            let y = if host.isFlipped() {
                top
            } else {
                host.bounds().size.height - top - height
            };
            let frame = NSRect::new(NSPoint::new(x, y), NSSize::new(width, height));
            if self.view.frame() != frame {
                self.view.setFrame(frame);
            }
            if unsafe { self.view.superview() }.is_none() {
                host.addSubview(&self.view);
            }
            self.view.setHidden(false);
        })
        .w_full()
        .flex_1()
        .into_any_element()
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.hide();
        self.view.removeFromSuperview();
    }
}
