// Read as a module by the crate and verbatim by build.rs (`include!`), which
// cannot `use` items from the crate it builds: the exe's FileDescription and
// the title reported over MCP must not drift apart.
// `pub` is required by the crate's re-export and inert in build.rs, where an
// include!'d item has no external consumer.

/// Human-readable product name, as distinct from the `donsetch` identifier.
pub const DISPLAY_NAME: &str = "DonSeTch";
