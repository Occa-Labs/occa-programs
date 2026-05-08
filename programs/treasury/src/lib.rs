// OCCA Treasury Program — milestones 1+2.
//
// Owns four account types:
//   • TreasuryAccount     — funds custody + accepted-asset allow-list per company.
//   • PolicyAccount       — authorization rules + per-period budgets per company.
//   • OperationsAccount   — capability-bounded signer registration (per company,
//                           per kind: Disbursement / Anchor).
//   • ProtocolFeeAccount  — protocol fee collection (singleton).
//
// Companion programs:
//   • Registry — owns CompanyAccount/AgentIdentity/Deployment. Registry will
//     CPI into `init_treasury` from `create_company` once milestone 5 lands.
//     Until then, init is invoked directly with the operator paying rent.
//
// Cargo dependency choice: treasury does NOT depend on the registry crate.
// Reason: in M5 the registry will depend on treasury for CPI (`features =
// ["cpi"]`). A bidirectional dep would be a Cargo cycle. We therefore read
// `CompanyAccount.owner` manually from a known byte offset; layout drift is
// caught by `version` byte at offset 8 (currently `3`, see `read_company_owner`).
//
// Milestone status:
//   • M1 — schemas + atomic init_treasury (Treasury+Policy) + init_protocol_fee_account. ✓
//   • M2 (this) — set_policy + calendar arithmetic (civil_from_days) + lazy rollover helpers.
//   • M3 — register/update/revoke/close OperationsAccount.
//   • M4 — disburse_routine / disburse_discretionary / disburse_privileged + Agent Operating Fee.
//   • M5 — Registry CPI from create_company; close direct caller path.
//   • M6 — Anchor unit + integration tests.
//
// Truth model: this program owns value-layer state. Off-chain DBs must be
// re-buildable from chain alone. See `occa/CLAUDE.md` "Chain = truth, DB = cache".

use anchor_lang::prelude::*;

declare_id!("occaxyVLnurdjedWCBPrvDCCto8wGYadtTZ3nAmcVzh");

/// Registry program ID — used to validate that `CompanyAccount` passed in
/// is genuinely owned by the Registry program (not a forgery).
const REGISTRY_PROGRAM_ID: Pubkey = pubkey!("occaTHMv5eYG5aZ85jimxTvHkBfsDCvndXC6J2k8kxr");

/// Registry's `CompanyAccount` schema version this program is built against.
/// Layout assumed by `read_company_owner`:
///   `[8B disc][1B version][32B owner][...]`
/// If Registry bumps this version, audit `read_company_owner` and update.
const SUPPORTED_COMPANY_VERSION: u8 = 3;

/// Byte offset of `owner: Pubkey` inside Registry's CompanyAccount data.
/// 8 (Anchor discriminator) + 1 (version: u8) = 9.
const COMPANY_OWNER_OFFSET: usize = 9;

/// Registry's `Deployment` schema version this program reads against.
/// Layout assumed by `read_deployment`:
///   `[8B disc][1B version][32B agent_identity][32B company][4B index][32B owner][32B receiving_address][...]`
const SUPPORTED_DEPLOYMENT_VERSION: u8 = 2;
/// Byte offset of `company: Pubkey` inside Deployment.
/// 8 (disc) + 1 (version) + 32 (agent_identity) = 41.
const DEPLOYMENT_COMPANY_OFFSET: usize = 41;
/// Byte offset of `receiving_address: Pubkey` inside Deployment.
/// 41 + 32 (company) + 4 (deployment_index) + 32 (owner) = 109.
const DEPLOYMENT_RECEIVING_OFFSET: usize = 109;

/// SOL pseudo-mint marker for the accepted-asset list. SOL has no real mint
/// pubkey on Solana (lamports live directly on accounts), so we use the
/// default pubkey as a sentinel. SPL mints are added later via `set_policy`.
pub const SOL_PSEUDO_MINT: Pubkey = Pubkey::new_from_array([0u8; 32]);

// ─── Account schema versions (bump on field changes) ───────────────────────
pub const TREASURY_ACCOUNT_VERSION: u8 = 1;
pub const POLICY_ACCOUNT_VERSION: u8 = 1;
pub const OPERATIONS_ACCOUNT_VERSION: u8 = 1;
pub const PROTOCOL_FEE_ACCOUNT_VERSION: u8 = 1;

// ─── Defaults ──────────────────────────────────────────────────────────────
/// 3% Agent Operating Fee — per design doc §2 ratification + §11.1.
pub const DEFAULT_AGENT_OPERATING_FEE_BPS: u16 = 300;
/// 100% — sanity ceiling for fee bps.
pub const MAX_FEE_BPS: u16 = 10_000;

// ─── Bounds ────────────────────────────────────────────────────────────────
pub const MAX_ACCEPTED_ASSETS: usize = 8;
pub const MAX_BUDGET_ENTRIES: usize = 8;
pub const MAX_WHITELIST_ENTRIES: usize = 8;

#[program]
pub mod treasury {
    use super::*;

    /// Atomically create the TreasuryAccount + PolicyAccount PDA pair bound
    /// to a CompanyAccount. Per design doc §6, both accounts are initialized
    /// together so a company never exists in a state where treasury is set
    /// but policy is not (or vice versa).
    ///
    /// Seeds:
    ///   • treasury: `["treasury", company_pda]`
    ///   • policy:   `["policy",   company_pda]`
    ///
    /// Phase 1 defaults (per §2 + §3.2):
    ///   • treasury.accepted_assets = `[SOL_PSEUDO_MINT]` — SPL mints added
    ///     later via Privileged-class `set_policy`.
    ///   • policy budgets = empty (nothing disbursable until operator calls
    ///     `set_policy`).
    ///   • policy.privileged_threshold_lamports = `u64::MAX` — secondary
    ///     signer never required by default.
    ///   • policy.agent_operating_fee_bps = `DEFAULT_AGENT_OPERATING_FEE_BPS`
    ///     (300 bps = 3%).
    ///   • policy.current_period_anchor = 0 — lazy-initialized on first
    ///     disbursement.
    ///
    /// Authorization model (M5): canonical invocation is via CPI from
    /// `registry::create_company`. Direct calls remain technically allowed
    /// — there is no controlling-authority check here because Anchor does
    /// not flush `Account<T>` mutations to the underlying data buffer until
    /// handler exit, so reading `company.owner` mid-CPI returns stale bytes.
    /// The omitted check is *safe* because:
    ///   1. Defaults written by this ix are identical regardless of caller.
    ///   2. PDA seeds pin treasury+policy to the company; re-init is impossible.
    ///   3. All subsequent operations (`set_policy`, `disburse_*`,
    ///      `register_*`) verify controlling_authority against `company.owner`.
    /// The worst an attacker can do is burn rent (~0.005 SOL) to pre-init a
    /// stranger's treasury with the same defaults the legitimate owner would
    /// have gotten anyway.
    pub fn init_treasury(ctx: Context<InitTreasury>) -> Result<()> {
        let company_key = ctx.accounts.company.key();

        let treasury = &mut ctx.accounts.treasury;
        treasury.version = TREASURY_ACCOUNT_VERSION;
        treasury.company = company_key;
        treasury.accepted_assets = vec![SOL_PSEUDO_MINT];
        treasury.bump = ctx.bumps.treasury;

        let policy = &mut ctx.accounts.policy;
        policy.version = POLICY_ACCOUNT_VERSION;
        policy.company = company_key;
        policy.routine_budget_per_month = vec![];
        policy.discretionary_budget_per_month = vec![];
        policy.privileged_threshold_lamports = u64::MAX;
        policy.privileged_threshold_per_token = vec![];
        policy.secondary_signer = None;
        policy.agent_operating_fee_bps = DEFAULT_AGENT_OPERATING_FEE_BPS;
        policy.current_period_anchor = 0;
        policy.routine_spent_this_period = vec![];
        policy.discretionary_spent_this_period = vec![];
        policy.bump = ctx.bumps.policy;

        Ok(())
    }

