use crate::{consts::MAX_WINDOW_SIZE, frame::WindowSize};

#[derive(Copy, Clone, Debug)]
pub struct Window {
    /// Window the peer knows about.
    ///
    /// This can go negative if a SETTINGS_INITIAL_WINDOW_SIZE is received.
    ///
    /// For example, say the peer sends a request and uses 32kb of the window.
    /// We send a SETTINGS_INITIAL_WINDOW_SIZE of 16kb. The peer has to adjust
    /// its understanding of the capacity of the window, and that would be:
    ///
    /// ```notrust
    /// default (64kb) - used (32kb) - settings_diff (64kb - 16kb): -16kb
    /// ```
    pub(crate) window_size: i32,
}

impl Window {
    pub const fn new(sz: i32) -> Window {
        Window { window_size: sz }
    }

    /// Returns the window size as known by the peer
    pub const fn window_size(&self) -> WindowSize {
        if self.window_size < 0 {
            0
        } else {
            self.window_size as WindowSize
        }
    }

    /// If a WINDOW_UPDATE frame should be sent, returns a positive number
    /// representing the increment to be used.
    ///
    /// If there is no available bytes to be reclaimed, or the number of
    /// available bytes does not reach the threshold, this returns `None`.
    ///
    /// This represents pending outbound WINDOW_UPDATE frames.
    pub fn update(
        &mut self,
        cap: WindowSize,
        max_size: WindowSize,
        threshold_size: WindowSize,
    ) -> Option<WindowSize> {
        if self.window_size >= (max_size as i32) {
            return None;
        }

        let available = (max_size as i32) - self.window_size - (cap as i32);
        if available < (threshold_size as i32) {
            None
        } else {
            self.window_size = self.window_size + available;
            Some(available as WindowSize)
        }
    }

    /// Increase the window size.
    ///
    /// This is called after receiving a WINDOW_UPDATE frame
    pub fn inc(self, sz: WindowSize) -> Result<Self, ()> {
        let (val, overflow) = self.window_size.overflowing_add(sz as i32);

        if overflow {
            return Err(());
        }

        if val > MAX_WINDOW_SIZE as i32 {
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
    /// This is called after receiving a SETTINGS frame with a lower
    /// INITIAL_WINDOW_SIZE value.
    pub fn dec(self, sz: WindowSize) -> Self {
        log::trace!("dec_window; sz={}; window={}", sz, self.window_size);
        Self::new(self.window_size - (sz as i32))
    }
}
