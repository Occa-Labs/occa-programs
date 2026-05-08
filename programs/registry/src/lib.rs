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
use treasury::{
    cpi::accounts::InitTreasury as CpiInitTreasury, program::Treasury, OperationsAccount,
    OperationsKind,
};

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
pub const DEPLOYMENT_ACCOUNT_VERSION: u8 = 2;
pub const DAILY_ANCHOR_ACCOUNT_VERSION: u8 = 1;

// Seconds in a calendar day (UTC). Used to validate `day_unix` aligns to
// 00:00:00 in `commit_daily_anchor`.
pub const SECONDS_PER_DAY: i64 = 86_400;

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

    /// Create a new CompanyAccount PDA + atomically initialize its
    /// TreasuryAccount + PolicyAccount via CPI to the treasury program.
    /// Per design §6: company creation never leaves a window where treasury
    /// pointers are unset; any failure rolls back the entire transaction.
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
        let treasury_pda = ctx.accounts.treasury.key();
        let policy_pda = ctx.accounts.policy.key();
        {
            let company = &mut ctx.accounts.company;
            company.version = COMPANY_ACCOUNT_VERSION;
            company.owner = ctx.accounts.owner.key();
            company.treasury = treasury_pda;
            company.policy = policy_pda;
            company.created_at = now;
            company.updated_at = now;
            company.nonce = nonce;
            company.status = COMPANY_STATUS_ACTIVE;
            company.name = name;
            company.locale = locale;
            company.metadata_uri = metadata_uri;
            company.metadata_hash = metadata_hash;
        }

        // CPI into treasury::init_treasury — atomic with company creation.
        // If this fails, the whole tx rolls back including the CompanyAccount
        // init above. Treasury verifies `company` is owned by Registry via
        // its own `owner = REGISTRY_PROGRAM_ID` constraint; PDA seeds for
        // treasury+policy ensure 1:1 mapping with this company.
        let cpi_accounts = CpiInitTreasury {
            company: ctx.accounts.company.to_account_info(),
            treasury: ctx.accounts.treasury.to_account_info(),
            policy: ctx.accounts.policy.to_account_info(),
            payer: ctx.accounts.payer.to_account_info(),
            system_program: ctx.accounts.system_program.to_account_info(),
        };
        treasury::cpi::init_treasury(CpiContext::new(
            ctx.accounts.treasury_program.key(),
            cpi_accounts,
        ))?;

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
        deployment.receiving_address = Pubkey::default();
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

    /// Set or replace the deployment's receiving address — the passive
    /// destination wallet that funds disbursed *to* this agent land in.
    /// Pass `Pubkey::default()` to clear. Owner-only. NEVER a signer:
    /// this address does not authorize on-chain actions, it only receives.
    pub fn set_receiving_address(
        ctx: Context<SetReceivingAddress>,
        new_receiving_address: Pubkey,
    ) -> Result<()> {
        let deployment = &mut ctx.accounts.deployment;
        require!(
            deployment.status != DEPLOYMENT_STATUS_RETIRED,
            RegistryError::DeploymentRetired
        );
        deployment.receiving_address = new_receiving_address;
        deployment.updated_at = Clock::get()?.unix_timestamp;
        Ok(())
    }

    // ───────────────────────── Daily Anchor (§2 trace) ─────────────────────

    /// Commit a daily Merkle root anchor for a deployment's task hashes.
    ///
    /// Per Whitepaper §2 trace architecture: per-task records live off-chain
    /// in OCCA's database; this anchor proves OCCA didn't tamper with that
    /// day's task list. Anyone can re-hash off-chain records and verify
    /// against `merkle_root`.
    ///
    /// Seeds: `["daily_anchor", deployment_pda, day_unix_le_i64]`.
    /// PDA collision = at most one anchor per (deployment, day). Re-attempts
    /// on the same key fail naturally (Anchor `init` rejects).
    ///
    /// Authorization: signed by the Anchor Wallet registered as
    /// `OperationsAccount[Anchor]` in the treasury program. The signer
    /// pubkey must equal `operations.signer`; this ix's discriminator must
    /// be in `operations.action_whitelist`. The OperationsAccount is
    /// resolved via cross-program PDA lookup (`seeds::program = treasury::ID`).
    ///
    /// Phase 1 rate limit: NOT enforced. Registry cannot mutate the
    /// treasury-owned OperationsAccount's `signatures_this_period` counter
    /// without an extra CPI. PDA collision (one per deployment+day) provides
    /// natural deduplication; rent burn (~0.002 SOL each) caps griefing.
    /// If a stronger rate limit becomes necessary, add a treasury CPI ix
    /// `tick_anchor_signature` and wire it here.
    pub fn commit_daily_anchor(
        ctx: Context<CommitDailyAnchor>,
        day_unix: i64,
        merkle_root: [u8; 32],
        task_count: u32,
    ) -> Result<()> {
        require!(task_count > 0, RegistryError::EmptyAnchor);
        require!(
            day_unix > 0 && day_unix % SECONDS_PER_DAY == 0,
            RegistryError::InvalidDayBoundary
        );

        let now = Clock::get()?.unix_timestamp;
        require!(day_unix <= now, RegistryError::FutureAnchor);

        // Active-deployment requirement (§2 "per active agent").
        let deployment = &ctx.accounts.deployment;
        require!(
            deployment.status == DEPLOYMENT_STATUS_ACTIVE,
            RegistryError::DeploymentNotActive
        );
        require!(
            deployment.company == ctx.accounts.company.key(),
            RegistryError::CompanyMismatch
        );

        // Operations state (read-only).
        let ops = &ctx.accounts.operations;
        require!(!ops.revoked, RegistryError::OperationsRevoked);
        if ops.expiry_unix != 0 {
            require!(now < ops.expiry_unix, RegistryError::OperationsExpired);
        }

        // Whitelist check — this ix's own discriminator must be allowed.
        let disc_slice: &[u8] = crate::instruction::CommitDailyAnchor::DISCRIMINATOR;
        let disc_arr: [u8; 8] = disc_slice
            .try_into()
            .map_err(|_| error!(RegistryError::InvalidDiscriminator))?;
        require!(
            ops.action_whitelist.iter().any(|d| *d == disc_arr),
            RegistryError::DiscriminatorNotWhitelisted
        );

        let anchor_acc = &mut ctx.accounts.daily_anchor;
        anchor_acc.version = DAILY_ANCHOR_ACCOUNT_VERSION;
        anchor_acc.deployment = deployment.key();
        anchor_acc.company = ctx.accounts.company.key();
        anchor_acc.day_unix = day_unix;
        anchor_acc.merkle_root = merkle_root;
        anchor_acc.task_count = task_count;
        anchor_acc.committed_at = now;
        anchor_acc.committed_by = ctx.accounts.anchor_signer.key();
        anchor_acc.bump = ctx.bumps.daily_anchor;

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

    /// Pays rent for company + treasury + policy PDAs.
    #[account(mut)]
    pub payer: Signer<'info>,

    /// TreasuryAccount PDA — created by `treasury::init_treasury` CPI in
    /// the handler. Seed/owner verification happens inside that program.
    /// CHECK: address + ownership verified by treasury program via init.
    #[account(mut)]
    pub treasury: UncheckedAccount<'info>,

    /// PolicyAccount PDA — created by `treasury::init_treasury` CPI.
    /// CHECK: address + ownership verified by treasury program via init.
    #[account(mut)]
    pub policy: UncheckedAccount<'info>,

    /// Treasury program — invoked via CPI to atomically init treasury+policy.
    pub treasury_program: Program<'info, Treasury>,

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
pub struct SetReceivingAddress<'info> {
    #[account(
        mut,
        has_one = owner @ RegistryError::Unauthorized,
    )]
    pub deployment: Account<'info, Deployment>,
    pub owner: Signer<'info>,
}