    /// Update policy fields and/or treasury's accepted-asset list.
    /// Privileged class (§4.2 + §7) — signed by controlling authority.
    ///
    /// Each field in `params` is `Option<T>`: `None` = no change, `Some(v)`
    /// = set. `secondary_signer` is `Option<Option<Pubkey>>` so the caller
    /// can distinguish "leave unchanged" (outer `None`), "clear" (outer
    /// `Some(None)`), and "set" (outer `Some(Some(pk))`).
    ///
    /// Validation: fee bps ≤ 10_000; vec lengths ≤ MAX_*; mint uniqueness
    /// inside any single vec. Period rollover is NOT performed here — spent
    /// counters are only touched by disbursement instructions (M4+).
    pub fn set_policy(ctx: Context<SetPolicy>, params: SetPolicyParams) -> Result<()> {
        let company_owner = read_company_owner(&ctx.accounts.company)?;
        require!(
            ctx.accounts.controlling_authority.key() == company_owner,
            TreasuryError::UnauthorizedSigner
        );

        let policy = &mut ctx.accounts.policy;
        let treasury = &mut ctx.accounts.treasury;

        if let Some(budgets) = params.routine_budget_per_month {
            validate_budget_vec(&budgets)?;
            policy.routine_budget_per_month = budgets;
        }
        if let Some(budgets) = params.discretionary_budget_per_month {
            validate_budget_vec(&budgets)?;
            policy.discretionary_budget_per_month = budgets;
        }
        if let Some(t) = params.privileged_threshold_lamports {
            policy.privileged_threshold_lamports = t;
        }
        if let Some(thresholds) = params.privileged_threshold_per_token {
            validate_budget_vec(&thresholds)?;
            policy.privileged_threshold_per_token = thresholds;
        }
        if let Some(secondary) = params.secondary_signer {
            policy.secondary_signer = secondary;
        }
        if let Some(bps) = params.agent_operating_fee_bps {
            require!(bps <= MAX_FEE_BPS, TreasuryError::InvalidFeeBps);
            policy.agent_operating_fee_bps = bps;
        }
        if let Some(assets) = params.accepted_assets {
            validate_accepted_assets(&assets)?;
            treasury.accepted_assets = assets;
        }

        Ok(())
    }

    /// One-time singleton init for the protocol-wide fee accumulator. Called
    /// once at program deployment by the upgrade authority. The `governance`
    /// pubkey passed in is the long-lived withdrawal authority (e.g. a
    /// multisig / DAO key) — it may differ from the deployer's upgrade key.
    ///
    /// Seeds: `["protocol_fees"]` — singleton.
    ///
    /// Authorization: signer must equal the program's upgrade authority,
    /// verified by reading the program's ProgramData PDA
    /// (`upgrade_authority_address`). Prevents anyone from calling this ix
    /// and pinning their own governance key on a fresh deploy.
    pub fn init_protocol_fee_account(
        ctx: Context<InitProtocolFeeAccount>,
        governance: Pubkey,
    ) -> Result<()> {
        let acc = &mut ctx.accounts.protocol_fee_account;
        acc.version = PROTOCOL_FEE_ACCOUNT_VERSION;
        acc.governance = governance;
        acc.balances = vec![];
        acc.bump = ctx.bumps.protocol_fee_account;
        Ok(())
    }

    /// Register a company-scoped operations key (Disbursement | Anchor).
    /// Privileged class — signed by controlling authority (= company.owner).
    ///
    /// Seeds: `["operations", company_pda, &[kind.as_byte()]]`.
    /// Up to 2 per company: one Disbursement, one Anchor (§3.3).
    ///
    /// Phase 1 scope: company-scoped only. `register_agent_operations` is
    /// deferred to Phase 2+ per design §2 — per-agent operations keys are
    /// not needed until marketplace settlement.
    ///
    /// `action_whitelist`: 8-byte Anchor instruction discriminators allowed
    /// to be signed by this key. Caller is responsible for computing them
    /// off-chain (e.g. `sha256("global:disburse_routine")[0..8]`). Expected
    /// values per design §3.3:
    ///   • Disbursement: `[disburse_routine]`
    ///   • Anchor:       `[commit_daily_anchor]` (lives in Registry program)
    ///
    /// `expiry_unix`: `0` for no expiry, otherwise must be in the future.
    /// `rate_limit_per_period`: max signatures per calendar month (lazily
    /// rolled over inside the ix being signed, in M4+).
    pub fn register_company_operations(
        ctx: Context<RegisterCompanyOperations>,
        kind: OperationsKind,
        signer: Pubkey,
        action_whitelist: Vec<[u8; 8]>,
        rate_limit_per_period: u32,
        expiry_unix: i64,
    ) -> Result<()> {
        let company_owner = read_company_owner(&ctx.accounts.company)?;
        require!(
            ctx.accounts.controlling_authority.key() == company_owner,
            TreasuryError::UnauthorizedSigner
        );

        require!(signer != Pubkey::default(), TreasuryError::InvalidSigner);
        require!(!action_whitelist.is_empty(), TreasuryError::EmptyWhitelist);
        require!(
            action_whitelist.len() <= MAX_WHITELIST_ENTRIES,
            TreasuryError::TooManyEntries
        );

        if expiry_unix != 0 {
            let now = Clock::get()?.unix_timestamp;
            require!(expiry_unix > now, TreasuryError::ExpiryInPast);
        }

        let ops = &mut ctx.accounts.operations;
        ops.version = OPERATIONS_ACCOUNT_VERSION;
        ops.company = ctx.accounts.company.key();
        ops.kind = kind;
        ops.signer = signer;
        ops.action_whitelist = action_whitelist;
        ops.rate_limit_per_period = rate_limit_per_period;
        ops.signatures_this_period = 0;
        ops.current_period_anchor = 0; // lazy-init on first use
        ops.expiry_unix = expiry_unix;
        ops.revoked = false;
        ops.bump = ctx.bumps.operations;

        Ok(())
    }

    /// Modify an OperationsAccount's whitelist / rate limit / expiry without
    /// rotating the signer pubkey. Privileged class.
    ///
    /// Each field of `params` is `Option<T>`: `None` = leave unchanged.
    /// To rotate the `signer` pubkey, the operator must `revoke_operations`
    /// → `close_operations` → `register_company_operations` with the new
    /// pubkey. This forces an audit trail across pubkey rotation events.
    ///
    /// Allowed on expired ops (operator can extend expiry to revive).
    /// Rejected on revoked ops (terminal state — must close + recreate).
    pub fn update_operations_capability(
        ctx: Context<UpdateOperationsCapability>,
        params: UpdateOperationsCapabilityParams,
    ) -> Result<()> {
        let company_owner = read_company_owner(&ctx.accounts.company)?;
        require!(
            ctx.accounts.controlling_authority.key() == company_owner,
            TreasuryError::UnauthorizedSigner
        );

        let ops = &mut ctx.accounts.operations;
        require!(!ops.revoked, TreasuryError::OperationsRevoked);

        if let Some(whitelist) = params.action_whitelist {
            require!(!whitelist.is_empty(), TreasuryError::EmptyWhitelist);
            require!(
                whitelist.len() <= MAX_WHITELIST_ENTRIES,
                TreasuryError::TooManyEntries
            );
            ops.action_whitelist = whitelist;
        }
        if let Some(rate) = params.rate_limit_per_period {
            ops.rate_limit_per_period = rate;
        }
        if let Some(expiry) = params.expiry_unix {
            if expiry != 0 {
                let now = Clock::get()?.unix_timestamp;
                require!(expiry > now, TreasuryError::ExpiryInPast);
            }
            ops.expiry_unix = expiry;
        }

        Ok(())
    }

    /// Mark an OperationsAccount revoked. Terminal flag — once set, no
    /// further signatures via this account are accepted (M4+ disburse
    /// handlers reject `ops.revoked == true`). Privileged class.
    ///
    /// Rotation flow: `revoke_operations` → `close_operations` (refunds
    /// rent) → `register_company_operations` with the new signer pubkey.
    pub fn revoke_operations(ctx: Context<RevokeOperations>) -> Result<()> {
        let company_owner = read_company_owner(&ctx.accounts.company)?;
        require!(
            ctx.accounts.controlling_authority.key() == company_owner,
            TreasuryError::UnauthorizedSigner
        );

        let ops = &mut ctx.accounts.operations;
        require!(!ops.revoked, TreasuryError::AlreadyRevoked);
        ops.revoked = true;

        Ok(())
    }

    /// Close a revoked OperationsAccount and refund rent to the controlling
    /// authority. Must be revoked first (terminal-state precondition makes
    /// the rotation flow explicit + auditable).
    pub fn close_operations(ctx: Context<CloseOperations>) -> Result<()> {
        let company_owner = read_company_owner(&ctx.accounts.company)?;
        require!(
            ctx.accounts.controlling_authority.key() == company_owner,
            TreasuryError::UnauthorizedSigner
        );

        require!(
            ctx.accounts.operations.revoked,
            TreasuryError::NotRevoked
        );
        // Anchor's `close = controlling_authority` constraint handles the
        // lamport refund + zero-out at end of ix.
        Ok(())
    }

    // ───────────────────────── Disbursements (§4.4) ─────────────────────────

