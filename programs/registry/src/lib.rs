// OCCA Registry Program — v3.
//
// Owns three account types:
//   • CompanyAccount    — a tenant on the platform.
//   • AgentIdentity     — agent identity, independent of any single
//                         company. Same identity may have multiple
//                         deployments (e.g. retired then re-deployed)
//                         within the same owner's companies.
//   • Deployment        — relation account binding an AgentIdentity to a
//                         CompanyAccount. Identity stays stable across
//                         deployments; the deployment is per-company.
//
// Ownership immutability: by design, neither CompanyAccount.owner nor
// AgentIdentity.owner can be changed once written. Companies and agents
// are NOT transferable assets. There is no "transfer", "sell", or
// "rotate authority" instruction. Loss of the owner wallet means the
// company/agent is permanently inaccessible — users must back up their
// wallet seed. A future recovery mechanism (if ever introduced) must
// be designed to be clearly distinguishable from commercial transfer.
//
// Naming compliance: the term "Deployment" (NOT "Employment") is used
// per Whitepaper §15.7 — OCCA avoids labor-coded terminology. See
// `occa/CLAUDE.md` "Regulatory naming guardrails".
//
// Authority model: every state-changing ix is signed by the user wallet
// (`owner`). The operator hot wallet sponsors fees only — it never
// appears as an authority on any account.
//
// Truth model: this program is the source of truth for identity,
// ownership, and deployment status. Off-chain DBs are caches that must
// be re-buildable from chain alone. See `occa/CLAUDE.md`
// "Chain = truth, DB = cache".

use anchor_lang::prelude::*;

declare_id!("occaTHMv5eYG5aZ85jimxTvHkBfsDCvndXC6J2k8kxr");

// ─── Bounds ────────────────────────────────────────────────────────────────
pub const MAX_NAME_LEN: usize = 64;
pub const MAX_LOCALE_LEN: usize = 8;
pub const MAX_ROLE_LEN: usize = 32;
pub const MAX_METADATA_URI_LEN: usize = 200;
pub const MAX_REPUTATION_URI_LEN: usize = 200;

// ─── Account schema versions (bump on field changes) ───────────────────────
pub const COMPANY_ACCOUNT_VERSION: u8 = 3;
pub const AGENT_IDENTITY_ACCOUNT_VERSION: u8 = 1;
pub const DEPLOYMENT_ACCOUNT_VERSION: u8 = 1;

// ─── Status encodings ──────────────────────────────────────────────────────
// CompanyAccount.status
pub const COMPANY_STATUS_ACTIVE: u8 = 0;
pub const COMPANY_STATUS_PAUSED: u8 = 1;

// Deployment.status
pub const DEPLOYMENT_STATUS_ACTIVE: u8 = 0;
pub const DEPLOYMENT_STATUS_PAUSED: u8 = 1;
pub const DEPLOYMENT_STATUS_RETIRED: u8 = 2;

#[program]
pub mod registry {
    use super::*;

    // ───────────────────────────── Company ─────────────────────────────────

    /// Create a new CompanyAccount PDA.
    ///
    /// Seeds: `["company", owner, nonce_le_u32]`
    pub fn create_company(
        ctx: Context<CreateCompany>,
        nonce: u32,
        name: String,
        locale: String,
        metadata_uri: String,
        metadata_hash: [u8; 32],
    ) -> Result<()> {
        require!(!name.is_empty(), RegistryError::NameRequired);
        require!(name.len() <= MAX_NAME_LEN, RegistryError::NameTooLong);
        require!(locale.len() <= MAX_LOCALE_LEN, RegistryError::LocaleTooLong);
        require!(
            metadata_uri.len() <= MAX_METADATA_URI_LEN,
            RegistryError::MetadataUriTooLong
        );

        let now = Clock::get()?.unix_timestamp;
        let company = &mut ctx.accounts.company;
        company.version = COMPANY_ACCOUNT_VERSION;
        company.owner = ctx.accounts.owner.key();
        // Treasury / Policy programs are deployed in a later phase. We
        // pin Pubkey::default() here so clients can detect "not yet
        // wired" via `treasury == Pubkey::default()`.
        company.treasury = Pubkey::default();
        company.policy = Pubkey::default();
        company.created_at = now;
        company.updated_at = now;
        company.nonce = nonce;
        company.status = COMPANY_STATUS_ACTIVE;
        company.name = name;
        company.locale = locale;
        company.metadata_uri = metadata_uri;
        company.metadata_hash = metadata_hash;
        Ok(())
    }

