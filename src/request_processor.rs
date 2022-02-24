use crate::postgres_client::DbAccountInfo;

use {
    crate::{
        postgres_client::SimplePostgresClient, postgres_rpc_server_error::PostgresRpcServerError,
        rpc::OptionalContext, rpc_service::JsonRpcConfig,
    },
    jsonrpc_core::{futures::lock::Mutex, types::error, types::Error, Metadata, Result},
    log::*,
    solana_account_decoder::{
        parse_account_data::AccountAdditionalData,
        parse_token::{
            get_token_account_mint, is_known_spl_token_id, spl_token_native_mint,
            spl_token_native_mint_program_id,
        },
        UiAccount, UiAccountEncoding, UiDataSliceConfig, MAX_BASE58_BYTES,
    },
    solana_client::{
        rpc_config::RpcAccountInfoConfig,
        rpc_custom_error::RpcCustomError,
        rpc_filter::{Memcmp, MemcmpEncodedBytes, RpcFilterType},
        rpc_response::{Response as RpcResponse, *},
    },
    solana_runtime::inline_spl_token::{
        SPL_TOKEN_ACCOUNT_MINT_OFFSET, SPL_TOKEN_ACCOUNT_OWNER_OFFSET,
    },
    solana_sdk::{
        account::ReadableAccount,
        program_pack::Pack,
        pubkey::{Pubkey, PUBKEY_BYTES},
    },
    spl_token::state::{Account as TokenAccount, Mint},
    std::sync::Arc,
};

type RpcCustomResult<T> = std::result::Result<T, RpcCustomError>;

#[derive(Clone)]
pub struct JsonRpcRequestProcessor {
    pub config: JsonRpcConfig,
    pub db_client: Arc<Mutex<SimplePostgresClient>>,
}

impl Metadata for JsonRpcRequestProcessor {}

impl From<PostgresRpcServerError> for RpcCustomError {
    fn from(error: PostgresRpcServerError) -> Self {
        let message = format!("Failed to load data from the database. Error: ({})", error);
        RpcCustomError::ScanError { message }
    }
}

fn rpc_custom_error_from_json_rpc_error(error: jsonrpc_core::Error) -> RpcCustomError {
    let message = format!("Failed to convert the JSON rpc object. Error: ({})", error);
    RpcCustomError::ScanError { message }
}

fn check_slice_and_encoding(encoding: &UiAccountEncoding, data_slice_is_some: bool) -> Result<()> {
    match encoding {
        UiAccountEncoding::JsonParsed => {
            if data_slice_is_some {
                let message =
                    "Sliced account data can only be encoded using binary (base 58) or base64 encoding."
                        .to_string();
                Err(error::Error {
                    code: error::ErrorCode::InvalidRequest,
                    message,
                    data: None,
                })
            } else {
                Ok(())
            }
        }
        UiAccountEncoding::Binary
        | UiAccountEncoding::Base58
        | UiAccountEncoding::Base64
        | UiAccountEncoding::Base64Zstd => Ok(()),
    }
}

fn optimize_filters(filters: &mut Vec<RpcFilterType>) {
    filters.iter_mut().for_each(|filter_type| {
        if let RpcFilterType::Memcmp(compare) = filter_type {
            use MemcmpEncodedBytes::*;
            match &compare.bytes {
                #[allow(deprecated)]
                Binary(bytes) | Base58(bytes) => {
                    compare.bytes = Bytes(bs58::decode(bytes).into_vec().unwrap());
                }
                Base64(bytes) => {
                    compare.bytes = Bytes(base64::decode(bytes).unwrap());
                }
                _ => {}
            }
        }
    })
}

fn encode_account<T: ReadableAccount>(
    account: &T,
    pubkey: &Pubkey,
    encoding: UiAccountEncoding,
    data_slice: Option<UiDataSliceConfig>,
) -> Result<UiAccount> {
    if (encoding == UiAccountEncoding::Binary || encoding == UiAccountEncoding::Base58)
        && account.data().len() > MAX_BASE58_BYTES
    {
        let message = format!("Encoded binary (base 58) data should be less than {} bytes, please use Base64 encoding.", MAX_BASE58_BYTES);
        Err(error::Error {
            code: error::ErrorCode::InvalidRequest,
            message,
            data: None,
        })
    } else {
        Ok(UiAccount::encode(
            pubkey, account, encoding, None, data_slice,
        ))
    }
}

