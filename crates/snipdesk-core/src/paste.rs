use std::thread;
use std::time::Duration;

/// Capture the paste target before the launcher steals focus.
/// Windows: foreground HWND. macOS: frontmost app pid. Elsewhere:
/// 0 = no target, the WM's own focus return has to do.
pub fn capture_foreground_hwnd() -> isize {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::UI::WindowsAndMessaging::GetForegroundWindow;
        GetForegroundWindow() as isize
    }
    #[cfg(target_os = "macos")]
    {
        use objc2_app_kit::NSWorkspace;
        unsafe {
            NSWorkspace::sharedWorkspace()
                .frontmostApplication()
                .map(|app| app.processIdentifier() as isize)
                .unwrap_or(0)
        }
    }
    #[cfg(all(not(windows), not(target_os = "macos")))]
    {
        0
    }
}

/// Best-effort. False on no-target / dead window / OS refusal.
#[cfg(windows)]
pub fn restore_foreground(hwnd: isize) -> bool {
    if hwnd == 0 {
        return false;
    }
    unsafe {
        use windows_sys::Win32::Foundation::HWND;
        use windows_sys::Win32::UI::WindowsAndMessaging::{IsWindow, SetForegroundWindow};
        let h = hwnd as HWND;
        if IsWindow(h) == 0 {
            return false;
        }
        SetForegroundWindow(h) != 0
    }
}

/// macOS: re-activate the target app by pid before Cmd+V. Hiding
/// our window doesn't reliably hand focus back (an app can stay
/// active with zero windows).
#[cfg(target_os = "macos")]
pub fn restore_foreground(pid: isize) -> bool {
    use objc2_app_kit::{NSApplicationActivationOptions, NSRunningApplication};
    if pid <= 0 {
        return false;
    }
    unsafe {
        match NSRunningApplication::runningApplicationWithProcessIdentifier(pid as i32) {
            Some(app) => app.activateWithOptions(
                NSApplicationActivationOptions::NSApplicationActivateIgnoringOtherApps,
            ),
            None => false,
        }
    }
}

#[cfg(all(not(windows), not(target_os = "macos")))]
pub fn restore_foreground(_hwnd: isize) -> bool {
    false
}

/// macOS drops synthetic keystrokes until the app is approved under
/// Accessibility. `prompt` also triggers the system approval dialog
/// (shown at most once).
#[cfg(target_os = "macos")]
pub fn macos_accessibility_trusted(prompt: bool) -> bool {
    use std::ffi::c_void;

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        static kCFTypeDictionaryKeyCallBacks: c_void;
        static kCFTypeDictionaryValueCallBacks: c_void;
        static kCFBooleanTrue: *const c_void;
        fn CFDictionaryCreate(
            allocator: *const c_void,
            keys: *const *const c_void,
            values: *const *const c_void,
            num_values: isize,
            key_callbacks: *const c_void,
            value_callbacks: *const c_void,
        ) -> *const c_void;
        fn CFRelease(cf: *const c_void);
    }
    #[link(name = "ApplicationServices", kind = "framework")]
    extern "C" {
        static kAXTrustedCheckOptionPrompt: *const c_void;
        fn AXIsProcessTrusted() -> bool;
        fn AXIsProcessTrustedWithOptions(options: *const c_void) -> bool;
    }

    unsafe {
        if !prompt {
            return AXIsProcessTrusted();
        }
        let keys = [kAXTrustedCheckOptionPrompt];
        let values = [kCFBooleanTrue];
        let options = CFDictionaryCreate(
            std::ptr::null(),
            keys.as_ptr(),
            values.as_ptr(),
            1,
            &kCFTypeDictionaryKeyCallBacks as *const c_void,
            &kCFTypeDictionaryValueCallBacks as *const c_void,
        );
        let trusted = AXIsProcessTrustedWithOptions(options);
        if !options.is_null() {
            CFRelease(options);
        }
        trusted
    }
}