    /// Update company metadata (name / locale / off-chain pointer).
    /// Owner-only. Use to rename, update brand assets, etc.
    pub fn update_company_metadata(
        ctx: Context<UpdateCompanyMetadata>,
        name: String,
        locale: String,
        metadata_uri: String,
        metadata_hash: [u8; 32],
    ) -> Result<()> {
        require!(!name.is_empty(), RegistryError::NameRequired);
        require!(name.len() <= MAX_NAME_LEN, RegistryError::NameTooLong);
        require!(locale.len() <= MAX_LOCALE_LEN, RegistryError::LocaleTooLong);
        require!(
            metadata_uri.len() <= MAX_METADATA_URI_LEN,
            RegistryError::MetadataUriTooLong
        );

        let company = &mut ctx.accounts.company;
        company.name = name;
        company.locale = locale;
        company.metadata_uri = metadata_uri;
        company.metadata_hash = metadata_hash;
        company.updated_at = Clock::get()?.unix_timestamp;
        Ok(())
    }

    /// Pause / resume a company. Paused companies should be skipped by
    /// dispatchers off-chain. Owner-only.
    pub fn update_company_status(
        ctx: Context<UpdateCompanyStatus>,
        new_status: u8,
    ) -> Result<()> {
        require!(
            new_status == COMPANY_STATUS_ACTIVE || new_status == COMPANY_STATUS_PAUSED,
            RegistryError::InvalidStatus
        );
        let company = &mut ctx.accounts.company;
        company.status = new_status;
        company.updated_at = Clock::get()?.unix_timestamp;
        Ok(())
    }

    // ────────────────────────── Agent Identity ─────────────────────────────

    /// Mint a new AgentIdentity PDA.
    ///
    /// Seeds: `["agent_identity", agent_pubkey]`
    ///
    /// `agent_pubkey` is a caller-supplied identifier (typically derived
    /// from a generated keypair held by the user wallet). It is the
    /// stable identity of the agent across deployments. The recorded
    /// `owner` is immutable — there is no transfer instruction.
    pub fn register_agent_identity(
        ctx: Context<RegisterAgentIdentity>,
        agent_pubkey: Pubkey,
        name: String,
        metadata_uri: String,
        metadata_hash: [u8; 32],
    ) -> Result<()> {
        require!(!name.is_empty(), RegistryError::NameRequired);
        require!(name.len() <= MAX_NAME_LEN, RegistryError::NameTooLong);
        require!(
            metadata_uri.len() <= MAX_METADATA_URI_LEN,
            RegistryError::MetadataUriTooLong
        );

        let now = Clock::get()?.unix_timestamp;
        let identity = &mut ctx.accounts.identity;
        identity.version = AGENT_IDENTITY_ACCOUNT_VERSION;
        identity.agent_pubkey = agent_pubkey;
        identity.owner = ctx.accounts.owner.key();
        identity.created_at = now;
        identity.updated_at = now;
        identity.name = name;
        identity.metadata_uri = metadata_uri;
        identity.metadata_hash = metadata_hash;
        // Reputation pointer reserved for Phase 2.
        identity.reputation_uri = String::new();
        Ok(())
    }

    /// Update identity metadata (rename, change persona, etc.).
    /// Identity-owner only.
    pub fn update_agent_identity_metadata(
        ctx: Context<UpdateAgentIdentityMetadata>,
        name: String,
        metadata_uri: String,
        metadata_hash: [u8; 32],
    ) -> Result<()> {
        require!(!name.is_empty(), RegistryError::NameRequired);
        require!(name.len() <= MAX_NAME_LEN, RegistryError::NameTooLong);
        require!(
            metadata_uri.len() <= MAX_METADATA_URI_LEN,
            RegistryError::MetadataUriTooLong
        );

        let identity = &mut ctx.accounts.identity;
        identity.name = name;
        identity.metadata_uri = metadata_uri;
        identity.metadata_hash = metadata_hash;
        identity.updated_at = Clock::get()?.unix_timestamp;
        Ok(())
    }

    // ──────────────────────────── Deployment ───────────────────────────────

