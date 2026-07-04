//! SwiftRamp swap contract (Soroban / Stellar)
//!
//! WHAT THIS CONTRACT ACTUALLY DOES:
//! - Lets an admin publish exchange rates between currency codes (matching the
//!   fixed rate table in the frontend: NGN, KES, GHS, ZAR, USD, EUR, GBP).
//! - Lets a sender swap `amount` of a `from_token` for the equivalent amount of
//!   a `to_token`, at the current published rate, with slippage protection.
//! - Requires real authorization (`sender.require_auth()`) and moves real
//!   tokens via the standard Soroban token interface.
//!
//! WHAT THIS CONTRACT DOES NOT DO (yet):
//! - It does NOT hide the transfer amount on-chain. Token transfer events on
//!   Soroban/Stellar are public by design — that's true of any contract using
//!   the standard token interface, not a bug in this code.
//! - The `commitment` parameter is a hash you can compute off-chain (e.g.
//!   from a Pedersen commitment to the real amount) and it gets stored and
//!   emitted alongside the swap. Right now it's inert — a placeholder so a
//!   real shielded-pool / zk-SNARK verifier can be slotted in later without
//!   changing the public interface. Implementing that verifier (circuit,
//!   trusted setup or a transparent proving system, on-chain verification)
//!   is a separate, much larger project.

#![no_std]
#![allow(clippy::too_many_arguments)]
use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, BytesN, Env,
    Symbol,
};

pub const RATE_SCALE: i128 = 10_000_000;

#[derive(Clone)]
#[contracttype]
pub enum DataKey {
    Admin,
    Rate(Symbol),
    LiquidityToken(Symbol),
    Commitment(BytesN<32>),
}

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum SwapError {
    NotInitialized = 1,
    AlreadyInitialized = 2,
    NotAdmin = 3,
    UnknownCurrency = 4,
    SlippageExceeded = 5,
    ZeroAmount = 6,
    CommitmentAlreadyUsed = 7,
}

#[contract]
pub struct SwiftRampSwap;

