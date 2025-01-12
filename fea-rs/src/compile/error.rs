//! Error types related to compilation

use std::sync::Arc;

use write_fonts::{read::ReadError, validate::ValidationReport};

use crate::{
    parse::{SourceList, SourceLoadError},
    Diagnostic,
};

/// An error that occurs when extracting a glyph order from a UFO.
#[derive(Clone, Debug, thiserror::Error)]
pub enum UfoGlyphOrderError {
    /// Missing 'public.glyphOrder' key
    #[error("No public.glyphOrder key in lib.plist")]
    KeyNotSet,
    /// Glyph order is present, but malformed
    #[error("public.glyphOrder exists, but is not an array of strings")]
    Malformed,
}

/// An error that occurs when extracting a glyph order from a font file.
#[derive(Clone, Debug, thiserror::Error)]
pub enum FontGlyphOrderError {
    /// Failed to read font data
    #[error("Failed to read font data: '{0}'")]
    ReadError(
        #[from]
        #[source]
        ReadError,
    ),
    /// Post table is missing glyph names
    #[error("The post table exists, but did not include all glyph names")]
    MissingNames,
}

/// An error that occurs when loading a raw glyph order.
#[derive(Clone, Debug, thiserror::Error)]
pub enum GlyphOrderError {
    /// Invalid name
    #[error("Invalid name '{name}' in glyph order")]
    #[allow(missing_docs)]
    NameError { name: String },
    /// Missing .notdef glyph
    #[error("The first glyph must be '.notdef'")]
    MissingNotDef,
}

/// An error reported by the compiler
#[derive(Debug, thiserror::Error)]
#[allow(missing_docs)]
pub enum CompilerError {
    #[error("{0}")]
    SourceLoad(
        #[from]
        #[source]
        SourceLoadError,
    ),
    #[error("Parsing failed with {} errors\n{0}", .0.messages.len())]
    ParseFail(DiagnosticSet),
    #[error("Validation failed with {} errors\n{0}", .0.messages.len())]
    ValidationFail(DiagnosticSet),
    #[error("Compilation failed with {} errors\n{0}", .0.messages.len())]
    CompilationFail(DiagnosticSet),
    #[error("Binary generation failed: '{0}'")]
    WriteFail(#[from] BinaryCompilationError),
}

/// An error that occured when generating the binary font
#[derive(Debug, thiserror::Error)]
#[error("Binary generation failed: '{0}'")]
pub struct BinaryCompilationError(ValidationReport);

/// A set of diagnostics with the associated source info
#[derive(Clone)]
pub struct DiagnosticSet {
    pub(crate) messages: Vec<Diagnostic>,
    pub(crate) sources: Arc<SourceList>,
}

impl std::fmt::Display for DiagnosticSet {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        let mut first = true;
        for err in &self.messages {
            if !first {
                writeln!(f)?;
            }
            write!(f, "{}", self.sources.format_diagnostic(err))?;
            first = false;
        }
        Ok(())
    }
}

impl std::fmt::Debug for DiagnosticSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiagnosticSet")
            .field("messages", &self.messages)
            .field("tree", &"ParseTree")
            .finish()
    }
}

impl From<ValidationReport> for BinaryCompilationError {
    fn from(src: ValidationReport) -> BinaryCompilationError {
        BinaryCompilationError(src)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assert_compiler_error_is_send() {
        fn send_me_baby<T: Send>() {}
        send_me_baby::<CompilerError>();
    }
}