    /// Create a Deployment binding an existing AgentIdentity to a Company.
    ///
    /// Seeds: `["deployment", company_pda, deployment_index_le_u32]`
    ///
    /// `deployment_index` is a per-company u32 counter — caller picks
    /// the next free index. Same identity may be deployed multiple times
    /// in the same company (e.g. retired then re-deployed) — each gets
    /// its own index.
    ///
    /// Constraint: `identity.owner == company.owner`. Both fields are
    /// immutable, so this binding is permanent.
    pub fn create_deployment(
        ctx: Context<CreateDeployment>,
        deployment_index: u32,
        role: String,
        parent_deployment_index: Option<u32>,
        adapter_id: Pubkey,
        metadata_uri: String,
        metadata_hash: [u8; 32],
    ) -> Result<()> {
        require!(!role.is_empty(), RegistryError::RoleRequired);
        require!(role.len() <= MAX_ROLE_LEN, RegistryError::RoleTooLong);
        require!(
            metadata_uri.len() <= MAX_METADATA_URI_LEN,
            RegistryError::MetadataUriTooLong
        );

        let company = &ctx.accounts.company;
        let identity = &ctx.accounts.identity;
        require!(
            identity.owner == company.owner,
            RegistryError::IdentityOwnerMismatch
        );

        let now = Clock::get()?.unix_timestamp;
        let deployment = &mut ctx.accounts.deployment;
        deployment.version = DEPLOYMENT_ACCOUNT_VERSION;
        deployment.agent_identity = identity.key();
        deployment.company = company.key();
        deployment.deployment_index = deployment_index;
        deployment.owner = company.owner;
        deployment.operating_wallet = Pubkey::default();
        deployment.adapter_id = adapter_id;
        deployment.role = role;
        deployment.parent_deployment_index = parent_deployment_index;
        deployment.status = DEPLOYMENT_STATUS_ACTIVE;
        deployment.deployed_at = now;
        deployment.retired_at = 0;
        deployment.updated_at = now;
        deployment.metadata_uri = metadata_uri;
        deployment.metadata_hash = metadata_hash;
        Ok(())
    }

    /// Update deployment metadata (role, off-chain pointer).
    /// Owner-only.
    pub fn update_deployment_metadata(
        ctx: Context<UpdateDeploymentMetadata>,
        role: String,
        metadata_uri: String,
        metadata_hash: [u8; 32],
    ) -> Result<()> {
        require!(!role.is_empty(), RegistryError::RoleRequired);
        require!(role.len() <= MAX_ROLE_LEN, RegistryError::RoleTooLong);
        require!(
            metadata_uri.len() <= MAX_METADATA_URI_LEN,
            RegistryError::MetadataUriTooLong
        );

        let deployment = &mut ctx.accounts.deployment;
        require!(
            deployment.status != DEPLOYMENT_STATUS_RETIRED,
            RegistryError::DeploymentRetired
        );
        deployment.role = role;
        deployment.metadata_uri = metadata_uri;
        deployment.metadata_hash = metadata_hash;
        deployment.updated_at = Clock::get()?.unix_timestamp;
        Ok(())
    }

    /// Toggle deployment between Active and Paused. Owner-only.
    /// Use `retire_deployment` for terminal transition.
    pub fn update_deployment_status(
        ctx: Context<UpdateDeploymentStatus>,
        new_status: u8,
    ) -> Result<()> {
        require!(
            new_status == DEPLOYMENT_STATUS_ACTIVE || new_status == DEPLOYMENT_STATUS_PAUSED,
            RegistryError::InvalidStatus
        );
        let deployment = &mut ctx.accounts.deployment;
        require!(
            deployment.status != DEPLOYMENT_STATUS_RETIRED,
            RegistryError::DeploymentRetired
        );
        deployment.status = new_status;
        deployment.updated_at = Clock::get()?.unix_timestamp;
        Ok(())
    }

    /// Retire a deployment. Terminal — once retired, this Deployment
    /// PDA is read-only forever (preserves history). To re-engage the
    /// same identity in the same company, create a new deployment with
    /// the next deployment_index.
    pub fn retire_deployment(ctx: Context<RetireDeployment>) -> Result<()> {
        let deployment = &mut ctx.accounts.deployment;
        require!(
            deployment.status != DEPLOYMENT_STATUS_RETIRED,
            RegistryError::DeploymentRetired
        );
        let now = Clock::get()?.unix_timestamp;
        deployment.status = DEPLOYMENT_STATUS_RETIRED;
        deployment.retired_at = now;
        deployment.updated_at = now;
        Ok(())
    }

    /// Set or replace the deployment's operating wallet. Pass
    /// `Pubkey::default()` to clear. Owner-only.
    pub fn set_operating_wallet(
        ctx: Context<SetOperatingWallet>,
        new_operating_wallet: Pubkey,
    ) -> Result<()> {
        let deployment = &mut ctx.accounts.deployment;
        require!(
            deployment.status != DEPLOYMENT_STATUS_RETIRED,
            RegistryError::DeploymentRetired
        );
        deployment.operating_wallet = new_operating_wallet;
        deployment.updated_at = Clock::get()?.unix_timestamp;
        Ok(())
    }
}

