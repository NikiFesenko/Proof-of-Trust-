use anchor_lang::prelude::*;
use anchor_spl::token::{self, Token, TokenAccount, Transfer};

declare_id!("2bpMKRggT1V2XExN2dXjjDhf67bBxZDnCnGNLqaVPWYw");

const FEE_BPS: u64 = 100; 
const BPS_DENOM: u64 = 10_000;


#[program]
pub mod trustless_handshake {
    use super::*;

    /// Buyer initialises the escrow and locks USDC into a PDA vault.
    ///
    /// Compatible with a Solana Pay QR-code flow: the Seller encodes
    /// `amount`, `deal_id`, and their pubkey into the QR URL, the Buyer
    /// scans and signs this transaction to lock exactly that amount.
    pub fn initialize_escrow(
        ctx: Context<InitializeEscrow>,
        deal_id: u64,
        amount: u64,
        timeout_seconds: i64,
        mediator: Option<Pubkey>,
    ) -> Result<()> {
        require!(amount > 0, EscrowError::ZeroAmount);

        let escrow = &mut ctx.accounts.escrow_account;
        escrow.buyer = ctx.accounts.buyer.key();
        escrow.seller = ctx.accounts.seller.key();
        escrow.mint = ctx.accounts.mint.key();
        escrow.vault = ctx.accounts.vault.key();
        escrow.amount = amount;
        escrow.deal_id = deal_id;
        escrow.mediator = mediator;
        escrow.state = EscrowState::Active;
        escrow.created_at = Clock::get()?.unix_timestamp;
        escrow.timeout_at = Clock::get()?.unix_timestamp + timeout_seconds;
        escrow.bump = ctx.bumps.escrow_account;
        escrow.vault_bump = ctx.bumps.vault;

        // Transfer USDC from buyer's ATA into the PDA vault
        let cpi_ctx = CpiContext::new(
            ctx.accounts.token_program.to_account_info(),
            Transfer {
                from: ctx.accounts.buyer_token_account.to_account_info(),
                to: ctx.accounts.vault.to_account_info(),
                authority: ctx.accounts.buyer.to_account_info(),
            },
        );
        token::transfer(cpi_ctx, amount)?;

        emit!(ClockedIn {
            deal_id,
            buyer: escrow.buyer,
            seller: escrow.seller,
            amount,
            timeout_at: escrow.timeout_at,
        });

        Ok(())
    }

    /// Seller calls this to indicate the item has been shipped/delivered.
    /// Transitions state from Active → Shipped and records the timestamp.
    pub fn mark_shipped(ctx: Context<MarkShipped>) -> Result<()> {
        let escrow = &mut ctx.accounts.escrow_account;
        require!(
            escrow.state == EscrowState::Active,
            EscrowError::InvalidState
        );
        escrow.state = EscrowState::Shipped;
        escrow.shipped_at = Some(Clock::get()?.unix_timestamp);
        Ok(())
    }

    /// Buyer confirms receipt of the goods.
    /// Releases 99% to Seller and 1% to Treasury.
    pub fn confirm_receipt(ctx: Context<ConfirmReceipt>) -> Result<()> {
        let escrow = &mut ctx.accounts.escrow_account;
        require!(
            escrow.state == EscrowState::Shipped,
            EscrowError::InvalidState
        );

        let deal_id = escrow.deal_id;
        let buyer = escrow.buyer;
        let amount = escrow.amount;

        let fee = amount.checked_mul(FEE_BPS).unwrap().checked_div(BPS_DENOM).unwrap();
        let seller_amount = amount.checked_sub(fee).unwrap();

        // PDA signer seeds
        let seeds: &[&[u8]] = &[
            b"escrow",
            buyer.as_ref(),
            &deal_id.to_le_bytes(),
            &[escrow.vault_bump],
        ];
        let signer = &[seeds];

        // Release to seller
        token::transfer(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.to_account_info(),
                Transfer {
                    from: ctx.accounts.vault.to_account_info(),
                    to: ctx.accounts.seller_token_account.to_account_info(),
                    authority: ctx.accounts.vault.to_account_info(),
                },
                signer,
            ),
            seller_amount,
        )?;