/// Analyze custom filters to determine if the result will be a subset of spl-token accounts by
/// owner.
/// NOTE: `optimize_filters()` should almost always be called before using this method because of
/// the strict match on `MemcmpEncodedBytes::Bytes`.
fn get_spl_token_owner_filter(program_id: &Pubkey, filters: &[RpcFilterType]) -> Option<Pubkey> {
    if !is_known_spl_token_id(program_id) {
        return None;
    }
    let mut data_size_filter: Option<u64> = None;
    let mut owner_key: Option<Pubkey> = None;
    let mut incorrect_owner_len: Option<usize> = None;
    for filter in filters {
        match filter {
            RpcFilterType::DataSize(size) => data_size_filter = Some(*size),
            RpcFilterType::Memcmp(Memcmp {
                offset: SPL_TOKEN_ACCOUNT_OWNER_OFFSET,
                bytes: MemcmpEncodedBytes::Bytes(bytes),
                ..
            }) => {
                if bytes.len() == PUBKEY_BYTES {
                    owner_key = Some(Pubkey::new(bytes));
                } else {
                    incorrect_owner_len = Some(bytes.len());
                }
            }
            _ => {}
        }
    }
    if data_size_filter == Some(TokenAccount::get_packed_len() as u64) {
        if let Some(incorrect_owner_len) = incorrect_owner_len {
            info!(
                "Incorrect num bytes ({:?}) provided for spl_token_owner_filter",
                incorrect_owner_len
            );
        }
        owner_key
    } else {
        debug!("spl_token program filters do not match by-owner index requisites");
        None
    }
}

/// Analyze custom filters to determine if the result will be a subset of spl-token accounts by
/// mint.
/// NOTE: `optimize_filters()` should almost always be called before using this method because of
/// the strict match on `MemcmpEncodedBytes::Bytes`.
fn get_spl_token_mint_filter(program_id: &Pubkey, filters: &[RpcFilterType]) -> Option<Pubkey> {
    if !is_known_spl_token_id(program_id) {
        return None;
    }
    let mut data_size_filter: Option<u64> = None;
    let mut mint: Option<Pubkey> = None;
    let mut incorrect_mint_len: Option<usize> = None;
    for filter in filters {
        match filter {
            RpcFilterType::DataSize(size) => data_size_filter = Some(*size),
            RpcFilterType::Memcmp(Memcmp {
                offset: SPL_TOKEN_ACCOUNT_MINT_OFFSET,
                bytes: MemcmpEncodedBytes::Bytes(bytes),
                ..
            }) => {
                if bytes.len() == PUBKEY_BYTES {
                    mint = Some(Pubkey::new(bytes));
                } else {
                    incorrect_mint_len = Some(bytes.len());
                }
            }
            _ => {}
        }
    }
    if data_size_filter == Some(TokenAccount::get_packed_len() as u64) {
        if let Some(incorrect_mint_len) = incorrect_mint_len {
            info!(
                "Incorrect num bytes ({:?}) provided for spl_token_mint_filter",
                incorrect_mint_len
            );
        }
        mint
    } else {
        debug!("spl_token program filters do not match by-mint index requisites");
        None
    }
}

fn filter_accounts(
    config: Option<RpcAccountInfoConfig>,
    mut filters: Vec<RpcFilterType>,
    accounts: Vec<crate::postgres_client::DbAccountInfo>,
    program_id: &Pubkey,
) -> RpcCustomResult<Vec<RpcKeyedAccount>> {
    let mut keyed_accounts = Vec::new();
    let config = config.unwrap_or_default();
    let encoding = config.encoding.unwrap_or(UiAccountEncoding::Binary);
    let data_slice_config = config.data_slice;
    check_slice_and_encoding(&encoding, data_slice_config.is_some())
        .map_err(rpc_custom_error_from_json_rpc_error)?;
    optimize_filters(&mut filters);
    for account in accounts {
        if account.owner() == program_id
            && filters.iter().all(|filter_type| match filter_type {
                RpcFilterType::DataSize(size) => account.data().len() as u64 == *size,
                RpcFilterType::Memcmp(compare) => compare.bytes_match(account.data()),
            })
        {
            keyed_accounts.push(RpcKeyedAccount {
                pubkey: account.pubkey.to_string(),
                account: encode_account(&account, &account.pubkey, encoding, data_slice_config)
                    .map_err(rpc_custom_error_from_json_rpc_error)?,
            });
        }
    }
    Ok(keyed_accounts)
}

