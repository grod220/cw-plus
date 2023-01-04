use std::fmt::Debug;

use cosmwasm_schema::cw_serde;
use cosmwasm_std::{
    Addr, Api, CustomQuery, DepsMut, MessageInfo, Response, StdError, StdResult, Storage,
};
use cw_storage_plus::Item;
use schemars::JsonSchema;
use thiserror::Error;

/// Returned from Admin.query()
#[cw_serde]
pub struct SecureAdminResponse {
    pub admin: Option<String>,
    pub proposed: Option<String>,
}

/// Errors returned from Admin state transitions
#[derive(Error, Debug, PartialEq)]
pub enum SecureAdminError {
    #[error("{0}")]
    Std(#[from] StdError),

    #[error("Caller is not admin")]
    NotAdmin {},

    #[error("Caller is not the proposed admin")]
    NotProposedAdmin {},

    #[error("Admin state was already initialized")]
    AlreadyInitialized {},

    #[error("The admin role is abolished. No further updates possible.")]
    AdminRoleAbolished {},
}

type AdminResult<T> = Result<T, SecureAdminError>;

/// The finite states that are possible
#[cw_serde]
struct AdminState {
    abolished: bool,
    admin: Option<Addr>,
    proposed: Option<Addr>,
}

#[cw_serde]
pub enum SecureAdminUpdate {
    /// Proposes a new admin to take role. Only current admin can execute.
    ProposeNewAdmin { proposed: String },
    /// Clears the currently proposed admin. Only current admin can execute.
    ClearProposed,
    /// Promotes the proposed admin to be the current one. Only the proposed admin can execute.
    AcceptProposed,
    /// Throws away the keys to the Admin role forever. Once done, no admin can ever be set later.
    AbolishAdminRole,
}

#[cw_serde]
pub enum AdminInit {
    /// Sets the initial admin when none. No restrictions permissions to modify.
    SetInitialAdmin { admin: String },
    /// Throws away the keys to the Admin role forever. Once done, no admin can ever be set later.
    AbolishAdminRole,
}

/// A struct designed to help facilitate a two-step transition between contract admins safely.
/// It implements a finite state machine with dispatched events to manage state transitions.
/// State A: AdminUninitialized
///     - No restrictions on who can initialize the admin role
/// State B: AdminSetNoneProposed
///     - Once admin is set. Only they can execute the following updates:
///       - ProposeNewAdmin
///       - ClearProposed
/// State C: AdminSetWithProposed
///     - Only the proposed new admin can accept the new role via AcceptProposed {}
///     - The current admin can also clear the proposed new admin via ClearProposed {}
///
/// In every state, the admin (or on init, the initializer) can choose to abandon the role
/// and make the config immutable.
///
///```text
///                                                                  Clear Proposed
///                                                    +-------------------------------------^
///                                                    |                                     |
///                                                    v                                     |
/// +----------------+                      +----------------+                       +-------+--------+
/// | Admin: None    |   Initialize Admin   | Admin: Gabe    |   Propose New Admin   | Admin: Gabe    |
/// | Proposed: None +--------------------->| Proposed: None +---------------------->| Proposed: Joy  |
/// +-----+----------+                      ++---------------+                       +-------+----+---+
///       |                                  | Admin: Joy                                    |    |
///       |                                  | Proposed: None                                |    |
///   Abolish Role                           |      ^                                        |    |
///       |                *immutable        |      |              Accept Proposed           |    |
///       |            +----------------+    |      <----------------------------------------+    |
///       +----------->| Admin: None    |    |                                                    |
///                    | Proposed: None +----+------------------ Abolish Role --------------------+
///                    +----------------+
/// ```
pub struct SecureAdmin<'a>(Item<'a, AdminState>);

impl<'a> SecureAdmin<'a> {
    pub const fn new(namespace: &'a str) -> Self {
        Self(Item::new(namespace))
    }

