use anchor_client::{RequestBuilder, RequestNamespace};
use anchor_lang::AccountDeserialize;
use async_trait::async_trait;
use helium_anchor_gen::{
    data_credits::{self, accounts, instruction},
    helium_sub_daos::{self, DaoV0, SubDaoV0},
};
use helium_crypto::PublicKeyBinary;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use solana_client::{
    client_error::ClientError, nonblocking::rpc_client::RpcClient, rpc_response::Response,
};
use solana_sdk::{
    commitment_config::CommitmentConfig,
    program_pack::Pack,
    pubkey::{ParsePubkeyError, Pubkey},
    signature::{read_keypair_file, Keypair, Signature},
    signer::Signer,
    transaction::Transaction,
};
use std::convert::Infallible;
use std::{collections::HashMap, str::FromStr};
use std::{
    sync::Arc,
    time::{Duration, SystemTime, SystemTimeError},
};
use tokio::sync::Mutex;

#[async_trait]
pub trait SolanaNetwork: Send + Sync + 'static {
    type Error: std::error::Error + Send + Sync + 'static;
    type Transaction: GetSignature + Send + Sync + 'static;

    async fn payer_balance(&self, payer: &PublicKeyBinary) -> Result<u64, Self::Error>;

    async fn make_burn_transaction(
        &self,
        payer: &PublicKeyBinary,
        amount: u64,
    ) -> Result<Self::Transaction, Self::Error>;

    async fn submit_transaction(&self, transaction: &Self::Transaction) -> Result<(), Self::Error>;

    async fn confirm_transaction(&self, txn: &Signature) -> Result<bool, Self::Error>;
}

pub trait GetSignature {
    fn get_signature(&self) -> &Signature;
}

impl GetSignature for Transaction {
    fn get_signature(&self) -> &Signature {
        &self.signatures[0]
    }
}

impl GetSignature for Signature {
    fn get_signature(&self) -> &Signature {
        self
    }
}

macro_rules! send_with_retry {
    ($rpc:expr) => {{
        let mut attempt = 1;
        loop {
            match $rpc.await {
                Ok(resp) => break Ok(resp),
                Err(err) => {
                    if attempt < 5 {
                        attempt += 1;
                        tokio::time::sleep(Duration::from_secs(attempt)).await;
                        continue;
                    } else {
                        break Err(err);
                    }
                }
            }
        }
    }};
}

