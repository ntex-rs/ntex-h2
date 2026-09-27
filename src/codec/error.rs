/// Frame encoding error.
#[derive(Copy, Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum EncoderError {
    /// A `DATA` frame payload is larger than the peer's maximum frame size.
    #[error("Max size exceeded")]
    MaxSizeExceeded,
}
