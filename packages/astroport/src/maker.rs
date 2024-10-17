use crate::asset::{Asset, AssetInfo};
use crate::factory::UpdateAddr;
use cosmwasm_schema::{cw_serde, QueryResponses};
use cosmwasm_std::{Addr, Decimal, Uint128, Uint64};
use std::ops::RangeInclusive;

/// Validations limits for cooldown period. From 30 to 600 seconds.
pub const COOLDOWN_LIMITS: RangeInclusive<u64> = 30..=600;

/// This structure stores the main parameters for the Maker contract.
#[cw_serde]
pub struct Config {
    /// Address that's allowed to set contract parameters
    pub owner: Addr,
    /// The factory contract address
    pub factory_contract: Addr,
    /// The Asteroid bridge contract
    pub asteroid_contract: Addr,
    /// Default bridge asset (Terra1 - LUNC, Terra2 - LUNA, etc.)
    pub default_bridge: Option<AssetInfo>,
    /// The ROIDS token asset info
    pub roids_token: AssetInfo,
    /// The max spread allowed when swapping fee tokens to ASTRO
    pub max_spread: Decimal,
    /// If set defines the period when maker collect can be called
    pub collect_cooldown: Option<u64>,
}

/// This structure stores general parameters for the contract.
#[cw_serde]
pub struct InstantiateMsg {
    /// Address that's allowed to change contract parameters
    pub owner: String,
    /// Default bridge asset (Terra1 - LUNC, Terra2 - LUNA, etc.)
    pub default_bridge: Option<AssetInfo>,
    /// The ROIDS token asset info
    pub roids_token: AssetInfo,
    /// The factory contract address
    pub factory_contract: String,
    /// The Asteroid bridge contract
    pub asteroid_contract: String,
    /// The maximum spread used when swapping fee tokens to ASTRO
    pub max_spread: Option<Decimal>,
    /// If set defines the period when maker collect can be called
    pub collect_cooldown: Option<u64>,
}

/// This structure describes the functions that can be executed in this contract.
#[cw_serde]
pub enum ExecuteMsg {
    /// Collects and swaps fee tokens to ASTRO
    Collect {
        /// The assets to swap to ASTRO
        assets: Vec<AssetWithLimit>,
    },
    /// Updates general settings
    UpdateConfig {
        /// The factory contract address
        factory_contract: Option<String>,
        /// Basic chain asset (Terra1 - LUNC, Terra2 - LUNA, etc.)
        basic_asset: Option<AssetInfo>,
        /// The maximum spread used when swapping fee tokens to ASTRO
        max_spread: Option<Decimal>,
        /// Defines the period when maker collect can be called
        collect_cooldown: Option<u64>,
        /// The ROIDS token asset info
        roids_token: Option<AssetInfo>,
        /// The Asteroid bridge contract
        asteroid_contract: Option<String>,
    },
    /// Add bridge tokens used to swap specific fee tokens to ASTRO (effectively declaring a swap route)
    UpdateBridges {
        add: Option<Vec<(AssetInfo, AssetInfo)>>,
        remove: Option<Vec<AssetInfo>>,
    },
    /// Swap fee tokens via bridge assets
    SwapBridgeAssets { assets: Vec<AssetInfo>, depth: u64 },
    /// Distribute ASTRO to stakers and to governance
    DistributeAstro {},
    /// Creates a request to change the contract's ownership
    ProposeNewOwner {
        /// The newly proposed owner
        owner: String,
        /// The validity period of the proposal to change the owner
        expires_in: u64,
    },
    /// Removes a request to change contract ownership
    DropOwnershipProposal {},
    /// Claims contract ownership
    ClaimOwnership {},
}

/// This structure describes the query functions available in the contract.
#[cw_serde]
#[derive(QueryResponses)]
pub enum QueryMsg {
    /// Returns information about the maker configs that contains in the [`ConfigResponse`]
    #[returns(ConfigResponse)]
    Config {},
    /// Returns the balance for each asset in the specified input parameters
    #[returns(BalancesResponse)]
    Balances { assets: Vec<AssetInfo> },
    #[returns(Vec<(String, String)>)]
    Bridges {},
}

/// A custom struct that holds contract parameters and is used to retrieve them.
#[cw_serde]
pub struct ConfigResponse {
    /// Address that is allowed to update contract parameters
    pub owner: Addr,
    /// Default bridge (Terra1 - LUNC, Terra2 - LUNA, etc.)
    pub default_bridge: Option<AssetInfo>,
    /// The ROIDS token asset info
    pub roids_token: AssetInfo,
    /// The factory contract address
    pub factory_contract: Addr,
    /// The Asteroid bridge contract
    pub asteroid_contract: Addr,
    /// The maximum spread used when swapping fee tokens to ROIDS
    pub max_spread: Decimal,
}

/// A custom struct used to return multiple asset balances.
#[cw_serde]
pub struct BalancesResponse {
    pub balances: Vec<Asset>,
}

/// This structure describes a migration message.
#[cw_serde]
pub struct MigrateMsg {
    
}

/// This struct holds parameters to help with swapping a specific amount of a fee token to ASTRO.
#[cw_serde]
pub struct AssetWithLimit {
    /// Information about the fee token to swap
    pub info: AssetInfo,
    /// The amount of tokens to swap
    pub limit: Option<Uint128>,
}
