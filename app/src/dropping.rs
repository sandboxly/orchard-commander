//! Files dropped from the system file manager. winit reports only the
//! paths, so the pointer position and the modifier keys held at the drop
//! are asked from the system: they choose the panel (or the folder row) and
//! copy or move, like the system file manager does.

use oc_grid::winit;
use oc_ui::DropAction;
use winit::dpi::PhysicalPosition;
use winit::window::Window;

/// The pointer in the window's content area (physical pixels) if the system
/// says, and the copy / move the held keys ask for.
pub fn pointer_and_action(window: &Window) -> (Option<PhysicalPosition<f64>>, DropAction) {
    imp::state(window)
}

#[cfg(target_os = "macos")]
mod imp {
    use super::*;
    use objc2_app_kit::{NSEvent, NSEventModifierFlags, NSView};
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};

    pub fn state(window: &Window) -> (Option<PhysicalPosition<f64>>, DropAction) {
        // Finder: ⌥ copies, ⌘ moves.
        let flags = NSEvent::modifierFlags_class();
        let action = match (flags.contains(NSEventModifierFlags::Option), flags.contains(NSEventModifierFlags::Command)) {
            (true, false) => DropAction::Copy,
            (false, true) => DropAction::Move,
            _ => DropAction::Auto,
        };
        (pointer(window), action)
    }

    fn pointer(window: &Window) -> Option<PhysicalPosition<f64>> {
        let RawWindowHandle::AppKit(h) = window.window_handle().ok()?.as_raw() else {
            return None;
        };
        // SAFETY: winit's content view, alive while the window is; this runs
        // on the main thread (the event loop).
        let view: &NSView = unsafe { h.ns_view.cast::<NSView>().as_ref() };
        let in_window = view.window()?.mouseLocationOutsideOfEventStream();
        let p = view.convertPoint_fromView(in_window, None);
        let y = if view.isFlipped() { p.y } else { view.bounds().size.height - p.y };
        let scale = window.scale_factor();
        Some(PhysicalPosition::new(p.x * scale, y * scale))
    }
}

#[cfg(windows)]
mod imp {
    use super::*;
    use windows_sys::Win32::Foundation::POINT;
    use windows_sys::Win32::Graphics::Gdi::ScreenToClient;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_CONTROL, VK_SHIFT};
    use windows_sys::Win32::UI::WindowsAndMessaging::GetCursorPos;
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};

    pub fn state(window: &Window) -> (Option<PhysicalPosition<f64>>, DropAction) {
        // Explorer: Ctrl copies, Shift moves.
        // SAFETY: plain queries of the keyboard state.
        let down = |vk: u16| unsafe { GetAsyncKeyState(i32::from(vk)) } < 0;
        let action = match (down(VK_CONTROL), down(VK_SHIFT)) {
            (true, false) => DropAction::Copy,
            (false, true) => DropAction::Move,
            _ => DropAction::Auto,
        };
        (pointer(window), action)
    }

    fn pointer(window: &Window) -> Option<PhysicalPosition<f64>> {
        let RawWindowHandle::Win32(h) = window.window_handle().ok()?.as_raw() else {
            return None;
        };
        let mut p = POINT { x: 0, y: 0 };
        // SAFETY: p is a valid POINT; the window handle is winit's, alive while the window is.
        let ok = unsafe { GetCursorPos(&mut p) != 0 && ScreenToClient(h.hwnd.get() as _, &mut p) != 0 };
        ok.then(|| PhysicalPosition::new(f64::from(p.x), f64::from(p.y)))
    }
}

/// X11 drops reach winit without the pointer position or keys: the files go
/// to the active panel's folder, copied or moved by the volume rule.
#[cfg(not(any(target_os = "macos", windows)))]
mod imp {
    use super::*;

    pub fn state(_window: &Window) -> (Option<PhysicalPosition<f64>>, DropAction) {
        (None, DropAction::Auto)
    }
}
