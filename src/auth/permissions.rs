//! Local authorization model for the bank-SMS endpoints.
//!
//! FalconGo issues the tokens and its claim contract is fixed. It carries no
//! roles array and no scopes -- authorization is a single integer ladder in the
//! `permission` claim. This module is the ONE place that translates FalconGo's
//! vocabulary into local permissions, so a future change to FalconGo's claims is
//! a change to one file.
//!
//! Handlers receive an `AuthContext` and never touch `Claims` directly. That is
//! the whole point of the split: the raw token shape stops leaking past this
//! boundary.

use crate::auth::claims::Claims;

/// FalconGo permission ladder, as used by the existing apex-rust routes and by
/// the legacy dashboard.
///
/// 1 viewer · 2 editor · 3 manager (financial access) · 4 admin
pub mod level {
    pub const VIEWER: i32 = 1;
    pub const EDITOR: i32 = 2;
    /// Financial visibility. apex-rust already treats `< 3` as "mask money".
    pub const MANAGER: i32 = 3;
    pub const ADMIN: i32 = 4;
}

/// What a caller is allowed to do, resolved once in middleware.
#[derive(Debug, Clone)]
pub struct AuthContext {
    /// FalconGo's `user_id` claim. Note this is NOT `sub` -- FalconGo emits no
    /// `sub` at all, and `iss` is the same id rendered as a string.
    pub user_id: i64,
    pub permission: i32,
}

impl AuthContext {
    /// Build from verified claims. Returns None for anything that is not an
    /// admin user: driver tokens must never reach these endpoints.
    pub fn from_claims(claims: &Claims) -> Option<Self> {
        if !claims.is_admin() {
            return None;
        }
        let user_id = claims.user_id? as i64;
        if user_id == 0 {
            return None;
        }
        Some(Self {
            user_id,
            permission: claims.permission.unwrap_or(0),
        })
    }

    pub fn has_at_least(&self, required: i32) -> bool {
        self.permission >= required
    }

    /// Read the transaction ledger.
    ///
    /// ADMIN, not MANAGER. The whole fleet-expenses module is level 4 only: it
    /// exposes every bank message, counterparty and account balance the company
    /// receives, which is a strictly wider disclosure than the financial figures
    /// a manager sees elsewhere. Read and write are the same level deliberately
    /// -- there is no useful "can look but not touch" tier here, and splitting
    /// them would imply a distinction the module does not actually enforce.
    pub fn can_read(&self) -> bool {
        self.has_at_least(level::ADMIN)
    }

    /// Create/edit/delete transactions, notes and tags.
    pub fn can_write(&self) -> bool {
        self.has_at_least(level::ADMIN)
    }

    /// Manage the template registry, trigger reparses, view ingest internals.
    pub fn can_admin(&self) -> bool {
        self.has_at_least(level::ADMIN)
    }

    /// Value recorded in `created_by` / `author` / `actor` columns.
    pub fn actor(&self) -> String {
        self.user_id.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claims(user_type: &str, user_id: Option<i32>, permission: Option<i32>) -> Claims {
        Claims {
            user_type: user_type.to_string(),
            user_id,
            driver_id: None,
            permission,
            exp: 9_999_999_999,
        }
    }

    #[test]
    fn driver_tokens_are_rejected() {
        // Driver tokens are structurally valid and correctly signed; they simply
        // have no business on these endpoints.
        let c = claims("driver", Some(0), None);
        assert!(AuthContext::from_claims(&c).is_none());
    }

    #[test]
    fn admin_with_zero_user_id_is_rejected() {
        let c = claims("admin_user", Some(0), Some(4));
        assert!(AuthContext::from_claims(&c).is_none());
    }

    #[test]
    fn missing_permission_claim_defaults_to_no_access() {
        // FalconGo always emits `permission`, but a token that somehow lacks it
        // must fail closed rather than inherit admin.
        let c = claims("admin_user", Some(7), None);
        let ctx = AuthContext::from_claims(&c).unwrap();
        assert_eq!(ctx.permission, 0);
        assert!(!ctx.can_read());
        assert!(!ctx.can_write());
        assert!(!ctx.can_admin());
    }

    /// The module is level 4 only. If this ever relaxes, it should be a
    /// deliberate decision with this test changed to match, not a drift.
    #[test]
    fn every_capability_requires_admin() {
        for level in 0..=3 {
            let ctx = AuthContext::from_claims(&claims("admin_user", Some(9), Some(level)))
                .expect("admin_user with a real id resolves");
            assert!(!ctx.can_read(), "permission {level} must not read");
            assert!(!ctx.can_write(), "permission {level} must not write");
            assert!(!ctx.can_admin(), "permission {level} must not administer");
        }
        let admin = AuthContext::from_claims(&claims("admin_user", Some(9), Some(4))).unwrap();
        assert!(admin.can_read() && admin.can_write() && admin.can_admin());
    }

    #[test]
    fn permission_ladder() {
        let viewer = AuthContext::from_claims(&claims("admin_user", Some(1), Some(1))).unwrap();
        assert!(!viewer.can_read());
        assert!(!viewer.can_admin());

        // A manager is deliberately locked out: this module is ADMIN-only.
        let manager = AuthContext::from_claims(&claims("admin_user", Some(2), Some(3))).unwrap();
        assert!(!manager.can_read());
        assert!(!manager.can_write());
        assert!(!manager.can_admin());

        let admin = AuthContext::from_claims(&claims("admin_user", Some(3), Some(4))).unwrap();
        assert!(admin.can_read());
        assert!(admin.can_write());
        assert!(admin.can_admin());
    }

    #[test]
    fn actor_is_the_user_id() {
        let ctx = AuthContext::from_claims(&claims("admin_user", Some(42), Some(4))).unwrap();
        assert_eq!(ctx.actor(), "42");
        assert_eq!(ctx.user_id, 42);
    }
}
