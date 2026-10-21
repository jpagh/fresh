//! Terminal mode management
//!
//! This module handles enabling and disabling various terminal modes:
//! - Raw mode
//! - Alternate screen
//! - Mouse capture
//! - Keyboard enhancement flags, plus the xterm `modifyOtherKeys` fallback
//!   that multiplexers need (see `modify_other_keys_request`)
//! - Bracketed paste
//!
//! It provides a `TerminalModes` struct that tracks which modes were enabled
//! and can restore the terminal to its original state via the `undo()` method.
//!
//! The `sequences` submodule provides raw ANSI escape sequence constants
//! shared between direct mode (crossterm) and client/server mode (raw bytes).

use anyhow::Result;
use crossterm::{
    cursor::SetCursorStyle,
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
    ExecutableCommand,
};
use std::io::{stdout, Write};

/// Raw ANSI escape sequences for terminal mode control.
///
/// These constants are the canonical source of truth for terminal escape sequences
/// used by both direct mode (`TerminalModes`) and client/server mode
/// (`terminal_setup_sequences` / `terminal_teardown_sequences`).
pub mod sequences {
    // Alternate screen
    pub const ENTER_ALTERNATE_SCREEN: &[u8] = b"\x1b[?1049h";
    pub const LEAVE_ALTERNATE_SCREEN: &[u8] = b"\x1b[?1049l";

    // Mouse tracking (SGR format)
    pub const ENABLE_MOUSE_CLICK: &[u8] = b"\x1b[?1000h";
    pub const ENABLE_MOUSE_DRAG: &[u8] = b"\x1b[?1002h";
    pub const ENABLE_MOUSE_MOTION: &[u8] = b"\x1b[?1003h";
    pub const ENABLE_SGR_MOUSE: &[u8] = b"\x1b[?1006h";
    pub const DISABLE_MOUSE_CLICK: &[u8] = b"\x1b[?1000l";
    pub const DISABLE_MOUSE_DRAG: &[u8] = b"\x1b[?1002l";
    pub const DISABLE_MOUSE_MOTION: &[u8] = b"\x1b[?1003l";
    pub const DISABLE_SGR_MOUSE: &[u8] = b"\x1b[?1006l";

    // Focus events
    pub const ENABLE_FOCUS_EVENTS: &[u8] = b"\x1b[?1004h";
    pub const DISABLE_FOCUS_EVENTS: &[u8] = b"\x1b[?1004l";

    // Bracketed paste
    pub const ENABLE_BRACKETED_PASTE: &[u8] = b"\x1b[?2004h";
    pub const DISABLE_BRACKETED_PASTE: &[u8] = b"\x1b[?2004l";

    // Cursor
    pub const SHOW_CURSOR: &[u8] = b"\x1b[?25h";
    pub const HIDE_CURSOR: &[u8] = b"\x1b[?25l";
    pub const RESET_CURSOR_STYLE: &[u8] = b"\x1b[0 q";

    // Attributes
    pub const RESET_ATTRIBUTES: &[u8] = b"\x1b[0m";

    // xterm `modifyOtherKeys`, sent only where the kitty push is ignored —
    // see `modify_other_keys_request`.
    pub const PUSH_MODIFY_OTHER_KEYS_MODE_2: &[u8] = b"\x1b[>4;2m";
    pub const RESET_MODIFY_OTHER_KEYS: &[u8] = b"\x1b[>4;0m";

    // Kitty keyboard protocol. The push varies with the flags the config asks
    // for, so it is built by `keyboard_enhancement_push`; the pop is fixed.
    // Mirrors the bytes crossterm's `PopKeyboardEnhancementFlags` sends.
    pub const POP_KEYBOARD_ENHANCEMENT_FLAGS: &[u8] = b"\x1b[<1u";
}

/// Configuration for keyboard enhancement flags.
#[derive(Debug, Clone)]
pub struct KeyboardConfig {
    /// Enable CSI-u sequences for unambiguous escape code reading.
    pub disambiguate_escape_codes: bool,
    /// Enable key repeat and release events.
    pub report_event_types: bool,
    /// Enable alternate keycodes.
    pub report_alternate_keys: bool,
    /// Represent all keys as CSI-u escape codes.
    pub report_all_keys_as_escape_codes: bool,
}