/// Why auto-paste can't work right now, or None when it can.
/// Callers fall back to clipboard-copy and show the reason.
/// macOS: needs the Accessibility permission. Wayland: compositors
/// reject synthetic input and enigo's backend is X11.
pub fn auto_paste_blocker(prompt_for_permission: bool) -> Option<String> {
    #[cfg(windows)]
    {
        let _ = prompt_for_permission;
        None
    }
    #[cfg(target_os = "macos")]
    {
        if macos_accessibility_trusted(prompt_for_permission) {
            None
        } else {
            Some(
                "Auto-paste needs the Accessibility permission: approve SnipDesk under \
                 System Settings > Privacy & Security > Accessibility, then try again. \
                 This snippet was copied to the clipboard instead."
                    .to_string(),
            )
        }
    }
    #[cfg(all(not(windows), not(target_os = "macos")))]
    {
        let _ = prompt_for_permission;
        let wayland = std::env::var("WAYLAND_DISPLAY").is_ok()
            && std::env::var("XDG_SESSION_TYPE")
                .map(|v| v != "x11")
                .unwrap_or(true);
        if wayland {
            Some(
                "Auto-paste isn't available on Wayland sessions; this snippet was \
                 copied to the clipboard instead. Press Ctrl+V to paste it."
                    .to_string(),
            )
        } else {
            None
        }
    }
}

/// Write CF_UNICODETEXT directly. arboard (via tauri-plugin-clipboard-manager)
/// sets CF_TEXT with UTF-8 bytes, which targets reading CF_UNICODETEXT first
/// decode as Windows-1252 - the classic mojibake for em dashes and curly
/// quotes.
#[cfg(windows)]
pub fn write_clipboard_unicode(text: &str) -> Result<(), String> {
    use windows_sys::Win32::Foundation::{HANDLE, HWND};
    use windows_sys::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
    };
    use windows_sys::Win32::System::Memory::{
        GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE,
    };
    use windows_sys::Win32::System::Ole::CF_UNICODETEXT;

    // CF_UNICODETEXT requires a zero-terminated wide string.
    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0u16)).collect();
    let byte_len = wide.len() * std::mem::size_of::<u16>();

    unsafe {
        // The clipboard is single-owner; VS Code / browsers / screenshot tools
        // occasionally hold it briefly after a copy.
        let mut opened = false;
        for attempt in 0..8 {
            if OpenClipboard(0 as HWND) != 0 {
                opened = true;
                break;
            }
            thread::sleep(Duration::from_millis(5 * (attempt + 1) as u64));
        }
        if !opened {
            return Err("OpenClipboard failed".into());
        }

        // Must CloseClipboard before returning - the handle is process-global.
        let result = (|| -> Result<(), String> {
            if EmptyClipboard() == 0 {
                return Err("EmptyClipboard failed".into());
            }

            // GMEM_MOVEABLE is required - SetClipboardData takes ownership and
            // the clipboard manager may relocate the block.
            let hmem = GlobalAlloc(GMEM_MOVEABLE, byte_len);
            if hmem.is_null() {
                return Err("GlobalAlloc failed".into());
            }

            let dst = GlobalLock(hmem) as *mut u16;
            if dst.is_null() {
                // hmem leaks here; GlobalFree isn't in our enabled features.
                return Err("GlobalLock failed".into());
            }
            std::ptr::copy_nonoverlapping(wide.as_ptr(), dst, wide.len());
            GlobalUnlock(hmem);

            // After SetClipboardData succeeds, hmem is owned by the system.
            if SetClipboardData(CF_UNICODETEXT as u32, hmem as HANDLE).is_null() {
                return Err("SetClipboardData failed".into());
            }

            Ok(())
        })();

        CloseClipboard();
        result
    }
}

#[cfg(not(windows))]
pub fn write_clipboard_unicode(_text: &str) -> Result<(), String> {
    // arboard is fine on macOS/Linux; only the Windows path is buggy.
    Err("write_clipboard_unicode is Windows-only".into())
}

