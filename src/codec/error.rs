use super::length_delimited::LengthDelimitedCodecError;
use crate::frame;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum EncoderError {
    MaxSizeExceeded,
}

impl From<LengthDelimitedCodecError> for frame::Error {
    fn from(_: LengthDelimitedCodecError) -> Self {
        frame::Error::MaxFrameSize
    }
}