// ─── Account contexts ──────────────────────────────────────────────────────

#[derive(Accounts)]
#[instruction(nonce: u32)]
pub struct CreateCompany<'info> {
    #[account(
        init,
        payer = payer,
        space = 8 + CompanyAccount::INIT_SPACE,
        seeds = [b"company", owner.key().as_ref(), &nonce.to_le_bytes()],
        bump,
    )]
    pub company: Account<'info, CompanyAccount>,

    /// Owning user wallet — signer to prevent PDA squatting.
    pub owner: Signer<'info>,

    /// Pays rent. Typically the operator hot wallet (sponsored UX).
    #[account(mut)]
    pub payer: Signer<'info>,

    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct UpdateCompanyMetadata<'info> {
    #[account(
        mut,
        has_one = owner @ RegistryError::Unauthorized,
    )]
    pub company: Account<'info, CompanyAccount>,
    pub owner: Signer<'info>,
}

#[derive(Accounts)]
pub struct UpdateCompanyStatus<'info> {
    #[account(
        mut,
        has_one = owner @ RegistryError::Unauthorized,
    )]
    pub company: Account<'info, CompanyAccount>,
    pub owner: Signer<'info>,
}

#[derive(Accounts)]
#[instruction(agent_pubkey: Pubkey)]
pub struct RegisterAgentIdentity<'info> {
    #[account(
        init,
        payer = payer,
        space = 8 + AgentIdentity::INIT_SPACE,
        seeds = [b"agent_identity", agent_pubkey.as_ref()],
        bump,
    )]
    pub identity: Account<'info, AgentIdentity>,

    /// Identity owner (= user wallet that minted this identity).
    pub owner: Signer<'info>,

    #[account(mut)]
    pub payer: Signer<'info>,

    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct UpdateAgentIdentityMetadata<'info> {
    #[account(
        mut,
        has_one = owner @ RegistryError::Unauthorized,
    )]
    pub identity: Account<'info, AgentIdentity>,
    pub owner: Signer<'info>,
}

#[derive(Accounts)]
#[instruction(deployment_index: u32)]
pub struct CreateDeployment<'info> {
    #[account(
        has_one = owner @ RegistryError::Unauthorized,
    )]
    pub company: Account<'info, CompanyAccount>,

    /// AgentIdentity to deploy. Phase 1: must be owned by the same
    /// wallet as the company (enforced in handler).
    pub identity: Account<'info, AgentIdentity>,

    /// Company owner (signer). Authority for state changes.
    pub owner: Signer<'info>,

    #[account(
        init,
        payer = payer,
        space = 8 + Deployment::INIT_SPACE,
        seeds = [b"deployment", company.key().as_ref(), &deployment_index.to_le_bytes()],
        bump,
    )]
    pub deployment: Account<'info, Deployment>,

    #[account(mut)]
    pub payer: Signer<'info>,

    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct UpdateDeploymentMetadata<'info> {
    #[account(
        mut,
        has_one = owner @ RegistryError::Unauthorized,
    )]
    pub deployment: Account<'info, Deployment>,
    pub owner: Signer<'info>,
}

#[derive(Accounts)]
pub struct UpdateDeploymentStatus<'info> {
    #[account(
        mut,
        has_one = owner @ RegistryError::Unauthorized,
    )]
    pub deployment: Account<'info, Deployment>,
    pub owner: Signer<'info>,
}

#[derive(Accounts)]
pub struct RetireDeployment<'info> {
    #[account(
        mut,
        has_one = owner @ RegistryError::Unauthorized,
    )]
    pub deployment: Account<'info, Deployment>,
    pub owner: Signer<'info>,
}

#[derive(Accounts)]
pub struct SetOperatingWallet<'info> {
    #[account(
        mut,
        has_one = owner @ RegistryError::Unauthorized,
    )]
    pub deployment: Account<'info, Deployment>,
    pub owner: Signer<'info>,
}

// ─── Account schemas ───────────────────────────────────────────────────────