/// Paste the clipboard into `target_hwnd`. Caller must have written the
/// snippet to the clipboard first (use_snippet step 3).
///
/// Strategy on Windows: WM_PASTE first (same path as right-click -> Paste),
/// SendInput Ctrl+V as fallback.
///
/// Why WM_PASTE: synthetic Ctrl+V leaks the modifier into apps that bind
/// bare Ctrl (ticketing/chat command palettes), interacts badly with
/// modifiers the user is still holding from the launcher hotkey, and on
/// Notepad has occasionally tripped the menu-accelerator before V arrives.
///
/// Caveat: WM_PASTE works on Win32 edit, RichEdit, and Scintilla controls
/// (Notepad, file dialogs, Word, WordPad, Outlook, Notepad++). It does NOT
/// work on Chromium (Chrome/Edge/Slack/Discord/Teams/VS Code/Electron) -
/// the text area isn't a real Win32 control. We detect those by class name
/// and route to SendInput.
///
/// We re-restore focus before pasting because after the variable-prompt
/// flow, Windows' own post-hide focus restoration is stale and both
/// WM_PASTE and Ctrl+V race into nothing.
pub fn trigger_paste(delay_ms: u64, target_hwnd: isize) {
    // macOS: AppKit activation stays on the calling (main) thread;
    // only the delay + Cmd+V moves to the worker.
    #[cfg(target_os = "macos")]
    let _ = restore_foreground(target_hwnd);

    thread::spawn(move || {
        if delay_ms > 0 {
            thread::sleep(Duration::from_millis(delay_ms));
        }

        #[cfg(not(target_os = "macos"))]
        {
            let restored = restore_foreground(target_hwnd);
            if restored {
                // Empirical settle time on Win10/11 between
                // SetForegroundWindow succeeding and the target being
                // ready for input.
                thread::sleep(Duration::from_millis(40));
            }
        }

        #[cfg(windows)]
        dispatch_paste_windows(target_hwnd);

        #[cfg(not(windows))]
        send_paste_fallback();
    });
}

/// Chromium -> SendInput. Otherwise WM_PASTE, then SendInput as fallback.
#[cfg(windows)]
fn dispatch_paste_windows(target_hwnd: isize) {
    if target_hwnd != 0 && is_chromium_window(target_hwnd) {
        send_ctrl_v_windows();
        return;
    }
    if target_hwnd != 0 && try_wm_paste(target_hwnd) {
        return;
    }
    send_ctrl_v_windows();
}

/// `Chrome_WidgetWin_*` covers Chrome/Edge/Slack/Discord/VS Code/Electron.
/// Firefox (`MozillaWindowClass`) is also non-WM_PASTE-friendly.
#[cfg(windows)]
fn is_chromium_window(hwnd: isize) -> bool {
    use windows_sys::Win32::Foundation::HWND;
    use windows_sys::Win32::UI::WindowsAndMessaging::GetClassNameW;

    unsafe {
        let mut buf = [0u16; 256];
        let len = GetClassNameW(hwnd as HWND, buf.as_mut_ptr(), buf.len() as i32);
        if len <= 0 {
            return false;
        }
        let class = String::from_utf16_lossy(&buf[..len as usize]);
        class.starts_with("Chrome_WidgetWin") || class == "MozillaWindowClass"
    }
}

/// AttachThreadInput is required so GetFocus() returns the focused child
/// control inside the target - without it we'd see our own focus or 0.
#[cfg(windows)]
fn try_wm_paste(target_hwnd: isize) -> bool {
    use windows_sys::Win32::Foundation::HWND;
    use windows_sys::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetFocus;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetWindowThreadProcessId, SendMessageTimeoutW, SMTO_ABORTIFHUNG, SMTO_NORMAL, WM_PASTE,
    };

    unsafe {
        let hwnd = target_hwnd as HWND;

        let target_tid = GetWindowThreadProcessId(hwnd, std::ptr::null_mut());
        if target_tid == 0 {
            return false;
        }
        let our_tid = GetCurrentThreadId();
        if our_tid == target_tid {
            return false;
        }

        if AttachThreadInput(our_tid, target_tid, 1) == 0 {
            return false;
        }
        let focused = GetFocus();
        // Detach immediately - leaving it attached interferes with the
        // target's own input handling.
        let _ = AttachThreadInput(our_tid, target_tid, 0);

        if focused.is_null() {
            return false;
        }

        // 1.5s + SMTO_ABORTIFHUNG so a frozen target can't hang us. WM_PASTE's
        // return value is unspecified; nonzero from SendMessageTimeoutW means
        // the call dispatched.
        let mut result: usize = 0;
        let ok = SendMessageTimeoutW(
            focused,
            WM_PASTE,
            0,
            0,
            SMTO_NORMAL | SMTO_ABORTIFHUNG,
            1500,
            &mut result,
        );
        ok != 0
    }
}

