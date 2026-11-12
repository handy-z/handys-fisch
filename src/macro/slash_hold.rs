use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering::{Relaxed, SeqCst};

use handy_sdk::input::hooks::KeyEvent;
use windows::Win32::UI::WindowsAndMessaging::{LLKHF_EXTENDED, LLKHF_INJECTED, WM_KEYDOWN, WM_SYSKEYDOWN};

/// LowLevelHooks key handler (true = swallow); reads/writes atomics only. Holds back the '/' that opens Roblox chat
/// while the line is out (hold_slash), so the bot can unequip before the chat box takes the keys; it sets slash_held,
/// and the bot sends '/' itself after. Only a physical, non-extended '/' key down while Roblox is foreground; once
/// held, its key repeats are swallowed too. Injected keys (the bot's own '/') pass through.
pub fn hold_back_slash(
    e: &KeyEvent,
    roblox_foreground: &AtomicBool,
    hold_slash: &AtomicBool,
    slash_held: &AtomicBool,
) -> bool {
    if (e.msg != WM_KEYDOWN && e.msg != WM_SYSKEYDOWN)
        || e.scan != 0x35
        || e.flags & (LLKHF_EXTENDED.0 | LLKHF_INJECTED.0) != 0
        || !roblox_foreground.load(Relaxed)
    {
        return false;
    }
    if !slash_held.load(SeqCst) && !hold_slash.load(SeqCst) {
        return false;
    }
    slash_held.store(true, SeqCst);
    true
}
