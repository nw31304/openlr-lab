#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub struct NodeId(pub u32);

#[derive(Debug, Clone)]
pub struct NetworkNode {
    pub id: NodeId,
    pub lon: f64,
    pub lat: f64,
    /// Opaque stable identifier supplied by the tile provider.
    /// Used for cross-tile node stitching; meaning is provider-defined.
    pub stable_id: String,
    pub is_boundary: bool,
}
