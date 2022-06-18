use super::length_delimited::LengthDelimitedCodecError;
use crate::frame;

#[derive(Copy, Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum EncoderError {
    #[error("Max size exceeded")]
    MaxSizeExceeded,
}

impl From<LengthDelimitedCodecError> for frame::FrameError {
    fn from(_: LengthDelimitedCodecError) -> Self {
        frame::FrameError::MaxFrameSize
    }
}
