use serde::Deserialize;
use serde::Serialize;

/// Arguments accepted by legacy apply-patch handlers and shell interception paths.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApplyPatchToolArgs {
    pub input: String,
}
