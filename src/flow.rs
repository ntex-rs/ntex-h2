use std::{cmp, fmt, ops};

use crate::consts::MAX_WINDOW_SIZE;
use crate::frame::{Reason, WindowSize};

#[derive(Copy, Clone, Debug)]
pub struct FlowControl {
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
    window_size: Window,
}

impl FlowControl {
    pub fn new(sz: i32) -> FlowControl {
        FlowControl {
            window_size: Window(sz),
        }
    }

    /// Returns the window size as known by the peer
    pub fn window_size(&self) -> WindowSize {
        self.window_size.as_size()
    }

    /// If a WINDOW_UPDATE frame should be sent, returns a positive number
    /// representing the increment to be used.
    ///
    /// If there is no available bytes to be reclaimed, or the number of
    /// available bytes does not reach the threshold, this returns `None`.
    ///
    /// This represents pending outbound WINDOW_UPDATE frames.
    pub fn need_update_window(
        &self,
        max_size: WindowSize,
        threshold_size: WindowSize,
    ) -> Option<WindowSize> {
        if (self.window_size.0 as u32) >= max_size {
            return None;
        }

        let unclaimed = max_size - (self.window_size.0 as u32);
        if unclaimed < threshold_size {
            None
        } else {
            Some(unclaimed as WindowSize)
        }
    }

    /// Increase the window size.
    ///
    /// This is called after receiving a WINDOW_UPDATE frame
    pub fn inc_window(&mut self, sz: WindowSize) -> Result<(), Reason> {
        let (val, overflow) = self.window_size.0.overflowing_add(sz as i32);

        if overflow {
            return Err(Reason::FLOW_CONTROL_ERROR);
        }

        if val > MAX_WINDOW_SIZE as i32 {
            return Err(Reason::FLOW_CONTROL_ERROR);
        }

        log::trace!(
            "inc_window; sz={}; old={}; new={}",
            sz,
            self.window_size,
            val
        );

        self.window_size = Window(val);
        Ok(())
    }

    /// Decrement the window size.
    ///
    /// This is called after receiving a SETTINGS frame with a lower
    /// INITIAL_WINDOW_SIZE value.
    pub fn dec_window(&mut self, sz: WindowSize) {
        log::trace!("dec_window; sz={}; window={}", sz, self.window_size);
        self.window_size -= sz;
    }

    /// Decrements the window reflecting data has actually been sent. The caller
    /// must ensure that the window has capacity.
    pub fn send_data(&mut self, sz: WindowSize) {
        log::trace!("send_data; sz={}; window={}", sz, self.window_size);

        // Ensure that the argument is correct
        assert!(self.window_size >= sz as usize);

        // Update values
        self.window_size -= sz;
    }
}

/// The current capacity of a flow-controlled Window.
///
/// This number can go negative when either side has used a certain amount
/// of capacity when the other side advertises a reduction in size.
///
/// This type tries to centralize the knowledge of addition and subtraction
/// to this capacity, instead of having integer casts throughout the source.
#[derive(Clone, Copy, Debug, PartialEq, PartialOrd)]
pub struct Window(i32);

impl Window {
    pub fn as_size(&self) -> WindowSize {
        if self.0 < 0 {
            0
        } else {
            self.0 as WindowSize
        }
    }

    pub fn checked_size(&self) -> WindowSize {
        assert!(self.0 >= 0, "negative Window");
        self.0 as WindowSize
    }
}

impl PartialEq<usize> for Window {
    fn eq(&self, other: &usize) -> bool {
        if self.0 < 0 {
            false
        } else {
            (self.0 as usize).eq(other)
        }
    }
}

impl PartialOrd<usize> for Window {
    fn partial_cmp(&self, other: &usize) -> Option<cmp::Ordering> {
        if self.0 < 0 {
            Some(cmp::Ordering::Less)
        } else {
            (self.0 as usize).partial_cmp(other)
        }
    }
}

impl ops::SubAssign<WindowSize> for Window {
    fn sub_assign(&mut self, other: WindowSize) {
        self.0 -= other as i32;
    }
}

impl ops::Add<WindowSize> for Window {
    type Output = Self;
    fn add(self, other: WindowSize) -> Self::Output {
        Window(self.0 + other as i32)
    }
}

impl ops::AddAssign<WindowSize> for Window {
    fn add_assign(&mut self, other: WindowSize) {
        self.0 += other as i32;
    }
}

impl fmt::Display for Window {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl From<Window> for isize {
    fn from(w: Window) -> isize {
        w.0 as isize
    }
}
