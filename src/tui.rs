//! Terminal setup and teardown. The terminal is restored when [`TerminalGuard`]
//! drops and also from the panic hook, so a panic message lands on the normal
//! screen in cooked mode.

use std::io::{self, Stdout, stdout};
use std::sync::atomic::{AtomicBool, Ordering};

use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
    supports_keyboard_enhancement,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

static ACTIVE: AtomicBool = AtomicBool::new(false);
static KEYBOARD_ENHANCED: AtomicBool = AtomicBool::new(false);

pub type Tui = Terminal<CrosstermBackend<Stdout>>;

pub struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore();
    }
}

pub fn init() -> io::Result<(Tui, TerminalGuard)> {
    install_panic_hook();
    enable_raw_mode()?;
    ACTIVE.store(true, Ordering::SeqCst);
    let guard = TerminalGuard;

    execute!(stdout(), EnterAlternateScreen, EnableBracketedPaste)?;
    // Lets terminals that support the kitty protocol report Shift+Enter distinctly.
    if supports_keyboard_enhancement().unwrap_or(false) {
        execute!(
            stdout(),
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        )?;
        KEYBOARD_ENHANCED.store(true, Ordering::SeqCst);
    }

    let terminal = Terminal::new(CrosstermBackend::new(stdout()))?;
    Ok((terminal, guard))
}

/// Idempotent; errors are ignored because there is nowhere left to report them.
pub fn restore() {
    if !ACTIVE.swap(false, Ordering::SeqCst) {
        return;
    }
    let mut out = stdout();
    if KEYBOARD_ENHANCED.swap(false, Ordering::SeqCst) {
        let _ = execute!(out, PopKeyboardEnhancementFlags);
    }
    let _ = execute!(
        out,
        DisableBracketedPaste,
        LeaveAlternateScreen,
        crossterm::cursor::Show
    );
    let _ = disable_raw_mode();
}

fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore();
        previous(info);
    }));
}
