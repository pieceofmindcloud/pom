//! Infrastructure-free domain types shared by public crates.

/// Proves that the workspace exposes a library crate without coupling it to
/// the node binary. Concrete shared domain types land here incrementally.
pub struct WorkspaceMarker;

#[cfg(test)]
mod tests {
    #[test]
    fn workspace_marker_is_public() {
        let _ = crate::WorkspaceMarker;
    }
}