impl Default for KeyboardConfig {
    fn default() -> Self {
        Self {
            disambiguate_escape_codes: true,
            report_event_types: false,
            report_alternate_keys: true,
            report_all_keys_as_escape_codes: false,
        }
    }
}

impl KeyboardConfig {
    /// Build crossterm KeyboardEnhancementFlags from this config.
    pub fn to_flags(&self) -> KeyboardEnhancementFlags {
        let mut flags = KeyboardEnhancementFlags::empty();
        if self.disambiguate_escape_codes {
            flags |= KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES;
        }
        if self.report_event_types {
            flags |= KeyboardEnhancementFlags::REPORT_EVENT_TYPES;
        }
        if self.report_alternate_keys {
            flags |= KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS;
        }
        if self.report_all_keys_as_escape_codes {
            flags |= KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES;
        }
        flags
    }

    /// Returns true if any flags are enabled.
    pub fn any_enabled(&self) -> bool {
        self.disambiguate_escape_codes
            || self.report_event_types
            || self.report_alternate_keys
            || self.report_all_keys_as_escape_codes
    }

    /// The flags the config's `editor.keyboard_*` settings ask for.
    ///
    /// Both terminal paths build their request from here: `TerminalModes::enable`
    /// for a direct launch, and the daemon for the clients it sets up.
    pub fn from_config(config: &crate::config::Config) -> Self {
        Self {
            disambiguate_escape_codes: config.editor.keyboard_disambiguate_escape_codes,
            report_event_types: config.editor.keyboard_report_event_types,
            report_alternate_keys: config.editor.keyboard_report_alternate_keys,
            report_all_keys_as_escape_codes: config.editor.keyboard_report_all_keys_as_escape_codes,
        }
    }
}

/// The xterm `modifyOtherKeys` request to send alongside the kitty push, if
/// this terminal needs one.
///
/// tmux ignores the kitty keyboard-protocol push: `CSI > … u` from an
/// application leaves the pane in tmux's legacy key mode whatever flags it
/// asks for, and tmux then encodes modified keys from its own key table. That
/// table has no entry for `Ctrl+Shift+<letter>`, so the shift is gone before
/// the editor ever sees it — `Ctrl+Shift+K` arrives as `0x0B`, byte-identical
/// to `Ctrl+K`. The whole family of `Ctrl+Shift` bindings is unreachable there,
/// which is why the macOS keymap avoids them.
///
/// `modifyOtherKeys` mode 2 is the request tmux does honour. With it on, tmux
/// reports every modified key unambiguously while leaving plain text, `Enter`
/// and `Esc` alone: `Ctrl+Shift+K` becomes `CSI 107;6u`, or `CSI 27;6;107~`
/// under `extended-keys-format xterm`. The input parser accepts both.
///
/// Detection is by environment rather than by probe: tmux always exports
/// `TMUX`, and only multiplexers need this (a real kitty-protocol terminal
/// honours the push directly), so a probe would buy nothing and would stall
/// startup on the terminals that never answer one — see the note in
/// [`TerminalModes::enable`].
///
/// Anything that pushes the keyboard enhancement flags must ask for this too,
/// or it will see unshifted keys where the editor sees shifted ones.
pub fn modify_other_keys_request() -> Option<&'static [u8]> {
    modify_other_keys_request_for(std::env::var("TMUX").ok().as_deref())
}

/// Pure half of [`modify_other_keys_request`], so the decision is testable.
fn modify_other_keys_request_for(tmux: Option<&str>) -> Option<&'static [u8]> {
    tmux.map(|_| sequences::PUSH_MODIFY_OTHER_KEYS_MODE_2)
}