#[derive(Accounts)]
#[instruction(day_unix: i64)]
pub struct CommitDailyAnchor<'info> {
    /// Deployment whose task stream this anchor covers.
    pub deployment: Account<'info, Deployment>,

    /// CompanyAccount referenced by deployment. Verified via
    /// `deployment.company == company.key()` constraint in handler so we
    /// can also resolve the OperationsAccount[Anchor] PDA.
    /// CHECK: matched against `deployment.company` in handler.
    pub company: UncheckedAccount<'info>,

    /// Anchor Wallet — pubkey verified against `operations.signer`.
    pub anchor_signer: Signer<'info>,

    /// `OperationsAccount[Anchor]` from treasury program. Resolved via
    /// cross-program PDA derivation. Anchor `Account<T>` auto-verifies
    /// owner = treasury::ID via `OperationsAccount`'s `Owner` impl.
    #[account(
        seeds = [b"operations", company.key().as_ref(), &[OperationsKind::Anchor.as_byte()]],
        bump = operations.bump,
        seeds::program = treasury::ID,
        constraint = operations.kind == OperationsKind::Anchor @ RegistryError::WrongOperationsKind,
        constraint = operations.signer == anchor_signer.key() @ RegistryError::Unauthorized,
        constraint = operations.company == company.key() @ RegistryError::CompanyMismatch,
    )]
    pub operations: Account<'info, OperationsAccount>,

    #[account(
        init,
        payer = payer,
        space = 8 + DailyAnchorAccount::INIT_SPACE,
        seeds = [b"daily_anchor", deployment.key().as_ref(), &day_unix.to_le_bytes()],
        bump,
    )]
    pub daily_anchor: Account<'info, DailyAnchorAccount>,

    /// Pays rent for the DailyAnchorAccount PDA.
    #[account(mut)]
    pub payer: Signer<'info>,

    pub system_program: Program<'info, System>,
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
    /// Passive destination wallet for funds disbursed *to* this agent
    /// (Agent Receiving Address per Whitepaper §8.2 v0.10). NOT a signer
    /// — never authorizes on-chain actions. Treasury disburse instructions
    /// match against this field to identify intra-company transfers and
    /// deduct the Agent Operating Fee. `Pubkey::default()` = unset
    /// (disbursements to this deployment will fail until set).
    pub receiving_address: Pubkey,
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

