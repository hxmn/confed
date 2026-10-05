use thiserror::Error;

pub type ConvertResult<T> = std::result::Result<T, ConvertError>;

#[derive(Debug, Error)]
pub enum ConvertError {
    #[error("could not parse storage format: {0}")]
    Parse(String),

    /// A ```` ```confluence ```` fence no longer contains well-formed XML. The push
    /// is refused rather than uploading a corrupted macro.
    #[error("preserved block at line {line} is not well-formed XML: {detail}")]
    InvalidPreservedBlock { line: usize, detail: String },

    #[error("could not generate storage format: {0}")]
    Generate(String),

    /// The stored block map does not line up with the base Markdown, so
    /// block-level patching is unsafe; the caller should fall back to a full
    /// regeneration and warn.
    #[error("block map is stale: {0}")]
    StaleBlockMap(String),
}