/// The kitty keyboard-protocol push for `keyboard`, as raw bytes, or `None`
/// when the config asks for no flags at all.
///
/// The direct path sends this request through crossterm's
/// `PushKeyboardEnhancementFlags`; the client/server path composes its clients'
/// terminal setup as bytes, since the daemon writes them down a socket. This
/// mirrors that command's wire format so both paths ask for the same thing.
pub fn keyboard_enhancement_push(keyboard: &KeyboardConfig) -> Option<Vec<u8>> {
    keyboard
        .any_enabled()
        .then(|| format!("\x1b[>{}u", keyboard.to_flags().bits()).into_bytes())
}

/// Tracks which terminal modes have been enabled and provides cleanup.
///
/// Use `TerminalModes::enable()` to set up the terminal, then call `undo()`
/// to restore the original state (e.g., on exit or panic).
#[derive(Debug, Default)]
pub struct TerminalModes {
    raw_mode: bool,
    alternate_screen: bool,
    mouse_capture: bool,
    keyboard_enhancement: bool,
    modify_other_keys: bool,
    bracketed_paste: bool,
}

impl TerminalModes {
    /// Create a new TerminalModes with nothing enabled.
    pub fn new() -> Self {
        Self::default()
    }

    /// Enable all terminal modes, checking support for each.
    ///
    /// The `keyboard_config` parameter controls which keyboard enhancement flags
    /// to enable. Pass `None` to use defaults, or `Some(config)` for custom flags.
    ///
    /// Returns Ok(Self) with tracked state of what was enabled.
    /// On error, automatically undoes any partially enabled modes.
    pub fn enable(keyboard_config: Option<&KeyboardConfig>) -> Result<Self> {
        let mut modes = Self::new();
        let keyboard_config = keyboard_config.cloned().unwrap_or_default();

        // Enable raw mode
        if let Err(e) = enable_raw_mode() {
            tracing::error!("Failed to enable raw mode: {}", e);
            return Err(e.into());
        }
        modes.raw_mode = true;
        tracing::debug!("Enabled raw mode");

        // Enable alternate screen BEFORE keyboard enhancement.
        // This is critical: the Kitty keyboard protocol specifies that main and
        // alternate screens maintain independent keyboard mode stacks. If we push
        // keyboard enhancement before entering alternate screen, it goes to the
        // main screen's stack. Then when we pop before leaving (in undo), we pop
        // from the alternate screen's stack, leaving the main screen corrupted.
        // See: https://sw.kovidgoyal.net/kitty/keyboard-protocol/
        if let Err(e) = stdout().execute(EnterAlternateScreen) {
            tracing::error!("Failed to enter alternate screen: {}", e);
            modes.undo();
            return Err(e.into());
        }
        modes.alternate_screen = true;
        tracing::debug!("Entered alternate screen");

        // Push keyboard enhancement flags (if any are configured).
        //
        // Must happen AFTER entering alternate screen so the flags land on
        // the alternate screen's stack, not the main screen's.
        //
        // We push optimistically — no detection probe. The kitty keyboard
        // protocol [1] is explicit that push/pop CSIs are silently ignored
        // by terminals that don't implement it, so an unconditional push
        // is safe; we still pop in `undo()` to leave the user's terminal
        // exactly as we found it.
        //
        // The detection probe `crossterm::supports_keyboard_enhancement`
        // exists, but at version 0.29 it has a 2-second timeout that
        // fires on every terminal answering the universal `\x1B[c`
        // (primary device attributes) query but not the kitty-specific
        // `\x1B[?u` query — i.e., gnome-terminal, konsole, xterm, Apple
        // Terminal, screen, tmux without kitty passthrough, etc. That's
        // a 2 s hang on every startup for those users, with no upside
        // (the editor still works fine without the enhancement).
        //
        // [1] https://sw.kovidgoyal.net/kitty/keyboard-protocol/
        if keyboard_config.any_enabled() {
            let flags = keyboard_config.to_flags();
            if let Err(e) = stdout().execute(PushKeyboardEnhancementFlags(flags)) {
                tracing::info!("Failed to push keyboard enhancement flags: {}", e);
                // Non-fatal, continue without it
            } else {
                modes.keyboard_enhancement = true;
                tracing::debug!(
                    "Pushed keyboard enhancement flags optimistically: {:?}",
                    flags
                );
            }

            // The push above is silently ignored under tmux, so ask for the
            // one request it does honour. See `modify_other_keys_request`.
            if let Some(request) = modify_other_keys_request() {
                let mut out = stdout();
                if out.write_all(request).is_ok() && out.flush().is_ok() {
                    modes.modify_other_keys = true;
                    tracing::debug!("Requested xterm modifyOtherKeys mode 2 (multiplexer)");
                }
            }
        } else {
            tracing::debug!("Keyboard enhancement disabled by config");
        }

        // Enable mouse capture.
        // On Windows, skip crossterm's EnableMouseCapture — it replaces the
        // entire console mode with ENABLE_MOUSE_INPUT (removing VT input mode)
        // and doesn't write VT tracking sequences. Mouse is handled by
        // win_vt_input::enable_vt_input() + enable_mouse_tracking() instead.
        #[cfg(not(windows))]
        {
            if let Err(e) = stdout().execute(EnableMouseCapture) {
                tracing::warn!("Failed to enable mouse capture: {}", e);
                // Non-fatal, continue without it
            } else {
                modes.mouse_capture = true;
                tracing::debug!("Enabled mouse capture");
            }
        }
        #[cfg(windows)]
        {
            modes.mouse_capture = true;
            tracing::debug!(
                "Skipped crossterm EnableMouseCapture on Windows (handled by win_vt_input)"
            );
        }

        // Enable bracketed paste
        if let Err(e) = stdout().execute(EnableBracketedPaste) {
            tracing::warn!("Failed to enable bracketed paste: {}", e);
            // Non-fatal, continue without it
        } else {
            modes.bracketed_paste = true;
            tracing::debug!("Enabled bracketed paste mode");
        }

        Ok(modes)
    }