        // Transfer fee to treasury
        token::transfer(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.to_account_info(),
                Transfer {
                    from: ctx.accounts.vault.to_account_info(),
                    to: ctx.accounts.treasury_token_account.to_account_info(),
                    authority: ctx.accounts.vault.to_account_info(),
                },
                signer,
            ),
            fee,
        )?;

        escrow.state = EscrowState::Released;

        emit!(Released {
            deal_id,
            recipient: escrow.seller,
            amount: seller_amount,
            fee,
        });

        Ok(())
    }

    /// Buyer or Seller can raise a dispute while state is Shipped.
    /// This transitions state to Disputed and notifies the frontend via event.
    /// The Mediator (if set) can then vote using resolve_dispute.
    pub fn trigger_dispute(ctx: Context<TriggerDispute>) -> Result<()> {
        let escrow = &mut ctx.accounts.escrow_account;
        require!(
            escrow.state == EscrowState::Shipped,
            EscrowError::InvalidState
        );
        require!(escrow.mediator.is_some(), EscrowError::NoMediatorSet);

        escrow.state = EscrowState::Disputed;

        emit!(Disputed {
            deal_id: escrow.deal_id,
            raised_by: ctx.accounts.raiser.key(),
            mediator: escrow.mediator.unwrap(),
        });

        Ok(())
    }

    /// Mediator resolves a dispute by voting to release funds to either party.
    ///
    /// Anti-collusion design:
    ///   - The mediator is set at init time and cannot be changed.
    ///   - The mediator has NO stake in the deal (no fee if they favour the seller,
    ///     no refund if they favour the buyer).
    ///   - All mediator votes are recorded on-chain via the Released/Refunded events,
    ///     creating a permanent, public reputation trail.
    ///   - A platform should rate-limit mediator keys and flag unusual patterns
    ///     (e.g., always favouring one party) via the event stream.
    pub fn resolve_dispute(
        ctx: Context<ResolveDispute>,
        favour_seller: bool,
    ) -> Result<()> {
        let escrow = &mut ctx.accounts.escrow_account;
        require!(
            escrow.state == EscrowState::Disputed,
            EscrowError::InvalidState
        );

        let deal_id = escrow.deal_id;
        let buyer = escrow.buyer;
        let amount = escrow.amount;

        let seeds: &[&[u8]] = &[
            b"escrow",
            buyer.as_ref(),
            &deal_id.to_le_bytes(),
            &[escrow.vault_bump],
        ];
        let signer = &[seeds];

        if favour_seller {
            let fee = amount.checked_mul(FEE_BPS).unwrap().checked_div(BPS_DENOM).unwrap();
            let seller_amount = amount.checked_sub(fee).unwrap();

            token::transfer(
                CpiContext::new_with_signer(
                    ctx.accounts.token_program.to_account_info(),
                    Transfer {
                        from: ctx.accounts.vault.to_account_info(),
                        to: ctx.accounts.seller_token_account.to_account_info(),
                        authority: ctx.accounts.vault.to_account_info(),
                    },
                    signer,
                ),
                seller_amount,
            )?;

            token::transfer(
                CpiContext::new_with_signer(
                    ctx.accounts.token_program.to_account_info(),
                    Transfer {
                        from: ctx.accounts.vault.to_account_info(),
                        to: ctx.accounts.treasury_token_account.to_account_info(),
                        authority: ctx.accounts.vault.to_account_info(),
                    },
                    signer,
                ),
                fee,
            )?;

            escrow.state = EscrowState::Released;
            emit!(Released {
                deal_id,
                recipient: escrow.seller,
                amount: seller_amount,
                fee,
            });
        } else {
            // Full refund to buyer, no platform fee
            token::transfer(
                CpiContext::new_with_signer(
                    ctx.accounts.token_program.to_account_info(),
                    Transfer {
                        from: ctx.accounts.vault.to_account_info(),
                        to: ctx.accounts.buyer_token_account.to_account_info(),
                        authority: ctx.accounts.vault.to_account_info(),
                    },
                    signer,
                ),
                amount,
            )?;

            escrow.state = EscrowState::Refunded;
            emit!(Released {
                deal_id,
                recipient: escrow.buyer,
                amount,
                fee: 0,
            });
        }

        Ok(())
    }

    /// Safety Switch: if the Seller never calls mark_shipped within `timeout_at`,
    /// the Buyer can call this to recover all funds. No platform fee on timeout refunds.
    pub fn refund_timeout(ctx: Context<RefundTimeout>) -> Result<()> {
        let escrow = &mut ctx.accounts.escrow_account;
        require!(
            escrow.state == EscrowState::Active,
            EscrowError::InvalidState
        );
        require!(
            Clock::get()?.unix_timestamp >= escrow.timeout_at,
            EscrowError::TimeoutNotReached
        );

        let deal_id = escrow.deal_id;
        let buyer = escrow.buyer;
        let amount = escrow.amount;

        let seeds: &[&[u8]] = &[
            b"escrow",
            buyer.as_ref(),
            &deal_id.to_le_bytes(),
            &[escrow.vault_bump],
        ];
        let signer = &[seeds];

        token::transfer(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.to_account_info(),
                Transfer {
                    from: ctx.accounts.vault.to_account_info(),
                    to: ctx.accounts.buyer_token_account.to_account_info(),
                    authority: ctx.accounts.vault.to_account_info(),
                },
                signer,
            ),
            amount,
        )?;

        escrow.state = EscrowState::Refunded;

        emit!(Released {
            deal_id,
            recipient: buyer,
            amount,
            fee: 0,
        });

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------
#[account]
pub struct EscrowAccount {
    pub buyer: Pubkey,         // 32
    pub seller: Pubkey,        // 32
    pub mint: Pubkey,          // 32  — USDC mint
    pub vault: Pubkey,         // 32  — PDA token account holding funds
    pub mediator: Option<Pubkey>, // 33 (1 discriminant + 32)
    pub amount: u64,           // 8
    pub deal_id: u64,          // 8
    pub state: EscrowState,    // 1
    pub created_at: i64,       // 8
    pub timeout_at: i64,       // 8
    pub shipped_at: Option<i64>, // 9
    pub bump: u8,              // 1
    pub vault_bump: u8,        // 1
}