    fn state(&self, storage: &'a dyn Storage) -> StdResult<AdminState> {
        Ok(self.0.may_load(storage)?.unwrap_or(AdminState {
            abolished: false,
            admin: None,
            proposed: None,
        }))
    }

    //--------------------------------------------------------------------------------------------------
    // Queries
    //--------------------------------------------------------------------------------------------------
    pub fn current(&self, storage: &'a dyn Storage) -> StdResult<Option<Addr>> {
        Ok(self.state(storage)?.admin)
    }

    pub fn is_admin(&self, storage: &'a dyn Storage, addr: &Addr) -> StdResult<bool> {
        match self.current(storage)? {
            Some(admin) if &admin == addr => Ok(true),
            _ => Ok(false),
        }
    }

    pub fn proposed(&self, storage: &'a dyn Storage) -> StdResult<Option<Addr>> {
        Ok(self.state(storage)?.proposed)
    }

    pub fn is_proposed(&self, storage: &'a dyn Storage, addr: &Addr) -> StdResult<bool> {
        match self.proposed(storage)? {
            Some(proposed) if &proposed == addr => Ok(true),
            _ => Ok(false),
        }
    }

    pub fn query(&self, storage: &'a dyn Storage) -> StdResult<SecureAdminResponse> {
        Ok(SecureAdminResponse {
            admin: self.current(storage)?.map(Into::into),
            proposed: self.proposed(storage)?.map(Into::into),
        })
    }

    //--------------------------------------------------------------------------------------------------
    // Mutations
    //--------------------------------------------------------------------------------------------------
    /// Execute inside instantiate fn
    pub fn initialize(
        &self,
        storage: &'a mut dyn Storage,
        api: &'a dyn Api,
        init_action: AdminInit,
    ) -> AdminResult<()> {
        let state = self.state(storage)?;

        if state.abolished {
            return Err(SecureAdminError::AdminRoleAbolished {});
        }

        if state.admin.is_some() {
            return Err(SecureAdminError::AlreadyInitialized {});
        }

        match init_action {
            AdminInit::SetInitialAdmin { admin } => {
                let validated = api.addr_validate(&admin)?;
                self.0.save(
                    storage,
                    &AdminState {
                        abolished: false,
                        admin: Some(validated),
                        proposed: None,
                    },
                )?;
            }
            AdminInit::AbolishAdminRole => {
                self.0.save(
                    storage,
                    &AdminState {
                        abolished: true,
                        admin: None,
                        proposed: None,
                    },
                )?;
            }
        }
        Ok(())
    }

    /// Composes execute responses for admin state updates
    pub fn update<C, Q: CustomQuery>(
        &self,
        deps: DepsMut<Q>,
        info: MessageInfo,
        update: SecureAdminUpdate,
    ) -> AdminResult<Response<C>>
    where
        C: Clone + Debug + PartialEq + JsonSchema,
    {
        let new_state = self.transition_state(deps.storage, deps.api, &info.sender, update)?;
        self.0.save(deps.storage, &new_state)?;

        let res = self.query(deps.storage)?;
        Ok(Response::new()
            .add_attribute("action", "update_admin")
            .add_attribute("admin", res.admin.unwrap_or_else(|| "None".to_string()))
            .add_attribute(
                "proposed",
                res.proposed.unwrap_or_else(|| "None".to_string()),
            )
            .add_attribute("sender", info.sender))
    }

    /// Executes admin state transitions
    fn transition_state(
        &self,
        storage: &'a mut dyn Storage,
        api: &'a dyn Api,
        sender: &Addr,
        event: SecureAdminUpdate,
    ) -> AdminResult<AdminState> {
        let mut state = self.state(storage)?;

        if state.abolished {
            return Err(SecureAdminError::AdminRoleAbolished {});
        }

        match event {
            SecureAdminUpdate::ProposeNewAdmin { proposed } => {
                self.assert_admin(storage, sender)?;
                let validated = api.addr_validate(&proposed)?;
                state.proposed = Some(validated);
            }
            SecureAdminUpdate::AbolishAdminRole => {
                self.assert_admin(storage, sender)?;
                state.abolished = true;
                state.proposed = None;
                state.admin = None;
            }
            SecureAdminUpdate::ClearProposed => {
                self.assert_admin(storage, sender)?;
                state.proposed = None;
            }
            SecureAdminUpdate::AcceptProposed => {
                self.assert_proposed(storage, sender)?;
                state.admin = Some(sender.clone());
                state.proposed = None;
            }
        }
        Ok(state)
    }