    /// Restore terminal to original state by disabling all enabled modes.
    ///
    /// This is safe to call multiple times - it tracks what was enabled
    /// and only disables those modes.
    #[allow(clippy::let_underscore_must_use)]
    pub fn undo(&mut self) {
        // Best-effort terminal teardown — if stdout is broken, we can't recover.
        // Disable mouse capture
        // On Windows, skip crossterm's DisableMouseCapture (same reason as enable).
        // Mouse cleanup is handled by win_vt_input::disable_mouse_tracking() +
        // restore_console_mode() in the event loop.
        if self.mouse_capture {
            #[cfg(not(windows))]
            let _ = stdout().execute(DisableMouseCapture);
            self.mouse_capture = false;
            tracing::debug!("Disabled mouse capture");
        }

        // Disable bracketed paste
        if self.bracketed_paste {
            let _ = stdout().execute(DisableBracketedPaste);
            self.bracketed_paste = false;
            tracing::debug!("Disabled bracketed paste");
        }

        // Reset cursor style to default. Not the cursor *colour*: the editor
        // never sets one, so there is nothing of ours to put back and a colour
        // the user's shell set is theirs to keep.
        let _ = stdout().execute(SetCursorStyle::DefaultUserShape);

        // Undo the multiplexer fallback, so the pane is back in its original
        // key mode by the time the shell resumes.
        if self.modify_other_keys {
            let _ = stdout().write_all(sequences::RESET_MODIFY_OTHER_KEYS);
            self.modify_other_keys = false;
            tracing::debug!("Reset xterm modifyOtherKeys");
        }

        // Pop keyboard enhancement flags
        if self.keyboard_enhancement {
            let _ = stdout().execute(PopKeyboardEnhancementFlags);
            self.keyboard_enhancement = false;
            tracing::debug!("Popped keyboard enhancement flags");
        }

        // Disable raw mode (before leaving alternate screen for cleaner output)
        if self.raw_mode {
            let _ = disable_raw_mode();
            self.raw_mode = false;
            tracing::debug!("Disabled raw mode");
        }

        // Leave alternate screen last
        if self.alternate_screen {
            let _ = stdout().execute(LeaveAlternateScreen);
            self.alternate_screen = false;
            tracing::debug!("Left alternate screen");
        }

        // Flush stdout to ensure all escape sequences are sent
        let _ = stdout().flush();
    }