    /// Routine-class disbursement to an agent's receiving address.
    /// Signed by `OperationsAccount[Disbursement].signer` (operator-held key,
    /// OCCA never holds the privkey). Per design §2, the FE constructs a
    /// batched tx of multiple `disburse_routine` ixs (one per agent) and
    /// the operator signs it manually each pay period.
    ///
    /// Validation pipeline (§4.4 row 1 + §7):
    ///   1. Mint is in `treasury.accepted_assets`.
    ///   2. Phase 1: mint == `SOL_PSEUDO_MINT` (SPL deferred per §2).
    ///   3. Destination matches `Deployment.receiving_address` and the
    ///      Deployment belongs to the same company.
    ///   4. OperationsAccount is not revoked, not expired.
    ///   5. This ix's discriminator is in `ops.action_whitelist`.
    ///   6. Lazy-rollover both `policy.current_period_anchor` and
    ///      `ops.current_period_anchor` to current month start.
    ///   7. `ops.signatures_this_period < ops.rate_limit_per_period`.
    ///   8. `routine_budget_per_month[mint] - routine_spent_this_period[mint] >= amount + fee`.
    ///
    /// Fee: always applies (intra-company agent destination per §11.1).
    /// `fee = amount * agent_operating_fee_bps / 10_000`.
    /// `gross = amount + fee` debited from treasury; `amount` to destination,
    /// `fee` to ProtocolFeeAccount.
    pub fn disburse_routine(
        ctx: Context<DisburseRoutine>,
        mint: Pubkey,
        amount: u64,
    ) -> Result<()> {
        require!(amount > 0, TreasuryError::ZeroAmount);
        require!(mint == SOL_PSEUDO_MINT, TreasuryError::SplNotSupported);
        require!(
            ctx.accounts.treasury.accepted_assets.contains(&mint),
            TreasuryError::AssetNotAllowListed
        );

        // Verify destination via Deployment.
        let dep = read_deployment(&ctx.accounts.deployment)?;
        require!(
            dep.company == ctx.accounts.company.key(),
            TreasuryError::DeploymentCompanyMismatch
        );
        require!(
            dep.receiving_address != Pubkey::default(),
            TreasuryError::ReceivingAddressUnset
        );
        require!(
            dep.receiving_address == ctx.accounts.destination.key(),
            TreasuryError::DestinationMismatch
        );

        let now = Clock::get()?.unix_timestamp;
        let ops = &mut ctx.accounts.operations;

        require!(!ops.revoked, TreasuryError::OperationsRevoked);
        if ops.expiry_unix != 0 {
            require!(now < ops.expiry_unix, TreasuryError::OperationsExpired);
        }

        // Whitelist check — this ix's own discriminator must be allowed.
        let disc_slice: &[u8] = crate::instruction::DisburseRoutine::DISCRIMINATOR;
        let disc_arr: [u8; 8] = disc_slice
            .try_into()
            .map_err(|_| error!(TreasuryError::InvalidProgramData))?;
        require!(
            ops.action_whitelist.iter().any(|d| *d == disc_arr),
            TreasuryError::DiscriminatorNotWhitelisted
        );

        // Lazy rollover.
        rollover_operations_period(ops, now);
        let policy = &mut ctx.accounts.policy;
        rollover_policy_period(policy, now);

        // Rate limit.
        require!(
            ops.signatures_this_period < ops.rate_limit_per_period,
            TreasuryError::RateLimitExceeded
        );

        // Fee + budget.
        let (fee, gross) = compute_fee(amount, policy.agent_operating_fee_bps)?;
        let routine_budget = get_asset_amount(&policy.routine_budget_per_month, mint);
        apply_spent(
            &mut policy.routine_spent_this_period,
            routine_budget,
            mint,
            gross,
        )?;

        // Lamport movement.
        let treasury_info = ctx.accounts.treasury.to_account_info();
        let dest_info = ctx.accounts.destination.to_account_info();
        let fee_info = ctx.accounts.protocol_fee_account.to_account_info();

        debit_treasury_lamports(&treasury_info, gross)?;
        credit_lamports(&dest_info, amount)?;
        if fee > 0 {
            credit_lamports(&fee_info, fee)?;
            upsert_asset_amount(
                &mut ctx.accounts.protocol_fee_account.balances,
                mint,
                fee,
            )?;
        }

        // Increment ops counter (after all fallible ops succeeded).
        ops.signatures_this_period = ops
            .signatures_this_period
            .checked_add(1)
            .ok_or(TreasuryError::ArithmeticOverflow)?;

        Ok(())
    }

    /// Discretionary-class disbursement to an agent's receiving address.
    /// Signed by controlling authority (= company.owner). Used for ad-hoc
    /// agent payouts outside of recurring routine flow.
    ///
    /// Same validation as routine except: no operations account, no rate
    /// limit, no whitelist check. Budget check uses `discretionary_*`
    /// counters. Fee always applies (intra-company agent destination).
    pub fn disburse_discretionary(
        ctx: Context<DisburseDiscretionary>,
        mint: Pubkey,
        amount: u64,
    ) -> Result<()> {
        let company_owner = read_company_owner(&ctx.accounts.company)?;
        require!(
            ctx.accounts.controlling_authority.key() == company_owner,
            TreasuryError::UnauthorizedSigner
        );

        require!(amount > 0, TreasuryError::ZeroAmount);
        require!(mint == SOL_PSEUDO_MINT, TreasuryError::SplNotSupported);
        require!(
            ctx.accounts.treasury.accepted_assets.contains(&mint),
            TreasuryError::AssetNotAllowListed
        );

        let dep = read_deployment(&ctx.accounts.deployment)?;
        require!(
            dep.company == ctx.accounts.company.key(),
            TreasuryError::DeploymentCompanyMismatch
        );
        require!(
            dep.receiving_address != Pubkey::default(),
            TreasuryError::ReceivingAddressUnset
        );
        require!(
            dep.receiving_address == ctx.accounts.destination.key(),
            TreasuryError::DestinationMismatch
        );

        let now = Clock::get()?.unix_timestamp;
        let policy = &mut ctx.accounts.policy;
        rollover_policy_period(policy, now);

        let (fee, gross) = compute_fee(amount, policy.agent_operating_fee_bps)?;
        let discretionary_budget =
            get_asset_amount(&policy.discretionary_budget_per_month, mint);
        apply_spent(
            &mut policy.discretionary_spent_this_period,
            discretionary_budget,
            mint,
            gross,
        )?;

        let treasury_info = ctx.accounts.treasury.to_account_info();
        let dest_info = ctx.accounts.destination.to_account_info();
        let fee_info = ctx.accounts.protocol_fee_account.to_account_info();

        debit_treasury_lamports(&treasury_info, gross)?;
        credit_lamports(&dest_info, amount)?;
        if fee > 0 {
            credit_lamports(&fee_info, fee)?;
            upsert_asset_amount(
                &mut ctx.accounts.protocol_fee_account.balances,
                mint,
                fee,
            )?;
        }

        Ok(())
    }

    /// Privileged-class disbursement. Destination unrestricted: may be an
    /// agent receiving address (fee applies) or any external pubkey (no fee).
    /// Signed by controlling authority. Above-threshold amounts additionally
    /// require the secondary signer registered in policy.
    ///
    /// `is_agent_destination`: if `true`, caller MUST pass the agent's
    /// `Deployment` as `deployment` and the destination must equal
    /// `deployment.receiving_address`. Fee deducted per §11.1.
    /// If `false`, `deployment` should be omitted; no fee applies.
    ///
    /// No budget check — privileged class is unbudgeted by design (§3.2 has
    /// no privileged_budget field).
    pub fn disburse_privileged(
        ctx: Context<DisbursePrivileged>,
        mint: Pubkey,
        amount: u64,
        is_agent_destination: bool,
    ) -> Result<()> {
        let company_owner = read_company_owner(&ctx.accounts.company)?;
        require!(
            ctx.accounts.controlling_authority.key() == company_owner,
            TreasuryError::UnauthorizedSigner
        );

        require!(amount > 0, TreasuryError::ZeroAmount);
        require!(mint == SOL_PSEUDO_MINT, TreasuryError::SplNotSupported);
        require!(
            ctx.accounts.treasury.accepted_assets.contains(&mint),
            TreasuryError::AssetNotAllowListed
        );

        let policy = &ctx.accounts.policy;

        // Above-threshold ⇒ secondary signer required.
        // Phase 1 SOL only ⇒ compare against `privileged_threshold_lamports`.
        if amount > policy.privileged_threshold_lamports {
            let registered = policy
                .secondary_signer
                .ok_or(TreasuryError::SecondarySignerRequired)?;
            let provided = ctx
                .accounts
                .secondary_signer
                .as_ref()
                .ok_or(TreasuryError::SecondarySignerRequired)?;
            require!(
                provided.key() == registered,
                TreasuryError::SecondarySignerMismatch
            );
        }

        // Fee path — only if destination is intra-company agent.
        let fee = if is_agent_destination {
            let dep_acc = ctx
                .accounts
                .deployment
                .as_ref()
                .ok_or(TreasuryError::MissingDeploymentForAgentDestination)?;
            let dep = read_deployment(dep_acc)?;
            require!(
                dep.company == ctx.accounts.company.key(),
                TreasuryError::DeploymentCompanyMismatch
            );
            require!(
                dep.receiving_address != Pubkey::default(),
                TreasuryError::ReceivingAddressUnset
            );
            require!(
                dep.receiving_address == ctx.accounts.destination.key(),
                TreasuryError::DestinationMismatch
            );
            let (fee, _gross) = compute_fee(amount, policy.agent_operating_fee_bps)?;
            fee
        } else {
            0
        };

        let gross = amount
            .checked_add(fee)
            .ok_or(TreasuryError::ArithmeticOverflow)?;

        let treasury_info = ctx.accounts.treasury.to_account_info();
        let dest_info = ctx.accounts.destination.to_account_info();
        let fee_info = ctx.accounts.protocol_fee_account.to_account_info();

        debit_treasury_lamports(&treasury_info, gross)?;
        credit_lamports(&dest_info, amount)?;
        if fee > 0 {
            credit_lamports(&fee_info, fee)?;
            upsert_asset_amount(
                &mut ctx.accounts.protocol_fee_account.balances,
                mint,
                fee,
            )?;
        }

        Ok(())
    }
}