fn get_mint_decimals(data: &[u8]) -> Result<u8> {
    Mint::unpack(data)
        .map_err(|_| {
            Error::invalid_params("Invalid param: Token mint could not be unpacked".to_string())
        })
        .map(|mint| mint.decimals)
}

/// Analyze a mint Pubkey that may be the native_mint and get the mint-account owner (token
/// program_id) and decimals
pub async fn get_mint_owner_and_decimals(
    client: &mut SimplePostgresClient,
    mint: &Pubkey,
) -> Result<(Pubkey, u8)> {
    if mint == &spl_token_native_mint() {
        Ok((
            spl_token_native_mint_program_id(),
            spl_token::native_mint::DECIMALS,
        ))
    } else {
        let result = client.get_account(mint).await;
        match result {
            Ok(mint_account) => {
                let decimals = get_mint_decimals(mint_account.data())?;
                Ok((*mint_account.owner(), decimals))
            }
            Err(err) => {
                error!("Received error while getting account {}", err);
                Err(Error::from(err))
            }
        }
    }
}

pub async fn get_parsed_token_account(
    client: &mut SimplePostgresClient,
    pubkey: &Pubkey,
    account: DbAccountInfo,
) -> Result<UiAccount> {
    if let Some(mint_pubkey) = get_token_account_mint(account.data()) {
        let (_, decimals) = get_mint_owner_and_decimals(client, &mint_pubkey).await?;
        let additional_data = Some(AccountAdditionalData {
            spl_token_decimals: Some(decimals),
        });

        return Ok(UiAccount::encode(
            pubkey,
            &account,
            UiAccountEncoding::JsonParsed,
            additional_data,
            None,
        ));
    }

    Err(Error::invalid_params("Could not find the mint".to_string()))
}

async fn get_encoded_account(
    client: &mut SimplePostgresClient,
    pubkey: &Pubkey,
    encoding: UiAccountEncoding,
    data_slice_config: Option<UiDataSliceConfig>,
) -> Result<Option<UiAccount>> {
    let result = client.get_account(pubkey).await;
    match result {
        Ok(account) => {
            if account.lamports() == 0 {
                return Ok(None)
            }
            if is_known_spl_token_id(account.owner()) && encoding == UiAccountEncoding::JsonParsed {
                let account = get_parsed_token_account(client, pubkey, account).await;
                account.and_then(|account| Ok(Some(account)))
            } else {                
                Ok(Some(UiAccount::encode(
                    &account.pubkey,
                    &account,
                    encoding,
                    None,
                    data_slice_config,
                )))
            }
        }
        Err(_err) => Err(Error::internal_error()),
    }
}

impl From<PostgresRpcServerError> for Error {
    fn from(error: PostgresRpcServerError) -> Self {
        match error {
            PostgresRpcServerError::ObjectNotFound { msg } => {
                info!("Object is not found: {}", msg);
                Error::invalid_params("Object is not found")
            }
            _ => Error::internal_error(),
        }
    }
}

impl JsonRpcRequestProcessor {
    pub fn new(config: JsonRpcConfig, db_client: SimplePostgresClient) -> Self {
        Self {
            config,
            db_client: Arc::new(Mutex::new(db_client)),
        }
    }

    pub async fn get_account_info(
        &mut self,
        pubkey: &Pubkey,
        config: Option<RpcAccountInfoConfig>,
    ) -> Result<RpcResponse<Option<UiAccount>>> {
        info!("getting account_info is called... {}", pubkey);
        let config = config.unwrap_or_default();
        let encoding = config.encoding.unwrap_or(UiAccountEncoding::Binary);
        check_slice_and_encoding(&encoding, config.data_slice.is_some())?;

        let data_slice_config = config.data_slice;
        let mut client = self.db_client.lock().await;

        let account = get_encoded_account(&mut client, pubkey, encoding, data_slice_config).await?;

        Ok(RpcResponse {
            context: RpcResponseContext { slot: 0 },
            value: account,
        })
    }

