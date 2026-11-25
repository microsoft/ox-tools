#[cfg(test)]
compile_error!("registry twin was selected for compiler evidence");

/// This doctest must not run for the workspace package selection.
///
/// ```
/// compile_error!("registry twin was selected for doctest evidence");
/// ```
pub fn registry_twin() {}