    /// Returns true if mouse capture is enabled.
    pub fn mouse_capture_enabled(&self) -> bool {
        self.mouse_capture
    }
}

impl Drop for TerminalModes {
    fn drop(&mut self) {
        self.undo();
    }
}

/// Suspend the editor process with SIGTSTP and restore terminal modes on resume.
///
/// Tears the terminal back down to a normal cooked-mode shell, raises SIGTSTP
/// so the shell regains control (the user can then `fg` to resume), and on
/// resume re-enables the same set of modes we started with.
///
/// The caller is responsible for requesting a full redraw after this returns —
/// the screen has been wiped and repainted by the shell.
#[cfg(unix)]
pub fn suspend_and_resume(
    terminal_modes: &mut TerminalModes,
    keyboard_config: Option<&KeyboardConfig>,
) -> Result<()> {
    use nix::sys::signal::{raise, Signal};

    terminal_modes.undo();

    // Block until the shell sends SIGCONT (typically via `fg`).
    raise(Signal::SIGTSTP)?;

    // Re-enable everything we tore down. If enable() fails we drop the
    // old (empty) TerminalModes and return the error — the caller can
    // surface it and still keep running in a degraded state.
    let restored = TerminalModes::enable(keyboard_config)?;
    *terminal_modes = restored;
    Ok(())
}

/// Unconditionally restore terminal state without tracking.
///
/// This is intended for use in panic hooks where we don't have access
/// to the TerminalModes instance. It attempts to disable all modes
/// regardless of whether they were actually enabled.
#[allow(clippy::let_underscore_must_use)]
pub fn emergency_cleanup() {
    // Best-effort emergency terminal restore — if stdout is broken, we can't recover.
    // Disable mouse capture
    let _ = stdout().execute(DisableMouseCapture);

    // Disable bracketed paste
    let _ = stdout().execute(DisableBracketedPaste);

    // Reset cursor style to default (never the colour — see `restore`).
    let _ = stdout().execute(SetCursorStyle::DefaultUserShape);

    // Reset xterm modifyOtherKeys. Paired with `PushKeyboardEnhancementFlags`
    // below, which is likewise popped unconditionally.
    let _ = stdout().write_all(sequences::RESET_MODIFY_OTHER_KEYS);

    // Pop keyboard enhancement flags
    let _ = stdout().execute(PopKeyboardEnhancementFlags);

    // Disable raw mode
    let _ = disable_raw_mode();

    // Leave alternate screen
    let _ = stdout().execute(LeaveAlternateScreen);

    // Flush stdout
    let _ = stdout().flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modify_other_keys_request_is_xterm_mode_2_only_under_tmux() {
        // Wire format is pinned on purpose: tmux reads exactly this sequence
        // off the pane's output stream to switch key mode, so a typo here
        // would fail silently rather than loudly.
        assert_eq!(
            modify_other_keys_request_for(Some("/private/tmp/tmux-501/default,12345,0")),
            Some(&b"\x1b[>4;2m"[..])
        );
        assert_eq!(modify_other_keys_request_for(None), None);
        assert_eq!(sequences::RESET_MODIFY_OTHER_KEYS, b"\x1b[>4;0m");
    }

    #[test]
    fn keyboard_enhancement_push_matches_the_bytes_crossterm_would_send() {
        // Defaults are disambiguate + alternate keys, i.e. `CSI > 5 u` — the
        // request a direct launch puts on the wire.
        let push = keyboard_enhancement_push(&KeyboardConfig::default()).expect("defaults are on");
        assert_eq!(push, b"\x1b[>5u");
        assert_eq!(sequences::POP_KEYBOARD_ENHANCEMENT_FLAGS, b"\x1b[<1u");
    }

    #[test]
    fn keyboard_enhancement_push_is_skipped_when_every_flag_is_off() {
        let all_off = KeyboardConfig {
            disambiguate_escape_codes: false,
            report_event_types: false,
            report_alternate_keys: false,
            report_all_keys_as_escape_codes: false,
        };
        assert!(!all_off.any_enabled());
        assert_eq!(keyboard_enhancement_push(&all_off), None);
    }
}