// ─── Account contexts ──────────────────────────────────────────────────────

#[derive(Accounts)]
pub struct InitTreasury<'info> {
    /// CompanyAccount this treasury+policy bind to. Program-owner match
    /// enforced — must be a real Registry-owned account, not a forgery.
    /// CHECK: ownership-program match enforced via `owner` constraint.
    #[account(owner = REGISTRY_PROGRAM_ID @ TreasuryError::CompanyOwnerMismatch)]
    pub company: UncheckedAccount<'info>,

    #[account(
        init,
        payer = payer,
        space = 8 + TreasuryAccount::INIT_SPACE,
        seeds = [b"treasury", company.key().as_ref()],
        bump,
    )]
    pub treasury: Account<'info, TreasuryAccount>,

    #[account(
        init,
        payer = payer,
        space = 8 + PolicyAccount::INIT_SPACE,
        seeds = [b"policy", company.key().as_ref()],
        bump,
    )]
    pub policy: Account<'info, PolicyAccount>,

    /// Pays rent for both PDAs. Direct call: operator hot wallet. CPI from
    /// `registry::create_company`: the same `payer` account from registry's
    /// outer ix is forwarded through.
    #[account(mut)]
    pub payer: Signer<'info>,

    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct SetPolicy<'info> {
    /// CompanyAccount this policy/treasury belong to.
    /// CHECK: ownership + signer check enforced as in `InitTreasury`.
    #[account(owner = REGISTRY_PROGRAM_ID @ TreasuryError::CompanyOwnerMismatch)]
    pub company: UncheckedAccount<'info>,

    /// Controlling Authority — must equal `CompanyAccount.owner`.
    pub controlling_authority: Signer<'info>,

    #[account(
        mut,
        seeds = [b"treasury", company.key().as_ref()],
        bump = treasury.bump,
        constraint = treasury.company == company.key() @ TreasuryError::PolicyMismatch,
    )]
    pub treasury: Account<'info, TreasuryAccount>,

    #[account(
        mut,
        seeds = [b"policy", company.key().as_ref()],
        bump = policy.bump,
        constraint = policy.company == company.key() @ TreasuryError::PolicyMismatch,
    )]
    pub policy: Account<'info, PolicyAccount>,
}

#[derive(Accounts)]
pub struct InitProtocolFeeAccount<'info> {
    #[account(
        init,
        payer = authority,
        space = 8 + ProtocolFeeAccount::INIT_SPACE,
        seeds = [b"protocol_fees"],
        bump,
    )]
    pub protocol_fee_account: Account<'info, ProtocolFeeAccount>,

    /// Upgrade authority of this program. Pays rent + signs.
    #[account(mut)]
    pub authority: Signer<'info>,

    /// This program's executable account — used to derive ProgramData PDA.
    #[account(constraint = program.programdata_address()? == Some(program_data.key()) @ TreasuryError::InvalidProgramData)]
    pub program: Program<'info, crate::program::Treasury>,

    /// ProgramData PDA — verifies `authority` is the actual upgrade authority.
    #[account(constraint = program_data.upgrade_authority_address == Some(authority.key()) @ TreasuryError::NotUpgradeAuthority)]
    pub program_data: Account<'info, ProgramData>,

    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
#[instruction(kind: OperationsKind)]
pub struct RegisterCompanyOperations<'info> {
    /// CHECK: ownership-program match enforced via `owner` constraint;
    /// `controlling_authority` signer check happens in handler.
    #[account(owner = REGISTRY_PROGRAM_ID @ TreasuryError::CompanyOwnerMismatch)]
    pub company: UncheckedAccount<'info>,

    /// Controlling Authority — must equal `CompanyAccount.owner`.
    pub controlling_authority: Signer<'info>,

    #[account(
        init,
        payer = payer,
        space = 8 + OperationsAccount::INIT_SPACE,
        seeds = [b"operations", company.key().as_ref(), &[kind.as_byte()]],
        bump,
    )]
    pub operations: Account<'info, OperationsAccount>,

    /// Pays rent. Typically the operator hot wallet.
    #[account(mut)]
    pub payer: Signer<'info>,

    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct UpdateOperationsCapability<'info> {
    /// CHECK: ownership-program match enforced via `owner` constraint;
    /// `controlling_authority` signer check happens in handler.
    #[account(owner = REGISTRY_PROGRAM_ID @ TreasuryError::CompanyOwnerMismatch)]
    pub company: UncheckedAccount<'info>,

    pub controlling_authority: Signer<'info>,

    #[account(
        mut,
        seeds = [b"operations", company.key().as_ref(), &[operations.kind.as_byte()]],
        bump = operations.bump,
        constraint = operations.company == company.key() @ TreasuryError::PolicyMismatch,
    )]
    pub operations: Account<'info, OperationsAccount>,
}

#[derive(Accounts)]
pub struct RevokeOperations<'info> {
    /// CHECK: ownership-program match enforced via `owner` constraint;
    /// `controlling_authority` signer check happens in handler.
    #[account(owner = REGISTRY_PROGRAM_ID @ TreasuryError::CompanyOwnerMismatch)]
    pub company: UncheckedAccount<'info>,

    pub controlling_authority: Signer<'info>,

    #[account(
        mut,
        seeds = [b"operations", company.key().as_ref(), &[operations.kind.as_byte()]],
        bump = operations.bump,
        constraint = operations.company == company.key() @ TreasuryError::PolicyMismatch,
    )]
    pub operations: Account<'info, OperationsAccount>,
}

#[derive(Accounts)]
pub struct CloseOperations<'info> {
    /// CHECK: ownership-program match enforced via `owner` constraint;
    /// `controlling_authority` signer check happens in handler.
    #[account(owner = REGISTRY_PROGRAM_ID @ TreasuryError::CompanyOwnerMismatch)]
    pub company: UncheckedAccount<'info>,

    /// Receives rent refund.
    #[account(mut)]
    pub controlling_authority: Signer<'info>,

    #[account(
        mut,
        close = controlling_authority,
        seeds = [b"operations", company.key().as_ref(), &[operations.kind.as_byte()]],
        bump = operations.bump,
        constraint = operations.company == company.key() @ TreasuryError::PolicyMismatch,
    )]
    pub operations: Account<'info, OperationsAccount>,
}

#[derive(Accounts)]
pub struct DisburseRoutine<'info> {
    /// CHECK: ownership-program match.
    #[account(owner = REGISTRY_PROGRAM_ID @ TreasuryError::CompanyOwnerMismatch)]
    pub company: UncheckedAccount<'info>,

    #[account(
        mut,
        seeds = [b"treasury", company.key().as_ref()],
        bump = treasury.bump,
        constraint = treasury.company == company.key() @ TreasuryError::PolicyMismatch,
    )]
    pub treasury: Account<'info, TreasuryAccount>,

    #[account(
        mut,
        seeds = [b"policy", company.key().as_ref()],
        bump = policy.bump,
        constraint = policy.company == company.key() @ TreasuryError::PolicyMismatch,
    )]
    pub policy: Account<'info, PolicyAccount>,

    #[account(
        mut,
        seeds = [b"operations", company.key().as_ref(), &[OperationsKind::Disbursement.as_byte()]],
        bump = operations.bump,
        constraint = operations.company == company.key() @ TreasuryError::PolicyMismatch,
        constraint = operations.kind == OperationsKind::Disbursement @ TreasuryError::WrongOperationsKind,
        constraint = operations.signer == operations_signer.key() @ TreasuryError::UnauthorizedSigner,
    )]
    pub operations: Account<'info, OperationsAccount>,

    /// Disbursement Wallet — operator-held only. Pubkey verified via the
    /// `operations.signer == operations_signer.key()` constraint above.
    pub operations_signer: Signer<'info>,

    /// Registry-owned Deployment whose `receiving_address` must equal
    /// `destination.key()`. Field decoding done in handler.
    /// CHECK: ownership-program match; field-level checks in handler.
    #[account(owner = REGISTRY_PROGRAM_ID @ TreasuryError::CompanyOwnerMismatch)]
    pub deployment: UncheckedAccount<'info>,

    /// Net amount lands here. Marked `mut` so its lamports balance can be
    /// updated by direct manipulation (treasury PDA is owned by this program,
    /// not System, so we can't use SystemProgram::transfer).
    /// CHECK: matched against `deployment.receiving_address` in handler.
    #[account(mut)]
    pub destination: UncheckedAccount<'info>,

    #[account(
        mut,
        seeds = [b"protocol_fees"],
        bump = protocol_fee_account.bump,
    )]
    pub protocol_fee_account: Account<'info, ProtocolFeeAccount>,
}

