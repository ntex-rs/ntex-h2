use ntex_bytes::ByteString;
use ntex_util::time::Seconds;

use crate::frame::WindowSize;

// Constants
pub(crate) const MAX_WINDOW_SIZE: WindowSize = (1 << 31) - 1;
pub(crate) const DEFAULT_RESET_STREAM_MAX: usize = 32;
pub(crate) const DEFAULT_RESET_STREAM_SECS: Seconds = Seconds(30);
pub(crate) const DEFAULT_CONNECTION_WINDOW_SIZE: WindowSize = 1_048_576;
pub(crate) const DEFAULT_SETTINGS_MAX_HEADER_LIST_SIZE: u32 = 48 * 1024;
pub(crate) const DEFAULT_MAX_COUNTINUATIONS: usize = 5;

pub(crate) const PREFACE: [u8; 24] = *b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

pub(crate) const HTTP_SCHEME: ByteString = ByteString::from_static("http");
pub(crate) const HTTPS_SCHEME: ByteString = ByteString::from_static("https");
