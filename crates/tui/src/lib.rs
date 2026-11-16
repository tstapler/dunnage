//! The explorer's terminal front end (ADR-005, Epic 6.5). It draws what `dunnage-core::explore` computes
//! and turns key events into messages; it has no filesystem, process or deletion code of its own, and
//! `scripts/check-tui-deps.sh` keeps it that way. Each module is declared here and filled by the story that
//! owns it, so no later story edits this file.

#![forbid(unsafe_code)]

pub mod app;
pub mod event_loop;
pub mod overlays;
pub mod terminal;
pub mod theme;
pub mod view;

/// The in-memory backend and buffer, so a test in another crate can drive [`event_loop::run_explorer`]
/// without depending on `ratatui` itself.
pub mod testing {
    use std::convert::Infallible;
    use std::sync::{Arc, Mutex};

    use ratatui::backend::{Backend, ClearType, WindowSize};
    use ratatui::buffer::Cell;
    use ratatui::layout::{Position, Size};

    pub use ratatui::backend::TestBackend;
    pub use ratatui::buffer::Buffer;

    /// A [`TestBackend`] that publishes the text of each flushed frame, so a key script can wait for a screen
    /// that a worker thread's answer produces instead of guessing the time it takes.
    pub struct FrameSpy {
        inner: TestBackend,
        frame: Arc<Mutex<String>>,
    }

    impl FrameSpy {
        pub fn new(width: u16, height: u16) -> FrameSpy {
            FrameSpy {
                inner: TestBackend::new(width, height),
                frame: Arc::new(Mutex::new(String::new())),
            }
        }

        /// The text of the last flushed frame (empty before the first); shared with every clone of the handle.
        pub fn frame(&self) -> Arc<Mutex<String>> {
            Arc::clone(&self.frame)
        }

        pub fn buffer(&self) -> &Buffer {
            self.inner.buffer()
        }
    }

    impl Backend for FrameSpy {
        type Error = Infallible;

        fn draw<'a, I>(&mut self, content: I) -> Result<(), Self::Error>
        where
            I: Iterator<Item = (u16, u16, &'a Cell)>,
        {
            self.inner.draw(content)
        }

        fn hide_cursor(&mut self) -> Result<(), Self::Error> {
            self.inner.hide_cursor()
        }

        fn show_cursor(&mut self) -> Result<(), Self::Error> {
            self.inner.show_cursor()
        }

        fn get_cursor_position(&mut self) -> Result<Position, Self::Error> {
            self.inner.get_cursor_position()
        }

        fn set_cursor_position<P: Into<Position>>(
            &mut self,
            position: P,
        ) -> Result<(), Self::Error> {
            self.inner.set_cursor_position(position)
        }

        fn clear(&mut self) -> Result<(), Self::Error> {
            self.inner.clear()
        }

        fn clear_region(&mut self, clear_type: ClearType) -> Result<(), Self::Error> {
            self.inner.clear_region(clear_type)
        }

        fn size(&self) -> Result<Size, Self::Error> {
            self.inner.size()
        }

        fn window_size(&mut self) -> Result<WindowSize, Self::Error> {
            self.inner.window_size()
        }

        fn flush(&mut self) -> Result<(), Self::Error> {
            let buf = self.inner.buffer();
            let a = buf.area;
            let text = (0..a.height)
                .map(|y| {
                    (0..a.width)
                        .map(|x| buf[(x, y)].symbol())
                        .collect::<String>()
                        .trim_end()
                        .to_string()
                })
                .collect::<Vec<_>>()
                .join("\n");
            *self
                .frame
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = text;
            self.inner.flush()
        }
    }
}