#[derive(Accounts)]
pub struct DisburseDiscretionary<'info> {
    /// CHECK: ownership-program match.
    #[account(owner = REGISTRY_PROGRAM_ID @ TreasuryError::CompanyOwnerMismatch)]
    pub company: UncheckedAccount<'info>,

    pub controlling_authority: Signer<'info>,

    #[account(
        mut,
        seeds = [b"treasury", company.key().as_ref()],
        bump = treasury.bump,
        constraint = treasury.company == company.key() @ TreasuryError::PolicyMismatch,
    )]
    pub treasury: Account<'info, TreasuryAccount>,

    #[account(
        mut,
        seeds = [b"policy", company.key().as_ref()],
        bump = policy.bump,
        constraint = policy.company == company.key() @ TreasuryError::PolicyMismatch,
    )]
    pub policy: Account<'info, PolicyAccount>,

    /// CHECK: ownership-program match; field-level checks in handler.
    #[account(owner = REGISTRY_PROGRAM_ID @ TreasuryError::CompanyOwnerMismatch)]
    pub deployment: UncheckedAccount<'info>,

    /// CHECK: matched against `deployment.receiving_address` in handler.
    #[account(mut)]
    pub destination: UncheckedAccount<'info>,

    #[account(
        mut,
        seeds = [b"protocol_fees"],
        bump = protocol_fee_account.bump,
    )]
    pub protocol_fee_account: Account<'info, ProtocolFeeAccount>,
}

#[derive(Accounts)]
pub struct DisbursePrivileged<'info> {
    /// CHECK: ownership-program match.
    #[account(owner = REGISTRY_PROGRAM_ID @ TreasuryError::CompanyOwnerMismatch)]
    pub company: UncheckedAccount<'info>,

    pub controlling_authority: Signer<'info>,

    /// Required only when `amount > policy.privileged_threshold_lamports`
    /// AND a `secondary_signer` is registered. Pass `None` for below-threshold
    /// disbursements. Pubkey verified against `policy.secondary_signer` in handler.
    pub secondary_signer: Option<Signer<'info>>,

    #[account(
        mut,
        seeds = [b"treasury", company.key().as_ref()],
        bump = treasury.bump,
        constraint = treasury.company == company.key() @ TreasuryError::PolicyMismatch,
    )]
    pub treasury: Account<'info, TreasuryAccount>,

    #[account(
        seeds = [b"policy", company.key().as_ref()],
        bump = policy.bump,
        constraint = policy.company == company.key() @ TreasuryError::PolicyMismatch,
    )]
    pub policy: Account<'info, PolicyAccount>,

    /// Pass `Some(deployment_account)` when `is_agent_destination = true`.
    /// Pass `None` for external destinations.
    /// CHECK: ownership-program match; field-level checks in handler.
    #[account(owner = REGISTRY_PROGRAM_ID @ TreasuryError::CompanyOwnerMismatch)]
    pub deployment: Option<UncheckedAccount<'info>>,

    /// CHECK: unrestricted destination (privileged class). Verified against
    /// `deployment.receiving_address` only when `is_agent_destination = true`.
    #[account(mut)]
    pub destination: UncheckedAccount<'info>,

    #[account(
        mut,
        seeds = [b"protocol_fees"],
        bump = protocol_fee_account.bump,
    )]
    pub protocol_fee_account: Account<'info, ProtocolFeeAccount>,
}

// ─── Account schemas ───────────────────────────────────────────────────────

#[account]
#[derive(InitSpace)]
pub struct TreasuryAccount {
    pub version: u8,
    pub company: Pubkey,
    /// Token mints allow-listed for inflow/outflow accounting. SOL is
    /// represented as `SOL_PSEUDO_MINT` (lamports custodied directly on this
    /// PDA). SPL mints are custodied via per-mint Associated Token Accounts
    /// authored by this PDA.
    #[max_len(8)]
    pub accepted_assets: Vec<Pubkey>,
    pub bump: u8,
}

#[account]
#[derive(InitSpace)]
pub struct PolicyAccount {
    pub version: u8,
    pub company: Pubkey,

    // ── Per-asset, per-month spending caps (§3.2) ──
    /// Max disbursable per calendar month via Routine class (auto-signed by
    /// OperationsAccount[Disbursement]).
    #[max_len(8)]
    pub routine_budget_per_month: Vec<AssetBudget>,
    /// Max disbursable per calendar month via Discretionary class
    /// (operator-signed, e.g. ad-hoc agent payouts).
    #[max_len(8)]
    pub discretionary_budget_per_month: Vec<AssetBudget>,

    // ── Privileged-class secondary-signer config ──
    /// Privileged disbursements above this lamport amount require both
    /// controlling authority + secondary signer. Default `u64::MAX` (no
    /// threshold trip until operator opts in).
    pub privileged_threshold_lamports: u64,
    /// Per-SPL-token equivalent of `privileged_threshold_lamports`. Empty
    /// vec = no per-token threshold configured.
    #[max_len(8)]
    pub privileged_threshold_per_token: Vec<AssetBudget>,
    /// Secondary signer pubkey. `None` = unconfigured; any threshold-crossing
    /// privileged ix will reject until set.
    pub secondary_signer: Option<Pubkey>,

    // ── Protocol fee on intra-company agent disbursements (§11.1) ──
    /// Fee in basis points (300 = 3%). Mutable via `set_policy`. Routes to
    /// `ProtocolFeeAccount` when destination is a `Deployment.receiving_address`
    /// under the same company.
    pub agent_operating_fee_bps: u16,

    // ── Period accounting (§5 lazy rollover) ──
    /// Unix timestamp of start-of-current-month UTC. `0` = uninitialized;
    /// first disbursement after init lazy-rolls to current month start.
    pub current_period_anchor: i64,
    #[max_len(8)]
    pub routine_spent_this_period: Vec<AssetBudget>,
    #[max_len(8)]
    pub discretionary_spent_this_period: Vec<AssetBudget>,

    pub bump: u8,
}

#[account]
#[derive(InitSpace)]
pub struct OperationsAccount {
    pub version: u8,
    pub company: Pubkey,
    /// What kind of operations key this is (Disbursement | Anchor).
    /// Also encoded into the PDA seed so each company can host one of each.
    pub kind: OperationsKind,
    /// Pubkey of the registered wallet:
    ///   • Disbursement: operator-held only — OCCA never holds the privkey.
    ///   • Anchor: shared session key — operator + OCCA both hold privkey,
    ///     either can sign `commit_daily_anchor` (capability-scoped).
    pub signer: Pubkey,

    /// Whitelist of allowed instruction discriminators (8-byte Anchor
    /// discriminators). Defaults set at registration:
    ///   • Disbursement: `[disburse_routine]`
    ///   • Anchor:       `[commit_daily_anchor]` (lives in Registry program)
    #[max_len(8)]
    pub action_whitelist: Vec<[u8; 8]>,

    pub rate_limit_per_period: u32,
    pub signatures_this_period: u32,
    /// Unix timestamp of start-of-current-month UTC. `0` = uninitialized.
    pub current_period_anchor: i64,

    /// Hard expiry (unix timestamp). `0` = no expiry.
    pub expiry_unix: i64,
    /// Once true, all future signatures via this account are rejected.
    /// Rotation flow: revoke → close → re-create with new `signer`.
    pub revoked: bool,

    pub bump: u8,
}

#[account]
#[derive(InitSpace)]
pub struct ProtocolFeeAccount {
    pub version: u8,
    /// Long-lived withdrawal authority — typically a DAO / multisig key,
    /// distinct from the program's upgrade authority. Settable only at init.
    pub governance: Pubkey,
    /// Per-asset accumulated fee balances.
    #[max_len(8)]
    pub balances: Vec<AssetBudget>,
    pub bump: u8,
}

// ─── Shared types ──────────────────────────────────────────────────────────

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, PartialEq, Eq, InitSpace, Debug)]
pub struct AssetBudget {
    /// SPL mint pubkey — `SOL_PSEUDO_MINT` for SOL.
    pub mint: Pubkey,
    /// Amount in lamports (SOL) or base units (SPL).
    pub amount: u64,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, PartialEq, Eq, InitSpace, Debug)]
pub enum OperationsKind {
    /// Disbursement Wallet — operator-held only. OCCA never holds privkey.
    /// Signs `disburse_routine` (batched payouts).
    Disbursement,
    /// Anchor Wallet — shared session key (operator + OCCA both hold).
    /// Signs `commit_daily_anchor` only (lives in Registry program).
    Anchor,
}

