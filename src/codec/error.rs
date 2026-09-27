#[derive(Copy, Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum EncoderError {
    #[error("Max size exceeded")]
    MaxSizeExceeded,
}
