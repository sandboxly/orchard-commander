//! The system clipboard (arboard), behind a trait so tests use memory.

pub trait Clipboard {
    fn get_text(&mut self) -> Option<String>;
    fn set_text(&mut self, text: &str);
}

/// A clipboard in memory (tests, or when the system one is unavailable).
#[derive(Default, Debug)]
pub struct MemoryClipboard(pub Option<String>);

impl Clipboard for MemoryClipboard {
    fn get_text(&mut self) -> Option<String> {
        self.0.clone()
    }

    fn set_text(&mut self, text: &str) {
        self.0 = Some(text.to_owned());
    }
}

/// The OS clipboard; opened lazily on first use (opening it can take a few
/// milliseconds, which shouldn't delay the first frame).
#[derive(Default)]
pub struct SystemClipboard {
    inner: Option<arboard::Clipboard>,
    failed: bool,
    fallback: MemoryClipboard,
}

impl SystemClipboard {
    fn get(&mut self) -> Option<&mut arboard::Clipboard> {
        if self.inner.is_none() && !self.failed {
            match arboard::Clipboard::new() {
                Ok(c) => self.inner = Some(c),
                Err(e) => {
                    log::warn!("no system clipboard: {e}");
                    self.failed = true;
                }
            }
        }
        self.inner.as_mut()
    }
}

impl Clipboard for SystemClipboard {
    fn get_text(&mut self) -> Option<String> {
        match self.get() {
            Some(c) => c.get_text().ok(),
            None => self.fallback.get_text(),
        }
    }

    fn set_text(&mut self, text: &str) {
        match self.get() {
            Some(c) => {
                let _ = c.set_text(text.to_owned());
            }
            None => self.fallback.set_text(text),
        }
    }
}