impl OperationsKind {
    /// Single-byte representation for use in PDA seeds.
    /// Stable encoding — order MUST NOT change once any account exists.
    pub fn as_byte(&self) -> u8 {
        match self {
            OperationsKind::Disbursement => 0,
            OperationsKind::Anchor => 1,
        }
    }
}

/// Parameter bag for `set_policy`. Each field is `Option<T>`: `None` =
/// "leave unchanged", `Some(v)` = "set to v". `secondary_signer` is doubly
/// optional (outer = include in update; inner = new value, with `None` =
/// clear and `Some(pk)` = set).
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct SetPolicyParams {
    pub routine_budget_per_month: Option<Vec<AssetBudget>>,
    pub discretionary_budget_per_month: Option<Vec<AssetBudget>>,
    pub privileged_threshold_lamports: Option<u64>,
    pub privileged_threshold_per_token: Option<Vec<AssetBudget>>,
    pub secondary_signer: Option<Option<Pubkey>>,
    pub agent_operating_fee_bps: Option<u16>,
    pub accepted_assets: Option<Vec<Pubkey>>,
}

/// Parameter bag for `update_operations_capability`. `None` = leave field
/// unchanged. To set `expiry_unix` to "no expiry", pass `Some(0)` (sentinel).
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct UpdateOperationsCapabilityParams {
    pub action_whitelist: Option<Vec<[u8; 8]>>,
    pub rate_limit_per_period: Option<u32>,
    pub expiry_unix: Option<i64>,
}

// ─── Helpers ───────────────────────────────────────────────────────────────

/// Read `CompanyAccount.owner` from raw account data without depending on
/// the registry crate (avoids a Cargo cycle once registry → treasury CPI
/// dep lands in M5).
///
/// Layout assumed: `[8B Anchor disc][1B version][32B owner][...]`.
/// Currently `version == 3` (see `SUPPORTED_COMPANY_VERSION`). If Registry
/// reorders fields before `owner`, bump version + audit this fn together.
fn read_company_owner(company: &UncheckedAccount<'_>) -> Result<Pubkey> {
    let data = company.try_borrow_data()?;
    require!(
        data.len() >= COMPANY_OWNER_OFFSET + 32,
        TreasuryError::InvalidCompanyAccount
    );
    let version = data[8];
    require!(
        version == SUPPORTED_COMPANY_VERSION,
        TreasuryError::IncompatibleCompanyVersion
    );
    let owner_bytes: [u8; 32] = data[COMPANY_OWNER_OFFSET..COMPANY_OWNER_OFFSET + 32]
        .try_into()
        .map_err(|_| error!(TreasuryError::InvalidCompanyAccount))?;
    Ok(Pubkey::new_from_array(owner_bytes))
}

fn validate_budget_vec(b: &[AssetBudget]) -> Result<()> {
    require!(b.len() <= MAX_BUDGET_ENTRIES, TreasuryError::TooManyEntries);
    for i in 0..b.len() {
        for j in (i + 1)..b.len() {
            require!(b[i].mint != b[j].mint, TreasuryError::DuplicateMint);
        }
    }
    Ok(())
}

fn validate_accepted_assets(a: &[Pubkey]) -> Result<()> {
    require!(
        a.len() <= MAX_ACCEPTED_ASSETS,
        TreasuryError::TooManyEntries
    );
    for i in 0..a.len() {
        for j in (i + 1)..a.len() {
            require!(a[i] != a[j], TreasuryError::DuplicateMint);
        }
    }
    Ok(())
}

/// Subset of Registry's Deployment fields we care about for fee/destination
/// validation. Read manually to avoid Cargo cycle.
struct DeploymentInfo {
    company: Pubkey,
    receiving_address: Pubkey,
}

fn read_deployment(deployment: &UncheckedAccount<'_>) -> Result<DeploymentInfo> {
    let data = deployment.try_borrow_data()?;
    require!(
        data.len() >= DEPLOYMENT_RECEIVING_OFFSET + 32,
        TreasuryError::InvalidDeploymentAccount
    );
    let version = data[8];
    require!(
        version == SUPPORTED_DEPLOYMENT_VERSION,
        TreasuryError::IncompatibleDeploymentVersion
    );
    let company_bytes: [u8; 32] = data
        [DEPLOYMENT_COMPANY_OFFSET..DEPLOYMENT_COMPANY_OFFSET + 32]
        .try_into()
        .map_err(|_| error!(TreasuryError::InvalidDeploymentAccount))?;
    let receiving_bytes: [u8; 32] = data
        [DEPLOYMENT_RECEIVING_OFFSET..DEPLOYMENT_RECEIVING_OFFSET + 32]
        .try_into()
        .map_err(|_| error!(TreasuryError::InvalidDeploymentAccount))?;
    Ok(DeploymentInfo {
        company: Pubkey::new_from_array(company_bytes),
        receiving_address: Pubkey::new_from_array(receiving_bytes),
    })
}

/// Compute the Agent Operating Fee for a target amount.
/// Returns `(fee, gross)` where:
///   • `fee = amount * bps / 10_000` (rounded down)
///   • `gross = amount + fee` (deducted from treasury; net to recipient = amount)
///
/// Per design §11.1, the fee is on top of the recipient amount, not subtracted
/// from it. Budget checks compare `gross` against `routine_budget_per_month` /
/// `discretionary_budget_per_month` (§4.4 "remaining ≥ amount + fee").
fn compute_fee(amount: u64, bps: u16) -> Result<(u64, u64)> {
    if bps == 0 {
        return Ok((0, amount));
    }
    let fee = (amount as u128)
        .checked_mul(bps as u128)
        .ok_or(TreasuryError::ArithmeticOverflow)?
        / 10_000u128;
    let fee_u64: u64 = fee
        .try_into()
        .map_err(|_| error!(TreasuryError::ArithmeticOverflow))?;
    let gross = amount
        .checked_add(fee_u64)
        .ok_or(TreasuryError::ArithmeticOverflow)?;
    Ok((fee_u64, gross))
}

fn get_asset_amount(vec: &[AssetBudget], mint: Pubkey) -> u64 {
    vec.iter()
        .find(|e| e.mint == mint)
        .map(|e| e.amount)
        .unwrap_or(0)
}

fn upsert_asset_amount(
    vec: &mut Vec<AssetBudget>,
    mint: Pubkey,
    delta: u64,
) -> Result<()> {
    for entry in vec.iter_mut() {
        if entry.mint == mint {
            entry.amount = entry
                .amount
                .checked_add(delta)
                .ok_or(TreasuryError::ArithmeticOverflow)?;
            return Ok(());
        }
    }
    require!(
        vec.len() < MAX_BUDGET_ENTRIES,
        TreasuryError::TooManyEntries
    );
    vec.push(AssetBudget { mint, amount: delta });
    Ok(())
}

/// Check that `delta` fits inside the per-period budget for `mint`, then
/// increment the spent counter. Used by disburse_routine and disburse_discretionary.
/// `budget_amount` is pre-extracted by the caller (`get_asset_amount`) to avoid
/// holding two simultaneous borrows on the same PolicyAccount.
fn apply_spent(
    spent_vec: &mut Vec<AssetBudget>,
    budget_amount: u64,
    mint: Pubkey,
    delta: u64,
) -> Result<()> {
    require!(budget_amount > 0, TreasuryError::AssetNotBudgeted);
    let new_spent = get_asset_amount(spent_vec, mint)
        .checked_add(delta)
        .ok_or(TreasuryError::ArithmeticOverflow)?;
    require!(new_spent <= budget_amount, TreasuryError::BudgetExceeded);
    upsert_asset_amount(spent_vec, mint, delta)?;
    Ok(())
}

/// Move SOL lamports out of a PDA owned by this program. Treasury PDA must
/// retain its rent-exempt minimum or the runtime will GC it.
fn debit_treasury_lamports<'info>(
    treasury: &AccountInfo<'info>,
    amount: u64,
) -> Result<()> {
    let rent_exempt = Rent::get()?.minimum_balance(treasury.data_len());
    let current = treasury.lamports();
    require!(
        current >= rent_exempt + amount,
        TreasuryError::InsufficientFunds
    );
    **treasury.try_borrow_mut_lamports()? = current
        .checked_sub(amount)
        .ok_or(TreasuryError::ArithmeticOverflow)?;
    Ok(())
}

fn credit_lamports<'info>(target: &AccountInfo<'info>, amount: u64) -> Result<()> {
    let new_bal = target
        .lamports()
        .checked_add(amount)
        .ok_or(TreasuryError::ArithmeticOverflow)?;
    **target.try_borrow_mut_lamports()? = new_bal;
    Ok(())
}

