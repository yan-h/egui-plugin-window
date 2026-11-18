//! AppKit needs the process's main thread, which the Rust test harness does not use.
//!
//! A real editor window, opened in a window that is never put on screen, so
//! the occlusion it reports is baseview's own, sampled as the view opens: the
//! notice is told once that the editor is not seen. Moving the view to a
//! second window off screen samples again, and tells nothing, since nothing
//! changed. A reveal cannot be produced here: a window reads as visible only
//! once it is on screen and the window server has said so, later and not
//! reliably on a CI runner, so the reveal is the unit test's
//! (`an_occlusion_and_a_reveal_are_told_once_each`).
//!
//! The window needs a GPU, which a CI runner may lack: then it says so and
//! checks nothing.
#[cfg(target_os = "macos")]
mod macos {
    use std::ffi::c_void;
    use std::sync::{Arc, Mutex};

    use egui_baseview::{EguiWindow, EguiWindowSettings, VisibilityNotice};
    use objc2::rc::{autoreleasepool, Retained};
    use objc2::{MainThreadMarker, MainThreadOnly};
    use objc2_app_kit::{
        NSApplication, NSBackingStoreType, NSView, NSWindow, NSWindowOcclusionState,
        NSWindowStyleMask,
    };
    use objc2_foundation::{NSPoint, NSRect, NSSize};
    use raw_window_handle::{AppKitWindowHandle, HasRawWindowHandle, RawWindowHandle};

    struct Parent(*mut c_void);

    unsafe impl HasRawWindowHandle for Parent {
        fn raw_window_handle(&self) -> RawWindowHandle {
            let mut handle = AppKitWindowHandle::empty();
            handle.ns_view = self.0;
            RawWindowHandle::AppKit(handle)
        }
    }

    fn off_screen(mtm: MainThreadMarker) -> Retained<NSWindow> {
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                NSRect::new(NSPoint::ZERO, NSSize::new(100.0, 100.0)),
                NSWindowStyleMask::Titled,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        unsafe { window.setReleasedWhenClosed(false) };
        assert!(
            !window.occlusionState().contains(NSWindowOcclusionState::Visible),
            "a window never ordered in is not visible",
        );
        window
    }

    pub fn run() {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        if pollster::block_on(instance.request_adapter(&Default::default())).is_err() {
            println!("visibility notice: no GPU adapter here, nothing checked");
            return;
        }
        autoreleasepool(|_| {
            let mtm = MainThreadMarker::new().expect("native test runs on the main thread");
            let _app = NSApplication::sharedApplication(mtm);
            let window = off_screen(mtm);
            let host = window.contentView().unwrap();
            let told = Arc::new(Mutex::new(Vec::new()));
            let log = told.clone();
            let settings = EguiWindowSettings::new()
                .with_logical_size(egui_baseview::baseview::Size::new(100.0, 100.0))
                .with_visibility_notice(VisibilityNotice::new(move |seen| {
                    log.lock().unwrap().push(seen)
                }));
            let mut handle = EguiWindow::open_parented(
                &Parent(Retained::as_ptr(&host).cast_mut().cast()),
                settings,
                (),
                |_, _, _| {},
                |_, _, _| {},
            );
            assert_eq!(*told.lock().unwrap(), [false], "opened unseen, told once that it is not seen");

            let RawWindowHandle::AppKit(raw) = handle.raw_window_handle() else { unreachable!() };
            let view = unsafe { Retained::retain(raw.ns_view.cast::<NSView>()).unwrap() };
            let other = off_screen(mtm);
            other.contentView().unwrap().addSubview(&view);
            assert_eq!(*told.lock().unwrap(), [false], "a move to a window as unseen told again");

            handle.close();
            window.close();
            other.close();
            println!("visibility notice: told once at open, and not again on a move");
        });
    }
}

fn main() {
    #[cfg(target_os = "macos")]
    macos::run();
}