#[contractimpl]
impl SwiftRampSwap {
    pub fn initialize(env: Env, admin: Address) -> Result<(), SwapError> {
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(SwapError::AlreadyInitialized);
        }
        admin.require_auth();
        env.storage().instance().set(&DataKey::Admin, &admin);
        Ok(())
    }

    pub fn set_currency_token(
        env: Env,
        admin: Address,
        currency: Symbol,
        token_address: Address,
    ) -> Result<(), SwapError> {
        Self::require_admin(&env, &admin)?;
        env.storage()
            .instance()
            .set(&DataKey::LiquidityToken(currency), &token_address);
        Ok(())
    }

    pub fn set_rate(
        env: Env,
        admin: Address,
        currency: Symbol,
        rate_scaled: i128,
    ) -> Result<(), SwapError> {
        Self::require_admin(&env, &admin)?;
        if rate_scaled <= 0 {
            return Err(SwapError::ZeroAmount);
        }
        env.storage()
            .instance()
            .set(&DataKey::Rate(currency), &rate_scaled);
        Ok(())
    }

    pub fn get_rate(env: Env, currency: Symbol) -> Result<i128, SwapError> {
        env.storage()
            .instance()
            .get(&DataKey::Rate(currency))
            .ok_or(SwapError::UnknownCurrency)
    }

    pub fn quote(
        env: Env,
        from_currency: Symbol,
        to_currency: Symbol,
        amount: i128,
    ) -> Result<i128, SwapError> {
        let rate_from = Self::get_rate(env.clone(), from_currency)?;
        let rate_to = Self::get_rate(env.clone(), to_currency)?;
        Ok(amount * rate_to / rate_from)
    }

    pub fn swap(
        env: Env,
        sender: Address,
        recipient: Address,
        from_currency: Symbol,
        to_currency: Symbol,
        amount: i128,
        min_receive: i128,
        commitment: BytesN<32>,
    ) -> Result<i128, SwapError> {
        sender.require_auth();

        if amount <= 0 {
            return Err(SwapError::ZeroAmount);
        }

        let receive_amount = Self::quote(
            env.clone(),
            from_currency.clone(),
            to_currency.clone(),
            amount,
        )?;
        if receive_amount < min_receive {
            return Err(SwapError::SlippageExceeded);
        }

        let is_zero_commitment = commitment == BytesN::from_array(&env, &[0u8; 32]);
        if !is_zero_commitment {
            let key = DataKey::Commitment(commitment.clone());
            if env.storage().persistent().has(&key) {
                return Err(SwapError::CommitmentAlreadyUsed);
            }
            env.storage().persistent().set(&key, &true);
        }

        let from_token_addr: Address = env
            .storage()
            .instance()
            .get(&DataKey::LiquidityToken(from_currency.clone()))
            .ok_or(SwapError::UnknownCurrency)?;
        let to_token_addr: Address = env
            .storage()
            .instance()
            .get(&DataKey::LiquidityToken(to_currency.clone()))
            .ok_or(SwapError::UnknownCurrency)?;

        let from_token = token::Client::new(&env, &from_token_addr);
        let to_token = token::Client::new(&env, &to_token_addr);

        from_token.transfer(&sender, &env.current_contract_address(), &amount);
        to_token.transfer(&env.current_contract_address(), &recipient, &receive_amount);

        env.events().publish(
            (symbol_short!("swap"), from_currency, to_currency),
            (sender, recipient, receive_amount, commitment),
        );

        Ok(receive_amount)
    }

    pub fn fund_liquidity(
        env: Env,
        admin: Address,
        currency: Symbol,
        amount: i128,
    ) -> Result<(), SwapError> {
        Self::require_admin(&env, &admin)?;
        let token_addr: Address = env
            .storage()
            .instance()
            .get(&DataKey::LiquidityToken(currency))
            .ok_or(SwapError::UnknownCurrency)?;
        let client = token::Client::new(&env, &token_addr);
        client.transfer(&admin, &env.current_contract_address(), &amount);
        Ok(())
    }

    fn require_admin(env: &Env, caller: &Address) -> Result<(), SwapError> {
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(SwapError::NotInitialized)?;
        if admin != *caller {
            return Err(SwapError::NotAdmin);
        }
        caller.require_auth();
        Ok(())
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::testutils::Address as _;
    use soroban_sdk::{token::StellarAssetClient, Env};

    /// Shared fixture: deploys the contract, initializes it with an admin,
    /// wires up USD/NGN tokens, and publishes a fixed rate.
    struct Fixture {
        env: Env,
        admin: Address,
        sender: Address,
        recipient: Address,
        contract_id: Address,
        client: SwiftRampSwapClient<'static>,
        usd_addr: Address,
        usd_token: token::Client<'static>,
        ngn_addr: Address,
        ngn_token: token::Client<'static>,
    }

    fn setup_token(env: &Env, admin: &Address) -> (Address, token::Client<'static>) {
        let sac = env.register_stellar_asset_contract_v2(admin.clone());
        let address = sac.address();
        let client = token::Client::new(env, &address);
        (address, client)
    }

    fn setup() -> Fixture {
        let env = Env::default();
        env.mock_all_auths();

        let admin = Address::generate(&env);
        let sender = Address::generate(&env);
        let recipient = Address::generate(&env);

        let contract_id = env.register_contract(None, SwiftRampSwap);
        let client = SwiftRampSwapClient::new(&env, &contract_id);
        client.initialize(&admin);

        let (usd_addr, usd_token) = setup_token(&env, &admin);
        let (ngn_addr, ngn_token) = setup_token(&env, &admin);

        client.set_currency_token(&admin, &Symbol::new(&env, "USD"), &usd_addr);
        client.set_currency_token(&admin, &Symbol::new(&env, "NGN"), &ngn_addr);
        client.set_rate(&admin, &Symbol::new(&env, "USD"), &RATE_SCALE);
        client.set_rate(&admin, &Symbol::new(&env, "NGN"), &(1580 * RATE_SCALE));

        Fixture {
            env,
            admin,
            sender,
            recipient,
            contract_id,
            client,
            usd_addr,
            usd_token,
            ngn_addr,
            ngn_token,
        }
    }

    fn zero_commitment(env: &Env) -> BytesN<32> {
        BytesN::from_array(env, &[0u8; 32])
    }

    // ---------------------------------------------------------------
    // Happy path
    // ---------------------------------------------------------------

    #[test]
    fn swap_pays_out_at_published_rate() {
        let f = setup();

        let usd_sac = StellarAssetClient::new(&f.env, &f.usd_addr);
        usd_sac.mint(&f.sender, &(100 * RATE_SCALE));
        let ngn_sac = StellarAssetClient::new(&f.env, &f.ngn_addr);
        ngn_sac.mint(&f.contract_id, &(1_000_000 * RATE_SCALE));

        let received = f.client.swap(
            &f.sender,
            &f.recipient,
            &Symbol::new(&f.env, "USD"),
            &Symbol::new(&f.env, "NGN"),
            &(100 * RATE_SCALE),
            &0,
            &zero_commitment(&f.env),
        );

        assert_eq!(received, 158_000 * RATE_SCALE);
        assert_eq!(f.ngn_token.balance(&f.recipient), 158_000 * RATE_SCALE);
        assert_eq!(f.usd_token.balance(&f.sender), 0);
    }

    #[test]
    fn quote_matches_manual_rate_math() {
        let f = setup();
        let quoted = f.client.quote(
            &Symbol::new(&f.env, "USD"),
            &Symbol::new(&f.env, "NGN"),
            &(50 * RATE_SCALE),
        );
        assert_eq!(quoted, 79_000 * RATE_SCALE);
    }

    #[test]
    fn fund_liquidity_moves_tokens_from_admin_to_contract() {
        let f = setup();
        let usd_sac = StellarAssetClient::new(&f.env, &f.usd_addr);
        usd_sac.mint(&f.admin, &(500 * RATE_SCALE));

        f.client
            .fund_liquidity(&f.admin, &Symbol::new(&f.env, "USD"), &(500 * RATE_SCALE));

        assert_eq!(f.usd_token.balance(&f.admin), 0);
        assert_eq!(f.usd_token.balance(&f.contract_id), 500 * RATE_SCALE);
    }

    // ---------------------------------------------------------------
    // Slippage / amount validation
    // ---------------------------------------------------------------

    #[test]
    fn swap_rejects_slippage_below_floor() {
        let f = setup();
        let usd_sac = StellarAssetClient::new(&f.env, &f.usd_addr);
        usd_sac.mint(&f.sender, &(100 * RATE_SCALE));

        let result = f.client.try_swap(
            &f.sender,
            &f.recipient,
            &Symbol::new(&f.env, "USD"),
            &Symbol::new(&f.env, "NGN"),
            &(100 * RATE_SCALE),
            &(999_999 * RATE_SCALE),
            &zero_commitment(&f.env),
        );

        assert_eq!(result, Err(Ok(SwapError::SlippageExceeded)));
    }

    #[test]
    fn swap_rejects_zero_amount() {
        let f = setup();

        let result = f.client.try_swap(
            &f.sender,
            &f.recipient,
            &Symbol::new(&f.env, "USD"),
            &Symbol::new(&f.env, "NGN"),
            &0,
            &0,
            &zero_commitment(&f.env),
        );

        assert_eq!(result, Err(Ok(SwapError::ZeroAmount)));
    }

    #[test]
    fn swap_rejects_negative_amount() {
        let f = setup();

        let result = f.client.try_swap(
            &f.sender,
            &f.recipient,
            &Symbol::new(&f.env, "USD"),
            &Symbol::new(&f.env, "NGN"),
            &(-RATE_SCALE),
            &0,
            &zero_commitment(&f.env),
        );

        assert_eq!(result, Err(Ok(SwapError::ZeroAmount)));
    }

    // ---------------------------------------------------------------
    // Currency / rate lookups
    // ---------------------------------------------------------------

    #[test]
    fn get_rate_errors_for_unknown_currency() {
        let f = setup();
        let result = f.client.try_get_rate(&Symbol::new(&f.env, "ZWL"));
        assert_eq!(result, Err(Ok(SwapError::UnknownCurrency)));
    }

    #[test]
    fn swap_rejects_unpublished_currency() {
        let f = setup();
        let usd_sac = StellarAssetClient::new(&f.env, &f.usd_addr);
        usd_sac.mint(&f.sender, &(100 * RATE_SCALE));

        let result = f.client.try_swap(
            &f.sender,
            &f.recipient,
            &Symbol::new(&f.env, "USD"),
            &Symbol::new(&f.env, "ZWL"),
            &(100 * RATE_SCALE),
            &0,
            &zero_commitment(&f.env),
        );

        assert_eq!(result, Err(Ok(SwapError::UnknownCurrency)));
    }

    #[test]
    fn set_rate_rejects_zero_or_negative() {
        let f = setup();
        let result = f
            .client
            .try_set_rate(&f.admin, &Symbol::new(&f.env, "USD"), &0);
        assert_eq!(result, Err(Ok(SwapError::ZeroAmount)));

        let result = f
            .client
            .try_set_rate(&f.admin, &Symbol::new(&f.env, "USD"), &-1);
        assert_eq!(result, Err(Ok(SwapError::ZeroAmount)));
    }

    // ---------------------------------------------------------------
    // Admin / auth boundaries
    // ---------------------------------------------------------------

    #[test]
    fn initialize_twice_errors() {
        let f = setup();
        let result = f.client.try_initialize(&f.admin);
        assert_eq!(result, Err(Ok(SwapError::AlreadyInitialized)));
    }

    #[test]
    fn admin_only_calls_reject_non_admin_caller() {
        let f = setup();
        let impostor = Address::generate(&f.env);

        let result = f
            .client
            .try_set_rate(&impostor, &Symbol::new(&f.env, "USD"), &RATE_SCALE);
        assert_eq!(result, Err(Ok(SwapError::NotAdmin)));

        let other_addr = Address::generate(&f.env);
        let result =
            f.client
                .try_set_currency_token(&impostor, &Symbol::new(&f.env, "GBP"), &other_addr);
        assert_eq!(result, Err(Ok(SwapError::NotAdmin)));

        let result =
            f.client
                .try_fund_liquidity(&impostor, &Symbol::new(&f.env, "USD"), &RATE_SCALE);
        assert_eq!(result, Err(Ok(SwapError::NotAdmin)));
    }

    // ---------------------------------------------------------------
    // Commitment replay protection
    // ---------------------------------------------------------------

    #[test]
    fn swap_rejects_replayed_commitment() {
        let f = setup();
        let usd_sac = StellarAssetClient::new(&f.env, &f.usd_addr);
        usd_sac.mint(&f.sender, &(200 * RATE_SCALE));
        let ngn_sac = StellarAssetClient::new(&f.env, &f.ngn_addr);
        ngn_sac.mint(&f.contract_id, &(10_000_000 * RATE_SCALE));

        let commitment = BytesN::from_array(&f.env, &[7u8; 32]);

        let first = f.client.try_swap(
            &f.sender,
            &f.recipient,
            &Symbol::new(&f.env, "USD"),
            &Symbol::new(&f.env, "NGN"),
            &(100 * RATE_SCALE),
            &0,
            &commitment,
        );
        assert!(first.is_ok());

        let second = f.client.try_swap(
            &f.sender,
            &f.recipient,
            &Symbol::new(&f.env, "USD"),
            &Symbol::new(&f.env, "NGN"),
            &(100 * RATE_SCALE),
            &0,
            &commitment,
        );
        assert_eq!(second, Err(Ok(SwapError::CommitmentAlreadyUsed)));
    }

    #[test]
    fn zero_commitment_can_be_reused_across_swaps() {
        // The all-zero commitment is treated as "no commitment supplied" and
        // is explicitly exempt from the replay check.
        let f = setup();
        let usd_sac = StellarAssetClient::new(&f.env, &f.usd_addr);
        usd_sac.mint(&f.sender, &(200 * RATE_SCALE));
        let ngn_sac = StellarAssetClient::new(&f.env, &f.ngn_addr);
        ngn_sac.mint(&f.contract_id, &(10_000_000 * RATE_SCALE));

        for _ in 0..2 {
            let result = f.client.try_swap(
                &f.sender,
                &f.recipient,
                &Symbol::new(&f.env, "USD"),
                &Symbol::new(&f.env, "NGN"),
                &(10 * RATE_SCALE),
                &0,
                &zero_commitment(&f.env),
            );
            assert!(result.is_ok());
        }
    }
}
