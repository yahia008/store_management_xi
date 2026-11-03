//! Roles and the access rules built on them (FR-05, FR-06). Permissions come from the signed-in
//! user's memberships in the database; a store ID sent by the client is only ever a target to be
//! checked against them, never a source of authority.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    BusinessOwner,
    Administrator,
    StoreManager,
    Cashier,
    InventoryOfficer,
    FinanceOfficer,
}

impl Role {
    pub const ALL: [Role; 6] =
        [Role::BusinessOwner, Role::Administrator, Role::StoreManager, Role::Cashier, Role::InventoryOfficer, Role::FinanceOfficer];

    pub fn as_str(self) -> &'static str {
        match self {
            Role::BusinessOwner => "business_owner",
            Role::Administrator => "administrator",
            Role::StoreManager => "store_manager",
            Role::Cashier => "cashier",
            Role::InventoryOfficer => "inventory_officer",
            Role::FinanceOfficer => "finance_officer",
        }
    }

    pub fn parse(s: &str) -> Option<Role> {
        Role::ALL.into_iter().find(|r| r.as_str() == s)
    }

    /// Owner and administrator are business-wide; every other role belongs to one store.
    pub fn is_business_wide(self) -> bool {
        matches!(self, Role::BusinessOwner | Role::Administrator)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Grant {
    pub role: Role,
    pub store_id: Option<Uuid>,
}

#[derive(Clone, Debug)]
pub struct Principal {
    pub user_id: Uuid,
    pub business_id: Uuid,
    pub session_id: Uuid,
    pub grants: Vec<Grant>,
}

impl Principal {
    pub fn is_business_wide(&self) -> bool {
        self.grants.iter().any(|g| g.role.is_business_wide())
    }

    pub fn is_owner(&self) -> bool {
        self.grants.iter().any(|g| g.role == Role::BusinessOwner)
    }

    /// True if the user holds one of `roles` at `store_id`. Business-wide roles count everywhere.
    pub fn has_role_at(&self, store_id: Uuid, roles: &[Role]) -> bool {
        self.is_business_wide() || self.grants.iter().any(|g| g.store_id == Some(store_id) && roles.contains(&g.role))
    }

    /// True if the user holds any of `roles` at any store (business-wide roles count).
    pub fn has_any_role(&self, roles: &[Role]) -> bool {
        self.is_business_wide() || self.grants.iter().any(|g| roles.contains(&g.role))
    }

    /// Stores the user can see at all.
    pub fn visible_store_ids(&self) -> Option<Vec<Uuid>> {
        if self.is_business_wide() {
            None // every store in the business
        } else {
            Some(self.grants.iter().filter_map(|g| g.store_id).collect())
        }
    }
}

/// What each role can do, published at `GET /roles` so the screens and the SRS stay in step.
pub fn role_summary(role: Role) -> &'static [&'static str] {
    match role {
        Role::BusinessOwner => &["everything in the business", "grant and remove owner and administrator roles", "see all stores"],
        Role::Administrator => &["manage business settings, stores and employees", "see all stores", "cannot grant the owner role"],
        Role::StoreManager => &["manage their own store's settings", "see their own store's staff and data"],
        Role::Cashier => &["ring up sales at their own store"],
        Role::InventoryOfficer => &["manage stock at their own store"],
        Role::FinanceOfficer => &["view financial data of their own store"],
    }
}