/// SendInput Ctrl+V. Releases Shift/Alt first so a still-held launcher hotkey
/// modifier doesn't turn this into Ctrl+Shift+V (Paste Special) or Ctrl+Alt+V.
#[cfg(windows)]
fn send_ctrl_v_windows() {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, VIRTUAL_KEY,
        VK_CONTROL, VK_MENU, VK_SHIFT, VK_V,
    };

    unsafe fn key_event(vk: VIRTUAL_KEY, key_up: bool) -> INPUT {
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: vk,
                    wScan: 0,
                    dwFlags: if key_up { KEYEVENTF_KEYUP } else { 0 },
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        }
    }

    unsafe {
        // key-up is a no-op if the key isn't down - cheap to send unconditionally.
        let flush = [
            key_event(VK_SHIFT, true),
            key_event(VK_MENU, true), // VK_MENU == Alt
        ];
        SendInput(
            flush.len() as u32,
            flush.as_ptr(),
            std::mem::size_of::<INPUT>() as i32,
        );

        // Single SendInput batch so another process can't interleave between
        // our V and Ctrl.
        let press = [
            key_event(VK_CONTROL, false),
            key_event(VK_V, false),
            key_event(VK_V, true),
            key_event(VK_CONTROL, true),
        ];
        SendInput(
            press.len() as u32,
            press.as_ptr(),
            std::mem::size_of::<INPUT>() as i32,
        );
    }
}

/// Synthesize Ctrl+C in the foreground app and snapshot the result.
/// Restores the prior clipboard contents - clobbering them is the worst
/// class of "helper app" bug.
///
/// Detection: snapshot GetClipboardSequenceNumber before Ctrl+C, poll for
/// a bump (up to ~400ms). No bump = empty selection / non-text control /
/// Chromium hasn't flushed -> Ok(None).
#[cfg(windows)]
pub fn capture_selection() -> Result<Option<String>, String> {
    use windows_sys::Win32::System::DataExchange::GetClipboardSequenceNumber;

    unsafe {
        let pre_seq = GetClipboardSequenceNumber();

        // Only Unicode text is preserved across the round-trip. Images and
        // file lists are lost - TODO if it bites users.
        let previous = read_clipboard_unicode_text();

        send_ctrl_c_windows();

        // 10ms * 40 = ~400ms. Native controls answer in <50ms; web apps need slack.
        let mut copied = false;
        for _ in 0..40 {
            thread::sleep(Duration::from_millis(10));
            if GetClipboardSequenceNumber() != pre_seq {
                copied = true;
                break;
            }
        }

        let selection = if copied {
            read_clipboard_unicode_text()
        } else {
            None
        };

        // Only restore on a successful capture; otherwise the clipboard is
        // already untouched and rewriting it just bumps the sequence number.
        if copied {
            if let Some(prev_text) = previous {
                if let Err(err) = write_clipboard_unicode(&prev_text) {
                    crate::logging::log_error(&format!(
                        "capture_selection: clipboard restore failed: {err}"
                    ));
                }
            }
            // No prior text - leave our Ctrl+C result in place.
        }

        Ok(selection.filter(|s| !s.is_empty()))
    }
}

/// None = no CF_UNICODETEXT on the clipboard (empty or non-text format).
#[cfg(windows)]
fn read_clipboard_unicode_text() -> Option<String> {
    use windows_sys::Win32::Foundation::HWND;
    use windows_sys::Win32::System::DataExchange::{
        CloseClipboard, GetClipboardData, OpenClipboard,
    };
    use windows_sys::Win32::System::Memory::{GlobalLock, GlobalUnlock};
    use windows_sys::Win32::System::Ole::CF_UNICODETEXT;

    unsafe {
        let mut opened = false;
        for attempt in 0..8 {
            if OpenClipboard(0 as HWND) != 0 {
                opened = true;
                break;
            }
            thread::sleep(Duration::from_millis(5 * (attempt + 1) as u64));
        }
        if !opened {
            return None;
        }

        let result = (|| -> Option<String> {
            let h = GetClipboardData(CF_UNICODETEXT as u32);
            if h.is_null() {
                return None;
            }
            let ptr = GlobalLock(h as _) as *const u16;
            if ptr.is_null() {
                return None;
            }
            // 16 MiB cap so a misbehaving app can't trap us in an infinite walk.
            let mut len = 0usize;
            while len < 8 * 1024 * 1024 {
                if *ptr.add(len) == 0 {
                    break;
                }
                len += 1;
            }
            let slice = std::slice::from_raw_parts(ptr, len);
            let s = String::from_utf16_lossy(slice);
            GlobalUnlock(h as _);
            Some(s)
        })();

        CloseClipboard();
        result
    }
}