// ─── Calendar arithmetic (§5) ──────────────────────────────────────────────
//
// Howard Hinnant's date algorithms — public domain, no external deps,
// fits Solana BPF. Source: https://howardhinnant.github.io/date_algorithms.html
//
// Used by lazy-rollover helpers below; called inside future disbursement
// instructions (M4) at the top of each handler:
//
//     rollover_policy_period(&mut policy, Clock::get()?.unix_timestamp);

pub mod calendar {
    /// Days since Unix epoch (1970-01-01) → (year, month [1-12], day [1-31]).
    pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
        let z = z + 719_468;
        let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
        let doe = (z - era * 146_097) as u64; // 0..=146_096
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let y_pre = (yoe as i64) + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
        let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
        let y = y_pre + if m <= 2 { 1 } else { 0 };
        (y, m, d)
    }

    /// (year, month [1-12], day [1-31]) → days since Unix epoch.
    pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
        let y = y - if m <= 2 { 1 } else { 0 };
        let era = if y >= 0 { y } else { y - 399 } / 400;
        let yoe = (y - era * 400) as u64; // 0..=399
        let m_term = if m > 2 { (m - 3) as u64 } else { (m + 9) as u64 };
        let doy = (153 * m_term + 2) / 5 + d as u64 - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        era * 146_097 + doe as i64 - 719_468
    }

    /// Unix timestamp of `00:00:00 UTC on the 1st` of the calendar month
    /// containing `now`.
    pub fn current_month_start_unix(now: i64) -> i64 {
        let days = now.div_euclid(86_400);
        let (year, month, _day) = civil_from_days(days);
        days_from_civil(year, month, 1) * 86_400
    }
}

// ─── Lazy rollover helpers (§5) ────────────────────────────────────────────
//
// Called from disbursement instructions before applying the new amount.

fn rollover_policy_period(policy: &mut PolicyAccount, now: i64) {
    let month_start = calendar::current_month_start_unix(now);
    if policy.current_period_anchor != month_start {
        policy.current_period_anchor = month_start;
        policy.routine_spent_this_period.clear();
        policy.discretionary_spent_this_period.clear();
    }
}

fn rollover_operations_period(ops: &mut OperationsAccount, now: i64) {
    let month_start = calendar::current_month_start_unix(now);
    if ops.current_period_anchor != month_start {
        ops.current_period_anchor = month_start;
        ops.signatures_this_period = 0;
    }
}

// ─── Errors ────────────────────────────────────────────────────────────────

#[error_code]
pub enum TreasuryError {
    #[msg("company account is not owned by the Registry program")]
    CompanyOwnerMismatch,
    #[msg("company account data is malformed or too short")]
    InvalidCompanyAccount,
    #[msg("company account version is not supported by this program")]
    IncompatibleCompanyVersion,
    #[msg("ProgramData account does not match this program")]
    InvalidProgramData,
    #[msg("signer is not the program's upgrade authority")]
    NotUpgradeAuthority,
    #[msg("signer is not authorized for this instruction")]
    UnauthorizedSigner,
    #[msg("policy/treasury does not match company")]
    PolicyMismatch,
    #[msg("vector exceeds maximum length")]
    TooManyEntries,
    #[msg("duplicate mint in vector")]
    DuplicateMint,
    #[msg("fee bps exceeds 10000 (100%)")]
    InvalidFeeBps,
    #[msg("operations account signer cannot be the default pubkey")]
    InvalidSigner,
    #[msg("operations account requires at least one whitelisted action")]
    EmptyWhitelist,
    #[msg("expiry timestamp must be in the future or zero")]
    ExpiryInPast,
    #[msg("operations account is revoked")]
    OperationsRevoked,
    #[msg("operations account is already revoked")]
    AlreadyRevoked,
    #[msg("operations account must be revoked before closing")]
    NotRevoked,
    #[msg("operations account kind does not match instruction expectation")]
    WrongOperationsKind,
    #[msg("operations account is past its expiry")]
    OperationsExpired,
    #[msg("instruction discriminator is not whitelisted on operations account")]
    DiscriminatorNotWhitelisted,
    #[msg("rate limit for operations account is exhausted this period")]
    RateLimitExceeded,
    #[msg("disbursement amount must be greater than zero")]
    ZeroAmount,
    #[msg("SPL token disbursement is not yet supported (Phase 1 SOL only)")]
    SplNotSupported,
    #[msg("mint is not in treasury accepted_assets list")]
    AssetNotAllowListed,
    #[msg("no budget configured for this asset")]
    AssetNotBudgeted,
    #[msg("budget remaining is insufficient for this disbursement")]
    BudgetExceeded,
    #[msg("treasury balance is insufficient (rent-exempt minimum protected)")]
    InsufficientFunds,
    #[msg("arithmetic overflow")]
    ArithmeticOverflow,
    #[msg("deployment account data is malformed or too short")]
    InvalidDeploymentAccount,
    #[msg("deployment account version is not supported by this program")]
    IncompatibleDeploymentVersion,
    #[msg("deployment does not belong to this company")]
    DeploymentCompanyMismatch,
    #[msg("destination does not match deployment.receiving_address")]
    DestinationMismatch,
    #[msg("deployment.receiving_address has not been set")]
    ReceivingAddressUnset,
    #[msg("disbursement above threshold requires secondary signer")]
    SecondarySignerRequired,
    #[msg("provided secondary signer does not match policy.secondary_signer")]
    SecondarySignerMismatch,
    #[msg("agent destination flag set but no deployment account provided")]
    MissingDeploymentForAgentDestination,
}