#[derive(thiserror::Error, Debug)]
pub enum SolanaRpcError {
    #[error("Solana rpc error: {0}")]
    RpcClientError(#[from] ClientError),
    #[error("Anchor error: {0}")]
    AnchorError(Box<anchor_lang::error::Error>),
    #[error("Solana program error: {0}")]
    ProgramError(#[from] solana_sdk::program_error::ProgramError),
    #[error("Parse pubkey error: {0}")]
    ParsePubkeyError(#[from] ParsePubkeyError),
    #[error("DC burn authority does not match keypair")]
    InvalidKeypair,
    #[error("System time error: {0}")]
    SystemTimeError(#[from] SystemTimeError),
    #[error("Failed to read keypair file")]
    FailedToReadKeypairError,
    #[error("crypto error: {0}")]
    Crypto(#[from] helium_crypto::Error),
}

impl From<anchor_lang::error::Error> for SolanaRpcError {
    fn from(err: anchor_lang::error::Error) -> Self {
        Self::AnchorError(Box::new(err))
    }
}

#[derive(Debug, Deserialize)]
pub struct Settings {
    rpc_url: String,
    cluster: String,
    burn_keypair: String,
    dc_mint: String,
    dnt_mint: String,
    #[serde(default)]
    payers_to_monitor: Vec<String>,
}

impl Settings {
    pub fn payers_to_monitor(&self) -> Result<Vec<PublicKeyBinary>, SolanaRpcError> {
        self.payers_to_monitor
            .iter()
            .map(|payer| PublicKeyBinary::from_str(payer))
            .collect::<Result<_, _>>()
            .map_err(SolanaRpcError::from)
    }
}

pub struct SolanaRpc {
    provider: RpcClient,
    program_cache: BurnProgramCache,
    cluster: String,
    keypair: [u8; 64],
    payers_to_monitor: Vec<PublicKeyBinary>,
}

impl SolanaRpc {
    pub async fn new(settings: &Settings) -> Result<Arc<Self>, SolanaRpcError> {
        let dc_mint = settings.dc_mint.parse()?;
        let dnt_mint = settings.dnt_mint.parse()?;
        let Ok(keypair) = read_keypair_file(&settings.burn_keypair) else {
            return Err(SolanaRpcError::FailedToReadKeypairError);
        };
        let provider =
            RpcClient::new_with_commitment(settings.rpc_url.clone(), CommitmentConfig::finalized());
        let program_cache = BurnProgramCache::new(&provider, dc_mint, dnt_mint).await?;
        if program_cache.dc_burn_authority != keypair.pubkey() {
            return Err(SolanaRpcError::InvalidKeypair);
        }
        Ok(Arc::new(Self {
            cluster: settings.cluster.clone(),
            provider,
            program_cache,
            keypair: keypair.to_bytes(),
            payers_to_monitor: settings.payers_to_monitor()?,
        }))
    }
}

#[async_trait]
impl SolanaNetwork for SolanaRpc {
    type Error = SolanaRpcError;
    type Transaction = Transaction;

    async fn payer_balance(&self, payer: &PublicKeyBinary) -> Result<u64, Self::Error> {
        let ddc_key = delegated_data_credits(&self.program_cache.sub_dao, payer);
        let (escrow_account, _) = Pubkey::find_program_address(
            &["escrow_dc_account".as_bytes(), &ddc_key.to_bytes()],
            &data_credits::ID,
        );
        let account_data = match self
            .provider
            .get_account_with_commitment(&escrow_account, CommitmentConfig::finalized())
            .await?
        {
            Response { value: None, .. } => {
                tracing::info!(%payer, "Account not found, therefore no balance");
                return Ok(0);
            }
            Response {
                value: Some(account),
                ..
            } => account.data,
        };
        let account_layout = spl_token::state::Account::unpack(account_data.as_slice())?;

        if self.payers_to_monitor.contains(payer) {
            metrics::gauge!(
                "balance",
                account_layout.amount as f64,
                "payer" => payer.to_string()
            );
        }

        Ok(account_layout.amount)
    }

    async fn make_burn_transaction(
        &self,
        payer: &PublicKeyBinary,
        amount: u64,
    ) -> Result<Self::Transaction, Self::Error> {
        // Fetch the sub dao epoch info:
        const EPOCH_LENGTH: u64 = 60 * 60 * 24;
        let epoch = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)?
            .as_secs()
            / EPOCH_LENGTH;
        let (sub_dao_epoch_info, _) = Pubkey::find_program_address(
            &[
                "sub_dao_epoch_info".as_bytes(),
                self.program_cache.sub_dao.as_ref(),
                &epoch.to_le_bytes(),
            ],
            &helium_sub_daos::ID,
        );

        // Fetch escrow account
        let ddc_key = delegated_data_credits(&self.program_cache.sub_dao, payer);
        let (escrow_account, _) = Pubkey::find_program_address(
            &["escrow_dc_account".as_bytes(), &ddc_key.to_bytes()],
            &data_credits::ID,
        );

        let instructions = {
            let request = RequestBuilder::from(
                data_credits::id(),
                &self.cluster,
                std::rc::Rc::new(Keypair::from_bytes(&self.keypair).unwrap()),
                Some(CommitmentConfig::confirmed()),
                RequestNamespace::Global,
            );

            let accounts = accounts::BurnDelegatedDataCreditsV0 {
                sub_dao_epoch_info,
                dao: self.program_cache.dao,
                sub_dao: self.program_cache.sub_dao,
                account_payer: self.program_cache.account_payer,
                data_credits: self.program_cache.data_credits,
                delegated_data_credits: delegated_data_credits(&self.program_cache.sub_dao, payer),
                token_program: spl_token::id(),
                helium_sub_daos_program: helium_sub_daos::id(),
                system_program: solana_program::system_program::id(),
                dc_burn_authority: self.program_cache.dc_burn_authority,
                dc_mint: self.program_cache.dc_mint,
                escrow_account,
                registrar: self.program_cache.registrar,
            };
            let args = instruction::BurnDelegatedDataCreditsV0 {
                _args: data_credits::BurnDelegatedDataCreditsArgsV0 { amount },
            };

            // As far as I can tell, the instructions function does not actually have any
            // error paths.
            request
                .accounts(accounts)
                .args(args)
                .instructions()
                .unwrap()
        };

        let blockhash = self.provider.get_latest_blockhash().await?;
        let signer = Keypair::from_bytes(&self.keypair).unwrap();

        Ok(Transaction::new_signed_with_payer(
            &instructions,
            Some(&signer.pubkey()),
            &[&signer],
            blockhash,
        ))
    }

    async fn submit_transaction(&self, tx: &Self::Transaction) -> Result<(), Self::Error> {
        match send_with_retry!(self.provider.send_and_confirm_transaction(tx)) {
            Ok(signature) => {
                tracing::info!(
                    transaction = %signature,
                    "Data credit burn successful",
                );
                Ok(())
            }
            Err(err) => {
                let signature = tx.get_signature();
                tracing::error!(
                    transaction = %signature,
                    "Data credit burn failed: {err:?}"
                );
                Err(SolanaRpcError::RpcClientError(err))
            }
        }
    }

    async fn confirm_transaction(&self, txn: &Signature) -> Result<bool, Self::Error> {
        Ok(matches!(
            self.provider
                .get_signature_status_with_commitment_and_history(
                    txn,
                    CommitmentConfig::confirmed(),
                    true,
                )
                .await?,
            Some(Ok(()))
        ))
    }
}

/// Cached pubkeys for the burn program
pub struct BurnProgramCache {
    pub account_payer: Pubkey,
    pub data_credits: Pubkey,
    pub sub_dao: Pubkey,
    pub dao: Pubkey,
    pub dc_mint: Pubkey,
    pub dc_burn_authority: Pubkey,
    pub registrar: Pubkey,
}

impl BurnProgramCache {
    pub async fn new(
        provider: &RpcClient,
        dc_mint: Pubkey,
        dnt_mint: Pubkey,
    ) -> Result<Self, SolanaRpcError> {
        let (account_payer, _) =
            Pubkey::find_program_address(&["account_payer".as_bytes()], &data_credits::ID);
        let (data_credits, _) =
            Pubkey::find_program_address(&["dc".as_bytes(), dc_mint.as_ref()], &data_credits::ID);
        let (sub_dao, _) = Pubkey::find_program_address(
            &["sub_dao".as_bytes(), dnt_mint.as_ref()],
            &helium_sub_daos::ID,
        );
        let (dao, dc_burn_authority) = {
            let account_data = provider.get_account_data(&sub_dao).await?;
            let mut account_data = account_data.as_ref();
            let sub_dao = SubDaoV0::try_deserialize(&mut account_data)?;
            (sub_dao.dao, sub_dao.dc_burn_authority)
        };
        let registrar = {
            let account_data = provider.get_account_data(&dao).await?;
            let mut account_data = account_data.as_ref();
            DaoV0::try_deserialize(&mut account_data)?.registrar
        };
        Ok(Self {
            account_payer,
            data_credits,
            sub_dao,
            dao,
            dc_mint,
            dc_burn_authority,
            registrar,
        })
    }
}

const FIXED_BALANCE: u64 = 1_000_000_000;

pub enum PossibleTransaction {
    NoTransaction(Signature),
    Transaction(Transaction),
}

impl GetSignature for PossibleTransaction {
    fn get_signature(&self) -> &Signature {
        match self {
            Self::NoTransaction(ref sig) => sig,
            Self::Transaction(ref txn) => txn.get_signature(),
        }
    }
}

#[async_trait]
impl SolanaNetwork for Option<Arc<SolanaRpc>> {
    type Error = SolanaRpcError;
    type Transaction = PossibleTransaction;

    async fn payer_balance(&self, payer: &PublicKeyBinary) -> Result<u64, Self::Error> {
        if let Some(ref rpc) = self {
            rpc.payer_balance(payer).await
        } else {
            Ok(FIXED_BALANCE)
        }
    }

    async fn make_burn_transaction(
        &self,
        payer: &PublicKeyBinary,
        amount: u64,
    ) -> Result<Self::Transaction, Self::Error> {
        if let Some(ref rpc) = self {
            Ok(PossibleTransaction::Transaction(
                rpc.make_burn_transaction(payer, amount).await?,
            ))
        } else {
            Ok(PossibleTransaction::NoTransaction(Signature::new_unique()))
        }
    }

    async fn submit_transaction(&self, transaction: &Self::Transaction) -> Result<(), Self::Error> {
        match (self, transaction) {
            (Some(ref rpc), PossibleTransaction::Transaction(ref txn)) => {
                rpc.submit_transaction(txn).await?
            }
            (None, PossibleTransaction::NoTransaction(_)) => (),
            _ => unreachable!(),
        }
        Ok(())
    }

    async fn confirm_transaction(&self, txn: &Signature) -> Result<bool, Self::Error> {
        if let Some(ref rpc) = self {
            rpc.confirm_transaction(txn).await
        } else {
            panic!("We will not confirm transactions when Solana is disabled");
        }
    }
}

pub struct MockTransaction {
    pub signature: Signature,
    pub payer: PublicKeyBinary,
    pub amount: u64,
}

impl GetSignature for MockTransaction {
    fn get_signature(&self) -> &Signature {
        &self.signature
    }
}

#[async_trait]
impl SolanaNetwork for Arc<Mutex<HashMap<PublicKeyBinary, u64>>> {
    type Error = Infallible;
    type Transaction = MockTransaction;

    async fn payer_balance(&self, payer: &PublicKeyBinary) -> Result<u64, Self::Error> {
        Ok(*self.lock().await.get(payer).unwrap())
    }

    async fn make_burn_transaction(
        &self,
        payer: &PublicKeyBinary,
        amount: u64,
    ) -> Result<MockTransaction, Self::Error> {
        Ok(MockTransaction {
            signature: Signature::new_unique(),
            payer: payer.clone(),
            amount,
        })
    }

    async fn submit_transaction(&self, txn: &MockTransaction) -> Result<(), Self::Error> {
        *self.lock().await.get_mut(&txn.payer).unwrap() -= txn.amount;
        Ok(())
    }

    async fn confirm_transaction(&self, _txn: &Signature) -> Result<bool, Self::Error> {
        Ok(true)
    }
}

/// Returns the PDA for the Delegated Data Credits of the given `payer`.
pub fn delegated_data_credits(sub_dao: &Pubkey, payer: &PublicKeyBinary) -> Pubkey {
    let mut hasher = Sha256::new();
    hasher.update(payer.to_string());
    let sha_digest = hasher.finalize();
    let (ddc_key, _) = Pubkey::find_program_address(
        &[
            "delegated_data_credits".as_bytes(),
            sub_dao.as_ref(),
            &sha_digest,
        ],
        &data_credits::ID,
    );
    ddc_key
}