    pub async fn get_multiple_accounts(
        &self,
        pubkeys: Vec<Pubkey>,
        config: Option<RpcAccountInfoConfig>,
    ) -> Result<RpcResponse<Vec<Option<UiAccount>>>> {
        info!("getting account_info is called for {:?}", pubkeys);
        let config = config.unwrap_or_default();
        let encoding = config.encoding.unwrap_or(UiAccountEncoding::Binary);
        check_slice_and_encoding(&encoding, config.data_slice.is_some())?;

        let mut client = self.db_client.lock().await;

        let mut accounts = Vec::new();

        for pubkey in pubkeys {
            let account =
                get_encoded_account(&mut client, &pubkey, encoding, config.data_slice).await?;
            accounts.push(account);
        }

        Ok(RpcResponse {
            context: RpcResponseContext { slot: 0 },
            value: accounts,
        })
    }

    /// Get an iterator of spl-token accounts by owner address
    async fn get_filtered_spl_token_accounts_by_owner(
        &self,
        client: &mut SimplePostgresClient,
        config: Option<RpcAccountInfoConfig>,
        program_id: &Pubkey,
        owner_key: &Pubkey,
        mut filters: Vec<RpcFilterType>,
    ) -> RpcCustomResult<Vec<RpcKeyedAccount>> {
        filters.push(RpcFilterType::DataSize(
            TokenAccount::get_packed_len() as u64
        ));
        // Filter on Owner address
        filters.push(RpcFilterType::Memcmp(Memcmp {
            offset: SPL_TOKEN_ACCOUNT_OWNER_OFFSET,
            bytes: MemcmpEncodedBytes::Bytes(owner_key.to_bytes().into()),
            encoding: None,
        }));

        let accounts = client.get_accounts_by_spl_token_owner(owner_key).await?;

        filter_accounts(config, filters, accounts, program_id)
    }

    /// Get an iterator of spl-token accounts by mint address
    async fn get_filtered_spl_token_accounts_by_mint(
        &self,
        client: &mut SimplePostgresClient,
        config: Option<RpcAccountInfoConfig>,
        program_id: &Pubkey,
        mint_key: &Pubkey,
        mut filters: Vec<RpcFilterType>,
    ) -> RpcCustomResult<Vec<RpcKeyedAccount>> {
        filters.push(RpcFilterType::DataSize(
            TokenAccount::get_packed_len() as u64
        ));
        // Filter on Mint address
        filters.push(RpcFilterType::Memcmp(Memcmp {
            offset: SPL_TOKEN_ACCOUNT_MINT_OFFSET,
            bytes: MemcmpEncodedBytes::Bytes(mint_key.to_bytes().into()),
            encoding: None,
        }));

        let accounts = client.get_accounts_by_spl_token_owner(mint_key).await?;
        filter_accounts(config, filters, accounts, program_id)
    }

    async fn get_filtered_program_accounts(
        &self,
        client: &mut SimplePostgresClient,
        program_id: &Pubkey,
        config: Option<RpcAccountInfoConfig>,
        filters: Vec<RpcFilterType>,
    ) -> RpcCustomResult<Vec<RpcKeyedAccount>> {
        let accounts = client.get_accounts_by_owner(program_id).await?;
        filter_accounts(config, filters, accounts, program_id)
    }

    #[allow(unused_mut)]
    pub async fn get_program_accounts(
        &self,
        program_id: &Pubkey,
        config: Option<RpcAccountInfoConfig>,
        mut filters: Vec<RpcFilterType>,
        with_context: bool,
    ) -> Result<OptionalContext<Vec<RpcKeyedAccount>>> {
        info!("get_program_accounts is called... {}", program_id);
        let mut client = self.db_client.lock().await;

        let result = {
            if let Some(owner) = get_spl_token_owner_filter(program_id, &filters) {
                self.get_filtered_spl_token_accounts_by_owner(
                    &mut client,
                    config,
                    program_id,
                    &owner,
                    filters,
                )
                .await?
            } else if let Some(mint) = get_spl_token_mint_filter(program_id, &filters) {
                self.get_filtered_spl_token_accounts_by_mint(
                    &mut client,
                    config,
                    program_id,
                    &mint,
                    filters,
                )
                .await?
            } else {
                self.get_filtered_program_accounts(&mut client, program_id, config, filters)
                    .await?
            }
        };

        Ok(result).map(OptionalContext::NoContext)
    }
}