// ─── Unit tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use super::calendar::*;

    // ─── Calendar arithmetic ──────────────────────────────────────────────

    #[test]
    fn epoch_round_trip() {
        // 1970-01-01 == day 0
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(days_from_civil(1970, 1, 1), 0);
    }

    #[test]
    fn known_dates() {
        // 2026-05-08 — today (per session context)
        let d = days_from_civil(2026, 5, 8);
        assert_eq!(civil_from_days(d), (2026, 5, 8));

        // 2000-02-29 — leap day
        assert_eq!(civil_from_days(days_from_civil(2000, 2, 29)), (2000, 2, 29));

        // 2100-02-28 — NOT a leap year (divisible by 100, not 400)
        let next = days_from_civil(2100, 2, 28) + 1;
        assert_eq!(civil_from_days(next), (2100, 3, 1));
    }

    #[test]
    fn month_start_floors_to_first_of_month_utc() {
        let ts = days_from_civil(2026, 5, 8) * 86_400 + 12 * 3600 + 34 * 60 + 56;
        let start = current_month_start_unix(ts);
        assert_eq!(civil_from_days(start / 86_400), (2026, 5, 1));
        assert_eq!(start % 86_400, 0);
    }

    #[test]
    fn month_start_idempotent() {
        let ts = days_from_civil(2026, 5, 1) * 86_400; // exactly month start
        assert_eq!(current_month_start_unix(ts), ts);
    }

    // ─── compute_fee ──────────────────────────────────────────────────────

    #[test]
    fn fee_zero_bps_returns_amount_unchanged() {
        let (fee, gross) = compute_fee(1_000_000, 0).unwrap();
        assert_eq!(fee, 0);
        assert_eq!(gross, 1_000_000);
    }

    #[test]
    fn fee_default_3pct_correct() {
        // 3% of 1 SOL (1_000_000_000 lamports) = 30_000_000
        let (fee, gross) = compute_fee(1_000_000_000, 300).unwrap();
        assert_eq!(fee, 30_000_000);
        assert_eq!(gross, 1_030_000_000);
    }

    #[test]
    fn fee_max_bps_doubles_amount() {
        // 100% (10_000 bps) — pathological but valid
        let (fee, gross) = compute_fee(500, 10_000).unwrap();
        assert_eq!(fee, 500);
        assert_eq!(gross, 1_000);
    }

    #[test]
    fn fee_rounds_down() {
        // 1 lamport @ 300 bps — fee = 1*300/10000 = 0 (rounded down)
        let (fee, gross) = compute_fee(1, 300).unwrap();
        assert_eq!(fee, 0);
        assert_eq!(gross, 1);
    }

    #[test]
    fn fee_overflow_on_huge_gross() {
        // amount near u64::MAX + any positive fee → gross overflow
        let amt = u64::MAX - 100;
        let res = compute_fee(amt, 300);
        assert!(res.is_err(), "expected overflow but got {:?}", res);
    }

    // ─── get_asset_amount + upsert_asset_amount ──────────────────────────

    #[test]
    fn get_asset_amount_found_and_not_found() {
        let mint_a = Pubkey::new_unique();
        let mint_b = Pubkey::new_unique();
        let v = vec![
            AssetBudget { mint: mint_a, amount: 42 },
            AssetBudget { mint: mint_b, amount: 7 },
        ];
        assert_eq!(get_asset_amount(&v, mint_a), 42);
        assert_eq!(get_asset_amount(&v, mint_b), 7);
        assert_eq!(get_asset_amount(&v, Pubkey::new_unique()), 0);
        assert_eq!(get_asset_amount(&[], mint_a), 0);
    }

    #[test]
    fn upsert_inserts_new_entry() {
        let mint = Pubkey::new_unique();
        let mut v: Vec<AssetBudget> = vec![];
        upsert_asset_amount(&mut v, mint, 100).unwrap();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].mint, mint);
        assert_eq!(v[0].amount, 100);
    }

    #[test]
    fn upsert_accumulates_existing_entry() {
        let mint = Pubkey::new_unique();
        let mut v = vec![AssetBudget { mint, amount: 100 }];
        upsert_asset_amount(&mut v, mint, 50).unwrap();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].amount, 150);
    }

    #[test]
    fn upsert_rejects_when_max_entries() {
        let mut v: Vec<AssetBudget> = (0..MAX_BUDGET_ENTRIES)
            .map(|_| AssetBudget {
                mint: Pubkey::new_unique(),
                amount: 1,
            })
            .collect();
        // upsert with new mint → would exceed cap
        let new_mint = Pubkey::new_unique();
        assert!(upsert_asset_amount(&mut v, new_mint, 1).is_err());
        // but accumulating into an existing entry still works
        let existing_mint = v[0].mint;
        assert!(upsert_asset_amount(&mut v, existing_mint, 1).is_ok());
        assert_eq!(v[0].amount, 2);
    }

    #[test]
    fn upsert_overflow_rejected() {
        let mint = Pubkey::new_unique();
        let mut v = vec![AssetBudget { mint, amount: u64::MAX }];
        assert!(upsert_asset_amount(&mut v, mint, 1).is_err());
    }

    // ─── apply_spent ──────────────────────────────────────────────────────

    #[test]
    fn apply_spent_within_budget() {
        let mint = Pubkey::new_unique();
        let mut spent = vec![];
        apply_spent(&mut spent, 1_000, mint, 300).unwrap();
        apply_spent(&mut spent, 1_000, mint, 200).unwrap();
        assert_eq!(get_asset_amount(&spent, mint), 500);
    }

    #[test]
    fn apply_spent_exceeds_budget_rejected() {
        let mint = Pubkey::new_unique();
        let mut spent = vec![AssetBudget { mint, amount: 800 }];
        // spending 300 more would push to 1100 > 1000
        assert!(apply_spent(&mut spent, 1_000, mint, 300).is_err());
        // unchanged
        assert_eq!(get_asset_amount(&spent, mint), 800);
    }

    #[test]
    fn apply_spent_no_budget_rejected() {
        let mint = Pubkey::new_unique();
        let mut spent = vec![];
        assert!(apply_spent(&mut spent, 0, mint, 1).is_err());
    }

    #[test]
    fn apply_spent_exact_budget_allowed() {
        let mint = Pubkey::new_unique();
        let mut spent = vec![];
        apply_spent(&mut spent, 1_000, mint, 1_000).unwrap();
        // next spend of any amount must fail
        assert!(apply_spent(&mut spent, 1_000, mint, 1).is_err());
    }

    // ─── validate_budget_vec / validate_accepted_assets ──────────────────

    #[test]
    fn validate_budget_vec_accepts_unique() {
        let v = vec![
            AssetBudget { mint: Pubkey::new_unique(), amount: 1 },
            AssetBudget { mint: Pubkey::new_unique(), amount: 2 },
        ];
        assert!(validate_budget_vec(&v).is_ok());
    }

    #[test]
    fn validate_budget_vec_rejects_duplicate_mint() {
        let mint = Pubkey::new_unique();
        let v = vec![
            AssetBudget { mint, amount: 1 },
            AssetBudget { mint, amount: 2 },
        ];
        assert!(validate_budget_vec(&v).is_err());
    }

    #[test]
    fn validate_budget_vec_rejects_too_long() {
        let v: Vec<AssetBudget> = (0..MAX_BUDGET_ENTRIES + 1)
            .map(|i| AssetBudget {
                mint: Pubkey::new_from_array([i as u8; 32]),
                amount: 1,
            })
            .collect();
        assert!(validate_budget_vec(&v).is_err());
    }

    #[test]
    fn validate_accepted_assets_rejects_duplicate() {
        let m = Pubkey::new_unique();
        assert!(validate_accepted_assets(&[m, m]).is_err());
    }

    #[test]
    fn validate_accepted_assets_accepts_sol_pseudo_only() {
        // Default-init treasury config — single SOL_PSEUDO_MINT entry.
        assert!(validate_accepted_assets(&[SOL_PSEUDO_MINT]).is_ok());
    }

    // ─── Lazy rollover (PolicyAccount + OperationsAccount) ───────────────

    fn make_policy(period_anchor: i64) -> PolicyAccount {
        PolicyAccount {
            version: POLICY_ACCOUNT_VERSION,
            company: Pubkey::default(),
            routine_budget_per_month: vec![],
            discretionary_budget_per_month: vec![],
            privileged_threshold_lamports: u64::MAX,
            privileged_threshold_per_token: vec![],
            secondary_signer: None,
            agent_operating_fee_bps: DEFAULT_AGENT_OPERATING_FEE_BPS,
            current_period_anchor: period_anchor,
            routine_spent_this_period: vec![AssetBudget {
                mint: SOL_PSEUDO_MINT,
                amount: 1_000,
            }],
            discretionary_spent_this_period: vec![AssetBudget {
                mint: SOL_PSEUDO_MINT,
                amount: 500,
            }],
            bump: 0,
        }
    }

    fn make_operations(period_anchor: i64, signatures: u32) -> OperationsAccount {
        OperationsAccount {
            version: OPERATIONS_ACCOUNT_VERSION,
            company: Pubkey::default(),
            kind: OperationsKind::Disbursement,
            signer: Pubkey::new_unique(),
            action_whitelist: vec![[0u8; 8]],
            rate_limit_per_period: 100,
            signatures_this_period: signatures,
            current_period_anchor: period_anchor,
            expiry_unix: 0,
            revoked: false,
            bump: 0,
        }
    }

    #[test]
    fn rollover_policy_resets_when_month_changes() {
        let may_first = days_from_civil(2026, 5, 1) * 86_400;
        let jun_first = days_from_civil(2026, 6, 1) * 86_400;
        let mut p = make_policy(may_first);

        // Now is mid-June → rollover should reset spent counters.
        let now = jun_first + 12 * 3600;
        rollover_policy_period(&mut p, now);

        assert_eq!(p.current_period_anchor, jun_first);
        assert!(p.routine_spent_this_period.is_empty());
        assert!(p.discretionary_spent_this_period.is_empty());
    }

    #[test]
    fn rollover_policy_preserves_within_same_month() {
        let may_first = days_from_civil(2026, 5, 1) * 86_400;
        let mut p = make_policy(may_first);

        let now = may_first + 15 * 86_400 + 7 * 3600; // mid-May
        rollover_policy_period(&mut p, now);

        assert_eq!(p.current_period_anchor, may_first);
        // Counters preserved.
        assert_eq!(p.routine_spent_this_period.len(), 1);
        assert_eq!(p.routine_spent_this_period[0].amount, 1_000);
    }

    #[test]
    fn rollover_policy_initializes_from_zero_anchor() {
        // Fresh-init policy has `current_period_anchor = 0`. First disburse
        // call must lazy-init the anchor without losing the zero-state
        // counters (which are empty anyway).
        let mut p = make_policy(0);
        p.routine_spent_this_period.clear();
        p.discretionary_spent_this_period.clear();

        let now = days_from_civil(2026, 5, 8) * 86_400 + 3600;
        let expected = days_from_civil(2026, 5, 1) * 86_400;
        rollover_policy_period(&mut p, now);

        assert_eq!(p.current_period_anchor, expected);
    }

    #[test]
    fn rollover_operations_resets_signature_counter() {
        let may_first = days_from_civil(2026, 5, 1) * 86_400;
        let jun_first = days_from_civil(2026, 6, 1) * 86_400;
        let mut o = make_operations(may_first, 42);

        rollover_operations_period(&mut o, jun_first + 100);

        assert_eq!(o.current_period_anchor, jun_first);
        assert_eq!(o.signatures_this_period, 0);
    }

    #[test]
    fn rollover_operations_preserves_within_same_month() {
        let may_first = days_from_civil(2026, 5, 1) * 86_400;
        let mut o = make_operations(may_first, 42);

        rollover_operations_period(&mut o, may_first + 15 * 86_400);

        assert_eq!(o.current_period_anchor, may_first);
        assert_eq!(o.signatures_this_period, 42);
    }

    // ─── OperationsKind seed encoding ────────────────────────────────────

    #[test]
    fn operations_kind_byte_encoding_stable() {
        // Stability invariant — these values are baked into PDA seeds.
        // Changing them would invalidate every existing OperationsAccount.
        assert_eq!(OperationsKind::Disbursement.as_byte(), 0);
        assert_eq!(OperationsKind::Anchor.as_byte(), 1);
    }
}
