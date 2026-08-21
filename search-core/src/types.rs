pub type Generation = u64;
pub type LogicalDocId = u64;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DocumentInput {
    pub key: String,
    pub display_path: String,
    pub normalized_name: Vec<u8>,
    pub normalized_content: Vec<u8>,
}

impl DocumentInput {
    #[must_use]
    pub fn new(
        key: impl Into<String>,
        display_path: impl Into<String>,
        normalized_name: impl Into<Vec<u8>>,
        normalized_content: impl Into<Vec<u8>>,
    ) -> Self {
        Self {
            key: key.into(),
            display_path: display_path.into(),
            normalized_name: normalized_name.into(),
            normalized_content: normalized_content.into(),
        }
    }
}
