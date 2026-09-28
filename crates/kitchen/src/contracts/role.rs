//! Kitchen station roles. A role names responsibilities; it never grants authority.

use crate::contracts::ValueKind;

closed_names! {
    /// A Kitchen role. Authority comes only from explicit grants, never from the role.
    #[non_exhaustive]
    pub enum Role(ValueKind::Role) {
        /// Owns house policy and final decisions.
        ChefOwner = "chef-owner",
        /// Coordinates pickup, supervision, and repair.
        SousChef = "sous-chef",
        /// Implements one task in an isolated workspace.
        StationCook = "station-cook",
        /// Performs bounded preparation work for a station.
        Commis = "commis",
        /// Evaluates exact-head evidence at the pass (merge gate).
        Expediter = "expediter",
        /// Maintains issue hygiene and specifications.
        Gardener = "gardener",
        /// Inspects and reclaims resources with positive ownership evidence.
        Dishwasher = "dishwasher",
        /// Inspects delivered work against concrete questions.
        Inspector = "inspector",
    }
}
