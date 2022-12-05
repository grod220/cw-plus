use cosmwasm_schema::cw_serde;
use cw_controllers::AdminExecuteUpdate;

#[cw_serde]
pub enum Cw4ExecuteMsg {
    /// Change the admin
    UpdateAdmin { update: AdminExecuteUpdate },
    /// Add a new hook to be informed of all membership changes. Must be called by Admin
    AddHook { addr: String },
    /// Remove a hook. Must be called by Admin
    RemoveHook { addr: String },
}