#[account]
#[derive(InitSpace)]
pub struct DailyAnchorAccount {
    /// Schema version.
    pub version: u8,
    /// Deployment whose task stream this anchor covers.
    pub deployment: Pubkey,
    /// CompanyAccount the deployment belongs to (denormalized for fast
    /// indexing — avoids a deployment-account fetch on read).
    pub company: Pubkey,
    /// Unix timestamp of 00:00:00 UTC for the day this anchor covers.
    /// Aligned by `day_unix % 86_400 == 0` constraint at commit time.
    pub day_unix: i64,
    /// Merkle root over that day's task hashes (off-chain DB).
    pub merkle_root: [u8; 32],
    /// Number of leaves (tasks) in the Merkle tree. Must be > 0 — empty
    /// days produce no anchor (§2).
    pub task_count: u32,
    /// Unix timestamp when this commit landed on-chain.
    pub committed_at: i64,
    /// Anchor Wallet pubkey that signed the commit. Mirror of
    /// `OperationsAccount[Anchor].signer` at commit time.
    pub committed_by: Pubkey,
    /// Bump for PDA verification.
    pub bump: u8,
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
    #[msg("daily anchor must cover at least one task")]
    EmptyAnchor,
    #[msg("day_unix must be > 0 and aligned to 00:00:00 UTC (multiple of 86400)")]
    InvalidDayBoundary,
    #[msg("daily anchor cannot be for a future day")]
    FutureAnchor,
    #[msg("deployment must be active to commit anchor")]
    DeploymentNotActive,
    #[msg("deployment.company does not match passed company account")]
    CompanyMismatch,
    #[msg("operations account is revoked")]
    OperationsRevoked,
    #[msg("operations account is past its expiry")]
    OperationsExpired,
    #[msg("operations account kind does not match instruction expectation")]
    WrongOperationsKind,
    #[msg("instruction discriminator is not whitelisted on operations account")]
    DiscriminatorNotWhitelisted,
    #[msg("could not parse instruction discriminator")]
    InvalidDiscriminator,
}