impl EscrowAccount {
    // 8 (discriminator) + 32*4 + 33 + 8*2 + 1 + 8*2 + 9 + 1 + 1
    pub const LEN: usize = 8 + 128 + 33 + 16 + 1 + 16 + 9 + 2;
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, PartialEq, Eq)]
pub enum EscrowState {
    Active,
    Shipped,
    Disputed,
    Released,
    Refunded,
}

// ---------------------------------------------------------------------------
// Instruction Accounts
// ---------------------------------------------------------------------------

#[derive(Accounts)]
#[instruction(deal_id: u64, amount: u64, timeout_seconds: i64, mediator: Option<Pubkey>)]
pub struct InitializeEscrow<'info> {
    #[account(mut)]
    pub buyer: Signer<'info>,

    /// CHECK: validated by constraint below
    #[account(
        constraint = seller.key() != buyer.key() @ EscrowError::BuyerIsSeller
    )]
    pub seller: AccountInfo<'info>,

    #[account(
        init,
        payer = buyer,
        space = EscrowAccount::LEN,
        seeds = [b"escrow", buyer.key().as_ref(), &deal_id.to_le_bytes()],
        bump
    )]
    pub escrow_account: Account<'info, EscrowAccount>,

    /// PDA token vault — holds USDC during the escrow period
    #[account(
        init,
        payer = buyer,
        token::mint = mint,
        token::authority = vault,
        seeds = [b"vault", buyer.key().as_ref(), &deal_id.to_le_bytes()],
        bump
    )]
    pub vault: Account<'info, TokenAccount>,

    #[account(mut)]
    pub buyer_token_account: Account<'info, TokenAccount>,

    /// CHECK: just the mint account, validated by SPL token constraints
    pub mint: AccountInfo<'info>,

    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
    pub rent: Sysvar<'info, Rent>,
}