/// Mirror of send_ctrl_v_windows with VK_C. Same modifier-flush rationale -
/// stray Shift/Alt from the quick-add hotkey would turn this into
/// Ctrl+Shift+C (devtools) or Ctrl+Alt+C.
#[cfg(windows)]
fn send_ctrl_c_windows() {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, VIRTUAL_KEY, VK_C,
        VK_CONTROL, VK_MENU, VK_SHIFT,
    };

    unsafe fn key_event(vk: VIRTUAL_KEY, key_up: bool) -> INPUT {
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: vk,
                    wScan: 0,
                    dwFlags: if key_up { KEYEVENTF_KEYUP } else { 0 },
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        }
    }

    unsafe {
        let flush = [key_event(VK_SHIFT, true), key_event(VK_MENU, true)];
        SendInput(
            flush.len() as u32,
            flush.as_ptr(),
            std::mem::size_of::<INPUT>() as i32,
        );

        let press = [
            key_event(VK_CONTROL, false),
            key_event(VK_C, false),
            key_event(VK_C, true),
            key_event(VK_CONTROL, true),
        ];
        SendInput(
            press.len() as u32,
            press.as_ptr(),
            std::mem::size_of::<INPUT>() as i32,
        );
    }
}

/// Non-Windows: synthesize the platform copy chord, poll the
/// clipboard for new text, restore the prior contents. Gated by the
/// same blocker as auto-paste (Accessibility / X11). Change
/// detection is text comparison (arboard has no sequence number), so
/// a selection identical to the current clipboard reads as "nothing
/// copied".
#[cfg(not(windows))]
pub fn capture_selection() -> Result<Option<String>, String> {
    if let Some(reason) = auto_paste_blocker(true) {
        return Err(reason);
    }
    let mut clipboard = arboard::Clipboard::new().map_err(|e| e.to_string())?;
    let previous = clipboard.get_text().ok();

    send_copy_chord('c');

    // 10ms * 40 = ~400ms, matching the Windows poll budget.
    let mut selection: Option<String> = None;
    for _ in 0..40 {
        thread::sleep(Duration::from_millis(10));
        if let Ok(now) = clipboard.get_text() {
            if Some(&now) != previous.as_ref() && !now.is_empty() {
                selection = Some(now);
                break;
            }
        }
    }

    if selection.is_some() {
        if let Some(prev) = previous {
            if let Err(err) = clipboard.set_text(prev) {
                crate::logging::log_error(&format!(
                    "capture_selection: clipboard restore failed: {err}"
                ));
            }
        }
    }

    Ok(selection)
}

/// macOS Cmd+V / Linux Ctrl+V via enigo.
#[cfg(not(windows))]
fn send_paste_fallback() {
    send_copy_chord('v');
}

/// Cmd/Ctrl + key with stray launcher-hotkey modifiers flushed
/// first, mirroring the Windows SendInput paths.
#[cfg(not(windows))]
fn send_copy_chord(key_char: char) {
    use enigo::{Direction, Enigo, Key, Keyboard, Settings};

    let mut enigo = match Enigo::new(&Settings::default()) {
        Ok(e) => e,
        Err(err) => {
            eprintln!("enigo init failed: {err}");
            return;
        }
    };

    let _ = enigo.key(Key::Control, Direction::Release);
    let _ = enigo.key(Key::Shift, Direction::Release);
    let _ = enigo.key(Key::Alt, Direction::Release);
    #[cfg(target_os = "macos")]
    let _ = enigo.key(Key::Meta, Direction::Release);

    #[cfg(target_os = "macos")]
    let mod_key = Key::Meta;
    #[cfg(not(target_os = "macos"))]
    let mod_key = Key::Control;

    let _ = enigo.key(mod_key, Direction::Press);
    let _ = enigo.key(Key::Unicode(key_char), Direction::Click);
    let _ = enigo.key(mod_key, Direction::Release);
}
