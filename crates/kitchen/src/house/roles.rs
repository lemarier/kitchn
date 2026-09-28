use crate::contracts::Role;

/// Load the generic role contract embedded in this Kitchen revision. These
/// contracts grant no authority and contain no house or domain specialization.
pub fn role_card(role: Role) -> &'static str {
    match role {
        Role::ChefOwner => include_str!("../../../../roles/chef-owner.md"),
        Role::SousChef => include_str!("../../../../roles/sous-chef.md"),
        Role::StationCook => include_str!("../../../../roles/station-cook.md"),
        Role::Commis => include_str!("../../../../roles/commis.md"),
        Role::Expediter => include_str!("../../../../roles/expediter.md"),
        Role::Gardener => include_str!("../../../../roles/gardener.md"),
        Role::Dishwasher => include_str!("../../../../roles/dishwasher.md"),
        Role::Inspector => include_str!("../../../../roles/inspector.md"),
    }
}