    //--------------------------------------------------------------------------------------------------
    // Assertions
    //--------------------------------------------------------------------------------------------------
    /// Similar to is_admin() except it raises an exception if caller is not current admin
    pub fn assert_admin(&self, storage: &'a dyn Storage, caller: &Addr) -> AdminResult<()> {
        if !self.is_admin(storage, caller)? {
            Err(SecureAdminError::NotAdmin {})
        } else {
            Ok(())
        }
    }

    /// Similar to is_proposed() except it raises an exception if caller is not currently proposed new admin
    pub fn assert_proposed(&self, storage: &'a dyn Storage, caller: &Addr) -> AdminResult<()> {
        if !self.is_proposed(storage, caller)? {
            Err(SecureAdminError::NotProposedAdmin {})
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use cosmwasm_std::testing::{mock_dependencies, mock_info};
    use cosmwasm_std::Empty;

    use crate::SecureAdminUpdate::{
        AbolishAdminRole, AcceptProposed, ClearProposed, ProposeNewAdmin,
    };

    use super::*;

    //--------------------------------------------------------------------------------------------------
    // Test invalid state transitions
    //--------------------------------------------------------------------------------------------------

    #[test]
    fn invalid_uninitialized_state_transitions() {
        let mut deps = mock_dependencies();
        let sender = Addr::unchecked("peter_parker");
        let info = mock_info(sender.as_ref(), &[]);
        let admin = SecureAdmin::new("xyz");

        let err = admin
            .update::<Empty, Empty>(
                deps.as_mut(),
                info.clone(),
                ProposeNewAdmin {
                    proposed: "abc".to_string(),
                },
            )
            .unwrap_err();
        assert_eq!(err, SecureAdminError::NotAdmin {});

        let err = admin
            .update::<Empty, Empty>(deps.as_mut(), info.clone(), ClearProposed)
            .unwrap_err();
        assert_eq!(err, SecureAdminError::NotAdmin {});

        let err = admin
            .update::<Empty, Empty>(deps.as_mut(), info.clone(), AcceptProposed)
            .unwrap_err();
        assert_eq!(err, SecureAdminError::NotProposedAdmin {});

        let err = admin
            .update::<Empty, Empty>(deps.as_mut(), info, AbolishAdminRole)
            .unwrap_err();
        assert_eq!(err, SecureAdminError::NotAdmin {});
    }

    #[test]
    fn invalid_admin_set_no_proposed_state_transitions() {
        let mut deps = mock_dependencies();
        let sender = Addr::unchecked("peter_parker");
        let info = mock_info(sender.as_ref(), &[]);
        let admin = SecureAdmin::new("xyz");

        let mut_deps = deps.as_mut();

        admin
            .initialize(
                mut_deps.storage,
                mut_deps.api,
                AdminInit::SetInitialAdmin {
                    admin: sender.to_string(),
                },
            )
            .unwrap();

        let err = admin
            .initialize(
                mut_deps.storage,
                mut_deps.api,
                AdminInit::SetInitialAdmin {
                    admin: "abc".to_string(),
                },
            )
            .unwrap_err();
        assert_eq!(err, SecureAdminError::AlreadyInitialized {});

        let err = admin
            .update::<Empty, Empty>(deps.as_mut(), info, AcceptProposed)
            .unwrap_err();
        assert_eq!(err, SecureAdminError::NotProposedAdmin {});
    }

    #[test]
    fn invalid_admin_set_with_proposed_state_transitions() {
        let mut deps = mock_dependencies();
        let sender = Addr::unchecked("peter_parker");
        let info = mock_info(sender.as_ref(), &[]);
        let admin = SecureAdmin::new("xyz");

        let mut_deps = deps.as_mut();

        admin
            .initialize(
                mut_deps.storage,
                mut_deps.api,
                AdminInit::SetInitialAdmin {
                    admin: sender.to_string(),
                },
            )
            .unwrap();

        admin
            .update::<Empty, Empty>(
                mut_deps,
                info.clone(),
                ProposeNewAdmin {
                    proposed: "abc".to_string(),
                },
            )
            .unwrap();

        let mut_deps = deps.as_mut();

        let err = admin
            .initialize(
                mut_deps.storage,
                mut_deps.api,
                AdminInit::SetInitialAdmin {
                    admin: "abc".to_string(),
                },
            )
            .unwrap_err();
        assert_eq!(err, SecureAdminError::AlreadyInitialized {});
    }

    #[test]
    fn invalid_admin_role_abolished_state_transitions() {
        let mut deps = mock_dependencies();
        let sender = Addr::unchecked("peter_parker");
        let info = mock_info(sender.as_ref(), &[]);
        let admin = SecureAdmin::new("xyz");

        let mut_deps = deps.as_mut();

        admin
            .initialize(mut_deps.storage, mut_deps.api, AdminInit::AbolishAdminRole)
            .unwrap();

        let err = admin
            .initialize(
                mut_deps.storage,
                mut_deps.api,
                AdminInit::SetInitialAdmin {
                    admin: "abc".to_string(),
                },
            )
            .unwrap_err();
        assert_eq!(err, SecureAdminError::AdminRoleAbolished {});

        let err = admin
            .update::<Empty, Empty>(
                deps.as_mut(),
                info.clone(),
                ProposeNewAdmin {
                    proposed: "efg".to_string(),
                },
            )
            .unwrap_err();
        assert_eq!(err, SecureAdminError::AdminRoleAbolished {});

        let err = admin
            .update::<Empty, Empty>(deps.as_mut(), info.clone(), ClearProposed)
            .unwrap_err();
        assert_eq!(err, SecureAdminError::AdminRoleAbolished {});

        let err = admin
            .update::<Empty, Empty>(deps.as_mut(), info.clone(), AcceptProposed)
            .unwrap_err();
        assert_eq!(err, SecureAdminError::AdminRoleAbolished {});

        let err = admin
            .update::<Empty, Empty>(deps.as_mut(), info, AbolishAdminRole)
            .unwrap_err();
        assert_eq!(err, SecureAdminError::AdminRoleAbolished {});
    }

    //--------------------------------------------------------------------------------------------------
    // Test permissions
    //--------------------------------------------------------------------------------------------------

    #[test]
    fn initialize_admin_permissions() {
        let mut deps = mock_dependencies();
        let mut_deps = deps.as_mut();
        let admin = SecureAdmin::new("xyz");

        // Anyone can initialize
        admin
            .initialize(mut_deps.storage, mut_deps.api, AdminInit::AbolishAdminRole)
            .unwrap();

        let mut deps = mock_dependencies();
        let mut_deps = deps.as_mut();

        admin
            .initialize(
                mut_deps.storage,
                mut_deps.api,
                AdminInit::SetInitialAdmin {
                    admin: "xyz".to_string(),
                },
            )
            .unwrap();
    }

    #[test]
    fn propose_new_admin_permissions() {
        let mut deps = mock_dependencies();
        let sender = Addr::unchecked("peter_parker");
        let admin = SecureAdmin::new("xyz");

        let mut_deps = deps.as_mut();
        admin
            .initialize(
                mut_deps.storage,
                mut_deps.api,
                AdminInit::SetInitialAdmin {
                    admin: sender.to_string(),
                },
            )
            .unwrap();

        let bad_guy = Addr::unchecked("doc_oc");
        let info = mock_info(bad_guy.as_ref(), &[]);
        let err = admin
            .update::<Empty, Empty>(
                mut_deps,
                info,
                ProposeNewAdmin {
                    proposed: bad_guy.to_string(),
                },
            )
            .unwrap_err();

        assert_eq!(err, SecureAdminError::NotAdmin {})
    }

    #[test]
    fn clear_proposed_permissions() {
        let mut deps = mock_dependencies();
        let sender = Addr::unchecked("peter_parker");
        let info = mock_info(sender.as_ref(), &[]);
        let admin = SecureAdmin::new("xyz");

        let mut_deps = deps.as_mut();
        admin
            .initialize(
                mut_deps.storage,
                mut_deps.api,
                AdminInit::SetInitialAdmin {
                    admin: sender.to_string(),
                },
            )
            .unwrap();
        admin
            .update::<Empty, Empty>(
                mut_deps,
                info,
                ProposeNewAdmin {
                    proposed: "miles_morales".to_string(),
                },
            )
            .unwrap();

        let bad_guy = Addr::unchecked("doc_oc");
        let info = mock_info(bad_guy.as_ref(), &[]);
        let err = admin
            .update::<Empty, Empty>(deps.as_mut(), info, ClearProposed)
            .unwrap_err();

        assert_eq!(err, SecureAdminError::NotAdmin {})
    }

    #[test]
    fn accept_proposed_permissions() {
        let mut deps = mock_dependencies();
        let sender = Addr::unchecked("peter_parker");
        let info = mock_info(sender.as_ref(), &[]);
        let admin = SecureAdmin::new("xyz");

        let mut_deps = deps.as_mut();
        admin
            .initialize(
                mut_deps.storage,
                mut_deps.api,
                AdminInit::SetInitialAdmin {
                    admin: sender.to_string(),
                },
            )
            .unwrap();
        admin
            .update::<Empty, Empty>(
                mut_deps,
                info,
                ProposeNewAdmin {
                    proposed: "miles_morales".to_string(),
                },
            )
            .unwrap();

        let bad_guy = Addr::unchecked("doc_oc");
        let info = mock_info(bad_guy.as_ref(), &[]);
        let err = admin
            .update::<Empty, Empty>(deps.as_mut(), info, AcceptProposed)
            .unwrap_err();

        assert_eq!(err, SecureAdminError::NotProposedAdmin {})
    }

    #[test]
    fn abolish_admin_role_permissions() {
        let mut deps = mock_dependencies();
        let sender = Addr::unchecked("peter_parker");
        let admin = SecureAdmin::new("xyz");

        let mut_deps = deps.as_mut();
        admin
            .initialize(
                mut_deps.storage,
                mut_deps.api,
                AdminInit::SetInitialAdmin {
                    admin: sender.to_string(),
                },
            )
            .unwrap();

        let bad_guy = Addr::unchecked("doc_oc");
        let info = mock_info(bad_guy.as_ref(), &[]);
        let err = admin
            .update::<Empty, Empty>(deps.as_mut(), info, AbolishAdminRole)
            .unwrap_err();

        assert_eq!(err, SecureAdminError::NotAdmin {})
    }

    //--------------------------------------------------------------------------------------------------
    // Test success cases
    //--------------------------------------------------------------------------------------------------

    fn assert_uninitialized(storage: &dyn Storage, admin: &SecureAdmin) {
        let state = admin.state(storage).unwrap();

        assert_eq!(state.abolished, false);
        assert_eq!(state.admin, None);
        assert_eq!(state.proposed, None);

        let current = admin.current(storage).unwrap();
        assert_eq!(current, None);

        let proposed = admin.proposed(storage).unwrap();
        assert_eq!(proposed, None);

        let res = admin.query(storage).unwrap();
        assert_eq!(
            res,
            SecureAdminResponse {
                admin: None,
                proposed: None
            }
        );
    }

    #[test]
    fn uninitialized_state() {
        let deps = mock_dependencies();
        let admin = SecureAdmin::new("xyz");
        assert_uninitialized(deps.as_ref().storage, &admin);
    }

    #[test]
    fn initialize_admin() {
        let mut deps = mock_dependencies();
        let original_admin = Addr::unchecked("peter_parker");
        let admin = SecureAdmin::new("xyz");

        let mut_deps = deps.as_mut();
        admin
            .initialize(
                mut_deps.storage,
                mut_deps.api,
                AdminInit::SetInitialAdmin {
                    admin: original_admin.to_string(),
                },
            )
            .unwrap();

        let state = admin.state(mut_deps.storage).unwrap();
        assert!(state.admin.is_some());
        assert!(state.proposed.is_none());
        assert_eq!(state.abolished, false);

        let current = admin.current(mut_deps.storage).unwrap();
        assert_eq!(current, Some(original_admin.clone()));
        assert!(admin.is_admin(mut_deps.storage, &original_admin).unwrap());

        let proposed = admin.proposed(mut_deps.storage).unwrap();
        assert_eq!(proposed, None);

        let res = admin.query(mut_deps.storage).unwrap();
        assert_eq!(
            res,
            SecureAdminResponse {
                admin: Some(original_admin.to_string()),
                proposed: None
            }
        );
    }

    #[test]
    fn propose_new_admin() {
        let mut deps = mock_dependencies();
        let original_admin = Addr::unchecked("peter_parker");
        let proposed_admin = Addr::unchecked("miles_morales");
        let info = mock_info(original_admin.as_ref(), &[]);
        let admin = SecureAdmin::new("xyz");

        let mut_deps = deps.as_mut();
        admin
            .initialize(
                mut_deps.storage,
                mut_deps.api,
                AdminInit::SetInitialAdmin {
                    admin: original_admin.to_string(),
                },
            )
            .unwrap();

        admin
            .update::<Empty, Empty>(
                mut_deps,
                info,
                ProposeNewAdmin {
                    proposed: "miles_morales".to_string(),
                },
            )
            .unwrap();

        let storage = deps.as_mut().storage;

        let state = admin.state(storage).unwrap();
        assert!(state.admin.is_some());
        assert!(state.proposed.is_some());
        assert_eq!(state.abolished, false);

        let current = admin.current(storage).unwrap();
        assert_eq!(current, Some(original_admin.clone()));
        assert!(admin.is_admin(storage, &original_admin).unwrap());

        let proposed = admin.proposed(storage).unwrap();
        assert_eq!(proposed, Some(proposed_admin.clone()));
        assert!(admin.is_proposed(storage, &proposed_admin).unwrap());

        let res = admin.query(storage).unwrap();
        assert_eq!(
            res,
            SecureAdminResponse {
                admin: Some(original_admin.to_string()),
                proposed: Some(proposed_admin.to_string())
            }
        );
    }

    #[test]
    fn clear_proposed() {
        let mut deps = mock_dependencies();
        let original_admin = Addr::unchecked("peter_parker");
        let proposed_admin = Addr::unchecked("miles_morales");
        let info = mock_info(original_admin.as_ref(), &[]);
        let admin = SecureAdmin::new("xyz");

        let mut_deps = deps.as_mut();
        admin
            .initialize(
                mut_deps.storage,
                mut_deps.api,
                AdminInit::SetInitialAdmin {
                    admin: original_admin.to_string(),
                },
            )
            .unwrap();

        let mut_deps = deps.as_mut();
        admin
            .update::<Empty, Empty>(
                mut_deps,
                info.clone(),
                ProposeNewAdmin {
                    proposed: "miles_morales".to_string(),
                },
            )
            .unwrap();

        let mut_deps = deps.as_mut();
        admin
            .update::<Empty, Empty>(mut_deps, info, ClearProposed)
            .unwrap();

        let storage = deps.as_mut().storage;

        let state = admin.state(storage).unwrap();
        assert!(state.admin.is_some());
        assert!(state.proposed.is_none());
        assert_eq!(state.abolished, false);

        let current = admin.current(storage).unwrap();
        assert_eq!(current, Some(original_admin.clone()));
        assert!(admin.is_admin(storage, &original_admin).unwrap());

        let proposed = admin.proposed(storage).unwrap();
        assert_eq!(proposed, None);
        assert!(!admin.is_proposed(storage, &proposed_admin).unwrap());

        let res = admin.query(storage).unwrap();
        assert_eq!(
            res,
            SecureAdminResponse {
                admin: Some(original_admin.to_string()),
                proposed: None
            }
        );
    }

    #[test]
    fn accept_proposed() {
        let mut deps = mock_dependencies();
        let original_admin = Addr::unchecked("peter_parker");
        let proposed_admin = Addr::unchecked("miles_morales");
        let info = mock_info(original_admin.as_ref(), &[]);
        let admin = SecureAdmin::new("xyz");

        let mut_deps = deps.as_mut();
        admin
            .initialize(
                mut_deps.storage,
                mut_deps.api,
                AdminInit::SetInitialAdmin {
                    admin: original_admin.to_string(),
                },
            )
            .unwrap();

        let mut_deps = deps.as_mut();
        admin
            .update::<Empty, Empty>(
                mut_deps,
                info,
                ProposeNewAdmin {
                    proposed: "miles_morales".to_string(),
                },
            )
            .unwrap();

        let info = mock_info(proposed_admin.as_ref(), &[]);
        let mut_deps = deps.as_mut();
        admin
            .update::<Empty, Empty>(mut_deps, info, AcceptProposed)
            .unwrap();

        let storage = deps.as_mut().storage;

        let state = admin.state(storage).unwrap();
        assert!(state.admin.is_some());
        assert!(state.proposed.is_none());
        assert_eq!(state.abolished, false);

        let current = admin.current(storage).unwrap();
        assert_eq!(current, Some(proposed_admin.clone()));
        assert!(admin.is_admin(storage, &proposed_admin).unwrap());

        let proposed = admin.proposed(storage).unwrap();
        assert_eq!(proposed, None);
        assert!(!admin.is_proposed(storage, &proposed_admin).unwrap());

        let res = admin.query(storage).unwrap();
        assert_eq!(
            res,
            SecureAdminResponse {
                admin: Some(proposed_admin.to_string()),
                proposed: None
            }
        );
    }

    #[test]
    fn abolish_admin_role() {
        let mut deps = mock_dependencies();
        let original_admin = Addr::unchecked("peter_parker");
        let info = mock_info(original_admin.as_ref(), &[]);
        let admin = SecureAdmin::new("xyz");

        let mut_deps = deps.as_mut();
        admin
            .initialize(
                mut_deps.storage,
                mut_deps.api,
                AdminInit::SetInitialAdmin {
                    admin: original_admin.to_string(),
                },
            )
            .unwrap();

        let mut_deps = deps.as_mut();
        admin
            .update::<Empty, Empty>(mut_deps, info, AbolishAdminRole)
            .unwrap();

        let storage = deps.as_mut().storage;

        let state = admin.state(storage).unwrap();
        assert!(state.admin.is_none());
        assert!(state.proposed.is_none());
        assert_eq!(state.abolished, true);

        let current = admin.current(storage).unwrap();
        assert_eq!(current, None);
        assert!(!admin.is_admin(storage, &original_admin).unwrap());

        let proposed = admin.proposed(storage).unwrap();
        assert_eq!(proposed, None);
        assert!(!admin.is_proposed(storage, &original_admin).unwrap());

        let res = admin.query(storage).unwrap();
        assert_eq!(
            res,
            SecureAdminResponse {
                admin: None,
                proposed: None
            }
        );
    }
}