#[account]
#[derive(InitSpace)]
pub struct CompanyAccount {
    /// Schema version — bump on field changes.
    pub version: u8,
    /// Owning user wallet — also baked into the PDA seed.
    pub owner: Pubkey,
    /// Pointer to TreasuryAccount PDA (Pubkey::default() until Phase 2).
    pub treasury: Pubkey,
    /// Pointer to PolicyAccount PDA (Pubkey::default() until Phase 2).
    pub policy: Pubkey,
    /// Unix timestamp at creation.
    pub created_at: i64,
    /// Unix timestamp of last state mutation.
    pub updated_at: i64,
    /// Seed disambiguator — same owner can register multiple companies.
    pub nonce: u32,
    /// Active=0, Paused=1.
    pub status: u8,
    /// Display name.
    #[max_len(64)]
    pub name: String,
    /// BCP-47 locale tag for default UI rendering ("en", "id", ...).
    #[max_len(8)]
    pub locale: String,
    /// Off-chain metadata URI (IPFS / Arweave / HTTPS) — extended profile,
    /// brand assets, etc.
    #[max_len(200)]
    pub metadata_uri: String,
    /// SHA-256 of canonical metadata JSON for integrity verification.
    pub metadata_hash: [u8; 32],
}

#[account]
#[derive(InitSpace)]
pub struct AgentIdentity {
    /// Schema version.
    pub version: u8,
    /// Stable identity key — also baked into the PDA seed.
    pub agent_pubkey: Pubkey,
    /// Owning user wallet. Immutable — set at mint and never changes.
    pub owner: Pubkey,
    /// Unix timestamp at mint.
    pub created_at: i64,
    /// Unix timestamp of last state mutation.
    pub updated_at: i64,
    /// Display name (e.g. "Aiden"). Not unique — identities are
    /// disambiguated by `agent_pubkey`.
    #[max_len(64)]
    pub name: String,
    /// Off-chain metadata URI — persona, avatar, public bio.
    #[max_len(200)]
    pub metadata_uri: String,
    /// SHA-256 of metadata JSON.
    pub metadata_hash: [u8; 32],
    /// Pointer to ReputationAccount (Phase 2). Empty string = unminted.
    #[max_len(200)]
    pub reputation_uri: String,
}

#[account]
#[derive(InitSpace)]
pub struct Deployment {
    /// Schema version.
    pub version: u8,
    /// AgentIdentity PDA this deployment belongs to.
    pub agent_identity: Pubkey,
    /// CompanyAccount PDA this deployment belongs to.
    pub company: Pubkey,
    /// Per-company counter — also part of the PDA seed.
    pub deployment_index: u32,
    /// Mirror of `company.owner` for fast single-account auth checks.
    pub owner: Pubkey,
    /// Externally-provided wallet for agent-side transactions.
    /// Pubkey::default() = unset.
    pub operating_wallet: Pubkey,
    /// Pinned adapter (Pubkey::default() = unspecified).
    pub adapter_id: Pubkey,
    /// Capability persona / function tag (e.g. "ceo", "sdr"). NOT a
    /// job title — see Whitepaper §15.7 + CLAUDE.md naming guardrails.
    #[max_len(32)]
    pub role: String,
    /// Reporting parent within the company (None = top-level).
    pub parent_deployment_index: Option<u32>,
    /// Active=0, Paused=1, Retired=2 (terminal).
    pub status: u8,
    /// Unix timestamp when deployment was created.
    pub deployed_at: i64,
    /// Unix timestamp when retired (0 = still active).
    pub retired_at: i64,
    /// Unix timestamp of last state mutation.
    pub updated_at: i64,
    /// Off-chain metadata URI — model preferences, skill list, etc.
    #[max_len(200)]
    pub metadata_uri: String,
    /// SHA-256 of metadata JSON.
    pub metadata_hash: [u8; 32],
}

// ─── Errors ────────────────────────────────────────────────────────────────

#[error_code]
pub enum RegistryError {
    #[msg("signer does not match the account owner")]
    Unauthorized,
    #[msg("name is required")]
    NameRequired,
    #[msg("name exceeds MAX_NAME_LEN")]
    NameTooLong,
    #[msg("locale exceeds MAX_LOCALE_LEN")]
    LocaleTooLong,
    #[msg("role is required")]
    RoleRequired,
    #[msg("role exceeds MAX_ROLE_LEN")]
    RoleTooLong,
    #[msg("metadata_uri exceeds MAX_METADATA_URI_LEN")]
    MetadataUriTooLong,
    #[msg("invalid status value")]
    InvalidStatus,
    #[msg("deployment is retired and cannot be modified")]
    DeploymentRetired,
    #[msg("identity owner does not match company owner")]
    IdentityOwnerMismatch,
}
