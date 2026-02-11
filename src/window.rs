#![allow(clippy::cast_possible_wrap, clippy::cast_sign_loss)]
use crate::frame::WindowSize;

#[derive(Copy, Clone, Debug)]
pub(super) struct Window {
    /// Window the peer knows about.
    ///
    /// This can go negative if a `SETTINGS_INITIAL_WINDOW_SIZE` is received.
    ///
    /// For example, say the peer sends a request and uses 32kb of the window.
    /// We send a `SETTINGS_INITIAL_WINDOW_SIZE` of 16kb. The peer has to adjust
    /// its understanding of the capacity of the window, and that would be:
    ///
    /// ```notrust
    /// default (64kb) - used (32kb) - settings_diff (64kb - 16kb): -16kb
    /// ```
    pub(crate) window_size: i32,
}

impl Window {
    pub(super) const fn new(sz: i32) -> Window {
        Window { window_size: sz }
    }

    #[allow(clippy::cast_sign_loss)]
    /// Returns the window size as known by the peer
    pub(super) const fn window_size(self) -> WindowSize {
        if self.window_size < 0 {
            0
        } else {
            self.window_size as WindowSize
        }
    }

    /// If a `WINDOW_UPDATE` frame should be sent, returns a positive number
    /// representing the increment to be used.
    ///
    /// If there is no available bytes to be reclaimed, or the number of
    /// available bytes does not reach the threshold, this returns `None`.
    ///
    /// This represents pending outbound `WINDOW_UPDATE` frames.
    pub(super) fn update(
        &mut self,
        cap: WindowSize,
        max_size: i32,
        threshold_size: WindowSize,
    ) -> Option<WindowSize> {
        if self.window_size >= max_size {
            return None;
        }

        let available = max_size - self.window_size - (cap as i32);
        if available < threshold_size.cast_signed() {
            None
        } else {
            self.window_size += available;
            Some(available as WindowSize)
        }
    }

    /// Increase the window size.
    ///
    /// This is called after receiving a `WINDOW_UPDATE` frame
    pub(super) fn inc(self, sz: i32) -> Result<Self, ()> {
        let (val, overflow) = self.window_size.overflowing_add(sz);

        if overflow {
            return Err(());
        }

        log::trace!(
            "inc_window; sz={}; old={}; new={}",
            sz,
            self.window_size,
            val
        );

        Ok(Self::new(val))
    }

    /// Decrement the window size.
    ///
    /// This is called after receiving a `SETTINGS` frame with a lower
    /// `INITIAL_WINDOW_SIZE` value.
    pub(super) fn dec(self, sz: WindowSize) -> Self {
        log::trace!("dec_window; sz={}; window={}", sz, self.window_size);
        Self::new(self.window_size - sz.cast_signed())
    }
}