#[derive(Accounts)]
pub struct MarkShipped<'info> {
    #[account(
        mut,
        has_one = seller @ EscrowError::Unauthorized,
    )]
    pub escrow_account: Account<'info, EscrowAccount>,
    pub seller: Signer<'info>,
}

#[derive(Accounts)]
pub struct ConfirmReceipt<'info> {
    #[account(
        mut,
        has_one = buyer @ EscrowError::Unauthorized,
        has_one = vault,
    )]
    pub escrow_account: Account<'info, EscrowAccount>,
    pub buyer: Signer<'info>,

    #[account(mut)]
    pub vault: Account<'info, TokenAccount>,

    #[account(mut)]
    pub seller_token_account: Account<'info, TokenAccount>,

    #[account(mut)]
    pub treasury_token_account: Account<'info, TokenAccount>,

    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct TriggerDispute<'info> {
    #[account(
        mut,
        constraint = (
            raiser.key() == escrow_account.buyer ||
            raiser.key() == escrow_account.seller
        ) @ EscrowError::Unauthorized
    )]
    pub escrow_account: Account<'info, EscrowAccount>,
    pub raiser: Signer<'info>,
}

#[derive(Accounts)]
pub struct ResolveDispute<'info> {
    #[account(
        mut,
        has_one = vault,
        constraint = escrow_account.mediator == Some(mediator.key()) @ EscrowError::Unauthorized
    )]
    pub escrow_account: Account<'info, EscrowAccount>,
    pub mediator: Signer<'info>,

    #[account(mut)]
    pub vault: Account<'info, TokenAccount>,

    #[account(mut)]
    pub seller_token_account: Account<'info, TokenAccount>,

    #[account(mut)]
    pub buyer_token_account: Account<'info, TokenAccount>,

    #[account(mut)]
    pub treasury_token_account: Account<'info, TokenAccount>,

    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct RefundTimeout<'info> {
    #[account(
        mut,
        has_one = buyer @ EscrowError::Unauthorized,
        has_one = vault,
    )]
    pub escrow_account: Account<'info, EscrowAccount>,
    pub buyer: Signer<'info>,

    #[account(mut)]
    pub vault: Account<'info, TokenAccount>,

    #[account(mut)]
    pub buyer_token_account: Account<'info, TokenAccount>,

    pub token_program: Program<'info, Token>,
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

#[event]
pub struct ClockedIn {
    pub deal_id: u64,
    pub buyer: Pubkey,
    pub seller: Pubkey,
    pub amount: u64,
    pub timeout_at: i64,
}

#[event]
pub struct Disputed {
    pub deal_id: u64,
    pub raised_by: Pubkey,
    pub mediator: Pubkey,
}

#[event]
pub struct Released {
    pub deal_id: u64,
    pub recipient: Pubkey,
    pub amount: u64,
    pub fee: u64,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[error_code]
pub enum EscrowError {
    #[msg("Amount must be greater than zero")]
    ZeroAmount,
    #[msg("Buyer and seller cannot be the same account")]
    BuyerIsSeller,
    #[msg("Escrow is not in the required state for this instruction")]
    InvalidState,
    #[msg("Signer is not authorised to call this instruction")]
    Unauthorized,
    #[msg("Timeout has not been reached yet")]
    TimeoutNotReached,
    #[msg("No mediator was set for this escrow")]
    NoMediatorSet,
}