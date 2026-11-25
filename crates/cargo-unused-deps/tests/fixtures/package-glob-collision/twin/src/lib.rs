/// Uses a dependency that ordinary compilation never loads.
///
/// ```
/// doc_only::used_from_doctest();
/// ```
pub fn value() -> u32 {
    registry_twin::registry_twin();
    library_used::value()
}
