use crate::models::*;
use anyhow::{Context, Result};
use reqwest::Client;
use serde_json::Value;
use std::str::FromStr;
use hex;
use log::{info, warn};

// Official SDK imports for proper order signing
use polymarket_client_sdk::clob::{Client as ClobClient, Config as ClobConfig};
use polymarket_client_sdk::clob::types::{Side, OrderType, SignatureType};
use polymarket_client_sdk::auth::state::Authenticated;
use polymarket_client_sdk::auth::Normal;
use polymarket_client_sdk::POLYGON;
use alloy::signers::local::{LocalSigner, PrivateKeySigner};
use alloy::signers::Signer as _;
use alloy::primitives::Address as AlloyAddress;
use alloy::primitives::{Address, B256, U256, Bytes};
use alloy::primitives::keccak256;
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::eth::TransactionRequest;
use alloy::sol;
use alloy_sol_types::SolCall;

sol! {
    interface IConditionalTokens {
        function redeemPositions(
            address collateralToken,
            bytes32 parentCollectionId,
            bytes32 conditionId,
            uint256[] indexSets
        ) external;
    }
}

pub struct PolymarketApi {
    client: Client,
    gamma_url: String,
    clob_url: String,
    private_key: Option<String>,
    proxy_wallet_address: Option<String>,
    signature_type: Option<u8>,
    rpc_urls: Vec<String>,
}

impl PolymarketApi {
    pub fn new(
        gamma_url: String,
        clob_url: String,
        private_key: Option<String>,
        proxy_wallet_address: Option<String>,
        signature_type: Option<u8>,
        rpc_urls: Vec<String>,
    ) -> Self {
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .expect("Failed to create HTTP client");
        Self {
            client,
            gamma_url,
            clob_url,
            private_key,
            proxy_wallet_address,
            signature_type,
            rpc_urls,
        }
    }

    /// Build a signer + authenticated CLOB client, deduplicating the repeated
    /// private-key → signer → auth-builder → proxy/signature-type setup.
    async fn build_clob_client(&self) -> Result<(PrivateKeySigner, ClobClient<Authenticated<Normal>>)> {
        let private_key = self.private_key.as_ref()
            .ok_or_else(|| anyhow::anyhow!("Private key is required. Please set private_key in config.json"))?;

        let signer = LocalSigner::from_str(private_key)
            .context("Failed to create signer from private key. Ensure private_key is a valid hex string.")?
            .with_chain_id(Some(POLYGON));

        let mut auth_builder = ClobClient::new(&self.clob_url, ClobConfig::default())
            .context("Failed to create CLOB client")?
            .authentication_builder(&signer);

        if let Some(proxy_addr) = &self.proxy_wallet_address {
            let funder_address = AlloyAddress::parse_checksummed(proxy_addr, None)
                .context(format!("Failed to parse proxy_wallet_address: {}. Ensure it's a valid Ethereum address.", proxy_addr))?;

            auth_builder = auth_builder.funder(funder_address);

            let sig_type = match self.signature_type {
                Some(1) => SignatureType::Proxy,
                Some(2) => SignatureType::GnosisSafe,
                Some(0) | None => {
                    warn!("Proxy_wallet_address is set but signature_type is EOA. Defaulting to Proxy.");
                    SignatureType::Proxy
                },
                Some(n) => anyhow::bail!("Invalid signature_type: {}. Must be 0 (EOA), 1 (Proxy), or 2 (GnosisSafe)", n),
            };

            auth_builder = auth_builder.signature_type(sig_type);
            eprintln!("Using proxy wallet: {} (signature type: {:?})", proxy_addr, sig_type);
        } else if let Some(sig_type_num) = self.signature_type {
            let sig_type = match sig_type_num {
                0 => SignatureType::Eoa,
                1 | 2 => anyhow::bail!("signature_type {} requires proxy_wallet_address to be set", sig_type_num),
                n => anyhow::bail!("Invalid signature_type: {}. Must be 0 (EOA), 1 (Proxy), or 2 (GnosisSafe)", n),
            };
            auth_builder = auth_builder.signature_type(sig_type);
        }

        let client = auth_builder
            .authenticate()
            .await
            .context("Failed to authenticate with CLOB API. Check your credentials and private_key.")?;

        Ok((signer, client))
    }

    // Authenticate with Polymarket CLOB API
    pub async fn authenticate(&self) -> Result<()> {
        let (_signer, _client) = self.build_clob_client().await?;

        eprintln!("   Successfully authenticated with Polymarket CLOB API");
        eprintln!("   Private key: Valid");
        eprintln!("   API credentials: Valid");
        if let Some(proxy_addr) = &self.proxy_wallet_address {
            eprintln!("   Proxy wallet: {}", proxy_addr);
        } else {
            eprintln!("   Trading account: EOA (private key account)");
        }
        Ok(())
    }

    // Get market by slug (e.g., "btc-updown-5m-1767726000")
    pub async fn get_market_by_slug(&self, slug: &str) -> Result<Market> {
        let url = format!("{}/events/slug/{}", self.gamma_url, slug);

        let response = self.client.get(&url).send().await
            .context(format!("Failed to fetch market by slug: {}", slug))?;

        let status = response.status();
        if !status.is_success() {
            anyhow::bail!("Failed to fetch market by slug: {} (status: {})", slug, status);
        }

        let json: Value = response.json().await
            .context("Failed to parse market response")?;

        if let Some(markets) = json.get("markets").and_then(|m| m.as_array()) {
            if let Some(market_json) = markets.first() {
                if let Ok(market) = serde_json::from_value::<Market>(market_json.clone()) {
                    return Ok(market);
                }
            }
        }

        anyhow::bail!("Invalid market response format: no markets array found")
    }

    // Get order book for a specific token
    pub async fn get_orderbook(&self, token_id: &str) -> Result<OrderBook> {
        let url = format!("{}/book", self.clob_url);
        let params = [("token_id", token_id)];

        let response = self
            .client
            .get(&url)
            .query(&params)
            .send()
            .await
            .context("Failed to fetch orderbook")?;

        let orderbook: OrderBook = response
            .json()
            .await
            .context("Failed to parse orderbook")?;

        Ok(orderbook)
    }

    /// Get market details by condition ID
    pub async fn get_market(&self, condition_id: &str) -> Result<MarketDetails> {
        let url = format!("{}/markets/{}", self.clob_url, condition_id);

        let response = self
            .client
            .get(&url)
            .send()
            .await
            .context(format!("Failed to fetch market for condition_id: {}", condition_id))?;

        let status = response.status();

        if !status.is_success() {
            anyhow::bail!("Failed to fetch market (status: {})", status);
        }

        let json_text = response.text().await
            .context("Failed to read response body")?;

        let market: MarketDetails = serde_json::from_str(&json_text)
            .map_err(|e| {
                log::error!("Failed to parse market response: {}. Response was: {}", e, json_text);
                anyhow::anyhow!("Failed to parse market response: {}", e)
            })?;

        Ok(market)
    }

    /// Place a Fill-or-Kill buy order. Returns Ok(Some(response)) if filled, Ok(None) if not fillable.
    pub async fn place_fok_buy(&self, token_id: &str, size: &str, price: &str) -> Result<Option<OrderResponse>> {
        let (signer, client) = self.build_clob_client().await?;

        let price_dec = rust_decimal::Decimal::from_str(price)
            .context(format!("Failed to parse price: {}", price))?;
        let size_dec = rust_decimal::Decimal::from_str(size)
            .context(format!("Failed to parse size: {}", size))?;

        let token_id_u256 = if token_id.starts_with("0x") {
            U256::from_str_radix(token_id.trim_start_matches("0x"), 16)
        } else {
            U256::from_str_radix(token_id, 10)
        }.context(format!("Failed to parse token_id as U256: {}", token_id))?;

        let order_builder = client
            .limit_order()
            .token_id(token_id_u256)
            .size(size_dec)
            .price(price_dec)
            .side(Side::Buy)
            .order_type(OrderType::FOK);

        let signed_order = client.sign(&signer, order_builder.build().await?)
            .await
            .context("Failed to sign FOK order")?;

        let response = match client.post_order(signed_order).await {
            Ok(resp) => resp,
            Err(e) => {
                let err_str = e.to_string().to_lowercase();
                if err_str.contains("timeout") || err_str.contains("timed out")
                    || err_str.contains("connection") || err_str.contains("connect")
                    || err_str.contains("broken pipe") || err_str.contains("reset")
                {
                    // Network error: order may have been placed — halt sweep
                    return Err(anyhow::anyhow!("FOK buy network error (order may be placed): {}", e));
                }
                // API rejection: order was not placed — skip and continue
                warn!("FOK buy rejected (unfillable): {}", e);
                return Ok(None);
            }
        };

        if !response.success {
            // Order rejected — not fillable at this price/size
            return Ok(None);
        }

        Ok(Some(OrderResponse {
            order_id: Some(response.order_id.clone()),
            status: response.status.to_string(),
            message: Some(format!("FOK buy filled. Order ID: {}", response.order_id)),
        }))
    }

    pub async fn get_redeemable_positions(&self, wallet: &str) -> Result<Vec<String>> {
        let url = "https://data-api.polymarket.com/positions";
        let user = if wallet.starts_with("0x") {
            wallet.to_string()
        } else {
            format!("0x{}", wallet)
        };
        let response = self.client
            .get(url)
            .query(&[("user", user.as_str()), ("redeemable", "true"), ("limit", "500")])
            .send()
            .await
            .context("Failed to fetch redeemable positions")?;
        if !response.status().is_success() {
            anyhow::bail!("Data API returned {} for redeemable positions", response.status());
        }
        let positions: Vec<Value> = response.json().await.unwrap_or_default();
        let mut condition_ids: Vec<String> = positions
            .iter()
            .filter(|p| {
                let size = p.get("size")
                    .and_then(|s| s.as_f64())
                    .or_else(|| p.get("size").and_then(|s| s.as_u64().map(|u| u as f64)))
                    .or_else(|| p.get("size").and_then(|s| s.as_str()).and_then(|s| s.parse::<f64>().ok()));
                size.map(|s| s > 0.0).unwrap_or(false)
            })
            .filter_map(|p| p.get("conditionId").and_then(|c| c.as_str()).map(|s| {
                if s.starts_with("0x") { s.to_string() } else { format!("0x{}", s) }
            }))
            .collect();
        condition_ids.sort();
        condition_ids.dedup();
        Ok(condition_ids)
    }

    pub async fn redeem_tokens(
        &self,
        condition_id: &str,
        outcome: &str,
    ) -> Result<RedeemResponse> {
        let private_key = self.private_key.as_ref()
            .ok_or_else(|| anyhow::anyhow!("Private key is required for order signing. Please set private_key in config.json"))?;

        let signer = LocalSigner::from_str(private_key)
            .context("Failed to create signer from private key. Ensure private_key is a valid hex string.")?
            .with_chain_id(Some(POLYGON));

        let parse_address_hex = |s: &str| -> Result<Address> {
            let hex_str = s.strip_prefix("0x").unwrap_or(s);
            let bytes = hex::decode(hex_str).context("Invalid hex in address")?;
            let len= bytes.len();
            let arr: [u8; 20] = bytes.try_into().map_err(|_| anyhow::anyhow!("Address must be 20 bytes, got {}", len))?;
            Ok(Address::from(arr))
        };

        let collateral_token = parse_address_hex("0x2791Bca1f2de4661ED88A30C99A7a9449Aa84174")
            .context("Failed to parse USDC address")?;

        let condition_id_clean = condition_id.strip_prefix("0x").unwrap_or(condition_id);
        let condition_id_b256 = B256::from_str(condition_id_clean)
            .context(format!("Failed to parse condition_id as B256: {}", condition_id))?;

        let index_set = if outcome.to_uppercase().contains("UP") || outcome == "1" {
            U256::from(1)
        } else {
            U256::from(2)
        };

        eprintln!("Redeeming winning tokens for condition {} (outcome: {}, index_set: {})",
              condition_id, outcome, index_set);

        const CTF_CONTRACT: &str = "0x4d97dcd97ec945f40cf65f87097ace5ea0476045";
        let rpc_url = self.rpc_urls.first().map(|s| s.as_str()).unwrap_or("https://polygon-rpc.com");
        const PROXY_WALLET_FACTORY: &str = "0xaB45c5A4B0c941a2F231C04C3f49182e1A254052";

        let ctf_address = parse_address_hex(CTF_CONTRACT)
            .context("Failed to parse CTF contract address")?;

        let parent_collection_id = B256::ZERO;
        let use_proxy = self.proxy_wallet_address.is_some();
        let sig_type = self.signature_type.unwrap_or(1);
        let index_sets: Vec<U256> = if use_proxy && sig_type == 2 {
            vec![U256::from(1), U256::from(2)]
        } else {
            vec![index_set]
        };

        eprintln!("   Prepared redemption parameters:");
        eprintln!("   - CTF Contract: {}", ctf_address);
        eprintln!("   - Collateral token (USDC): {}", collateral_token);
        eprintln!("   - Condition ID: {} ({:?})", condition_id, condition_id_b256);
        eprintln!("   - Index set(s): {:?} (outcome: {})", index_sets, outcome);

        let redeem_call = IConditionalTokens::redeemPositionsCall {
            collateralToken: collateral_token,
            parentCollectionId: parent_collection_id,
            conditionId: condition_id_b256,
            indexSets: index_sets.clone(),
        };
        let redeem_calldata = redeem_call.abi_encode();

        let (tx_to, tx_data, gas_limit, used_safe_redemption) = if use_proxy && sig_type == 2 {
            let safe_address_str = self.proxy_wallet_address.as_deref()
                .ok_or_else(|| anyhow::anyhow!("proxy_wallet_address required for Safe redemption"))?;
            let safe_address = parse_address_hex(safe_address_str)
                .context("Failed to parse proxy_wallet_address (Safe address)")?;
            eprintln!("   Using Gnosis Safe (proxy): signing and executing redemption via Safe.execTransaction");
            let nonce_selector = keccak256("nonce()".as_bytes());
            let nonce_calldata: Vec<u8> = nonce_selector.as_slice()[..4].to_vec();
            let provider_read = ProviderBuilder::new()
                .connect(rpc_url)
                .await
                .context("Failed to connect to RPC for Safe read calls")?;
            let nonce_tx = TransactionRequest::default()
                .to(safe_address)
                .input(Bytes::from(nonce_calldata.clone()).into());
            let nonce_result = provider_read.call(nonce_tx).await
                .map_err(|e| anyhow::anyhow!("Failed to call Safe.nonce() on {}: {}. \
                    If you use MagicLink/email login, your proxy is a Polymarket custom proxy, not a Gnosis Safe; \
                    redemption via Safe is only supported for MetaMask (Gnosis Safe) proxies.",
                    safe_address_str, e))?;
            let nonce_bytes: [u8; 32] = nonce_result.as_ref().try_into()
                .map_err(|_| anyhow::anyhow!("Safe.nonce() did not return 32 bytes"))?;
            let nonce = U256::from_be_slice(&nonce_bytes);
            const SAFE_TX_GAS: u64 = 300_000;
            let get_tx_hash_sig = "getTransactionHash(address,uint256,bytes,uint8,uint256,uint256,uint256,address,address,uint256)";
            let get_tx_hash_selector = keccak256(get_tx_hash_sig.as_bytes()).as_slice()[..4].to_vec();
            let zero_addr = [0u8; 32];
            let mut to_enc = [0u8; 32];
            to_enc[12..].copy_from_slice(ctf_address.as_slice());
            let data_offset_get_hash = U256::from(32u32 * 10u32);
            let mut get_tx_hash_calldata = Vec::new();
            get_tx_hash_calldata.extend_from_slice(&get_tx_hash_selector);
            get_tx_hash_calldata.extend_from_slice(&to_enc);
            get_tx_hash_calldata.extend_from_slice(&U256::ZERO.to_be_bytes::<32>());
            get_tx_hash_calldata.extend_from_slice(&data_offset_get_hash.to_be_bytes::<32>());
            get_tx_hash_calldata.push(0); get_tx_hash_calldata.extend_from_slice(&[0u8; 31]);
            get_tx_hash_calldata.extend_from_slice(&U256::from(SAFE_TX_GAS).to_be_bytes::<32>());
            get_tx_hash_calldata.extend_from_slice(&U256::ZERO.to_be_bytes::<32>());
            get_tx_hash_calldata.extend_from_slice(&U256::ZERO.to_be_bytes::<32>());
            get_tx_hash_calldata.extend_from_slice(&zero_addr);
            get_tx_hash_calldata.extend_from_slice(&zero_addr);
            get_tx_hash_calldata.extend_from_slice(&nonce.to_be_bytes::<32>());
            get_tx_hash_calldata.extend_from_slice(&U256::from(redeem_calldata.len()).to_be_bytes::<32>());
            get_tx_hash_calldata.extend_from_slice(&redeem_calldata);
            let get_tx_hash_tx = TransactionRequest::default()
                .to(safe_address)
                .input(Bytes::from(get_tx_hash_calldata).into());
            let tx_hash_result = provider_read.call(get_tx_hash_tx).await
                .context("Failed to call Safe.getTransactionHash()")?;
            let tx_hash_to_sign: B256 = tx_hash_result.as_ref().try_into()
                .map_err(|_| anyhow::anyhow!("getTransactionHash did not return 32 bytes"))?;
            const EIP191_PREFIX: &[u8] = b"\x19Ethereum Signed Message:\n32";
            let mut eip191_message = Vec::with_capacity(EIP191_PREFIX.len() + 32);
            eip191_message.extend_from_slice(EIP191_PREFIX);
            eip191_message.extend_from_slice(tx_hash_to_sign.as_slice());
            let hash_to_sign = keccak256(&eip191_message);
            let sig = signer.sign_hash(&hash_to_sign).await
                .context("Failed to sign Safe transaction hash")?;
            let sig_bytes = sig.as_bytes();
            let r = &sig_bytes[0..32];
            let s = &sig_bytes[32..64];
            let v = sig_bytes[64];
            let v_safe = if v == 27 || v == 28 { v + 4 } else { v };
            let mut packed_sig: Vec<u8> = Vec::with_capacity(85);
            packed_sig.extend_from_slice(r);
            packed_sig.extend_from_slice(s);
            packed_sig.extend_from_slice(&[v_safe]);
            let get_threshold_selector = keccak256("getThreshold()".as_bytes()).as_slice()[..4].to_vec();
            let threshold_tx = TransactionRequest::default()
                .to(safe_address)
                .input(Bytes::from(get_threshold_selector).into());
            let threshold_result = provider_read.call(threshold_tx).await
                .context("Failed to call Safe.getThreshold()")?;
            let threshold_bytes: [u8; 32] = threshold_result.as_ref().try_into()
                .map_err(|_| anyhow::anyhow!("getThreshold did not return 32 bytes"))?;
            let threshold = U256::from_be_slice(&threshold_bytes);
            if threshold > U256::from(1) {
                let owner = signer.address();
                let mut with_owner = Vec::with_capacity(20 + packed_sig.len());
                with_owner.extend_from_slice(owner.as_slice());
                with_owner.extend_from_slice(&packed_sig);
                packed_sig = with_owner;
            }
            let safe_sig_bytes = packed_sig;
            let exec_sig = "execTransaction(address,uint256,bytes,uint8,uint256,uint256,uint256,address,address,bytes)";
            let exec_selector = keccak256(exec_sig.as_bytes()).as_slice()[..4].to_vec();
            let data_offset = 32u32 * 10u32;
            let sigs_offset = data_offset + 32 + redeem_calldata.len() as u32;
            let mut exec_calldata = Vec::new();
            exec_calldata.extend_from_slice(&exec_selector);
            exec_calldata.extend_from_slice(&to_enc);
            exec_calldata.extend_from_slice(&U256::ZERO.to_be_bytes::<32>());
            exec_calldata.extend_from_slice(&U256::from(data_offset).to_be_bytes::<32>());
            exec_calldata.push(0); exec_calldata.extend_from_slice(&[0u8; 31]);
            exec_calldata.extend_from_slice(&U256::from(SAFE_TX_GAS).to_be_bytes::<32>());
            exec_calldata.extend_from_slice(&U256::ZERO.to_be_bytes::<32>());
            exec_calldata.extend_from_slice(&U256::ZERO.to_be_bytes::<32>());
            exec_calldata.extend_from_slice(&zero_addr);
            exec_calldata.extend_from_slice(&zero_addr);
            exec_calldata.extend_from_slice(&U256::from(sigs_offset).to_be_bytes::<32>());
            exec_calldata.extend_from_slice(&U256::from(redeem_calldata.len()).to_be_bytes::<32>());
            exec_calldata.extend_from_slice(&redeem_calldata);
            exec_calldata.extend_from_slice(&U256::from(safe_sig_bytes.len()).to_be_bytes::<32>());
            exec_calldata.extend_from_slice(&safe_sig_bytes);
            (safe_address, exec_calldata, 400_000u64, true)
        } else if use_proxy && sig_type == 1 {
            eprintln!("   Using proxy wallet: sending redemption via Proxy Wallet Factory");
            let factory_address = parse_address_hex(PROXY_WALLET_FACTORY)
                .context("Failed to parse Proxy Wallet Factory address")?;
            let selector = keccak256("proxy((uint8,address,uint256,bytes)[])".as_bytes());
            let proxy_selector = &selector.as_slice()[..4];
            let mut proxy_calldata = Vec::with_capacity(4 + 32 * 3 + 128 + 32 + redeem_calldata.len());
            proxy_calldata.extend_from_slice(proxy_selector);
            proxy_calldata.extend_from_slice(&U256::from(32u32).to_be_bytes::<32>());
            proxy_calldata.extend_from_slice(&U256::from(1u32).to_be_bytes::<32>());
            proxy_calldata.extend_from_slice(&U256::from(96u32).to_be_bytes::<32>());
            let mut type_code = [0u8; 32];
            type_code[31] = 1;
            proxy_calldata.extend_from_slice(&type_code);
            let mut to_bytes = [0u8; 32];
            to_bytes[12..].copy_from_slice(ctf_address.as_slice());
            proxy_calldata.extend_from_slice(&to_bytes);
            proxy_calldata.extend_from_slice(&U256::ZERO.to_be_bytes::<32>());
            proxy_calldata.extend_from_slice(&U256::from(128u32).to_be_bytes::<32>());
            let data_len = redeem_calldata.len();
            proxy_calldata.extend_from_slice(&U256::from(data_len).to_be_bytes::<32>());
            proxy_calldata.extend_from_slice(&redeem_calldata);
            (factory_address, proxy_calldata, 400_000u64, false)
        } else {
            eprintln!("   Sending redemption from EOA to CTF contract");
            (ctf_address, redeem_calldata, 300_000, false)
        };

        // Try each RPC URL for sending the redemption transaction
        let redeem_urls: Vec<&str> = if self.rpc_urls.is_empty() {
            vec!["https://polygon-rpc.com"]
        } else {
            self.rpc_urls.iter().map(|s| s.as_str()).collect()
        };

        let mut last_redeem_err = anyhow::anyhow!("no RPC URLs configured for redemption");

        for redeem_rpc_url in &redeem_urls {
            let provider = match ProviderBuilder::new()
                .wallet(signer.clone())
                .connect(*redeem_rpc_url)
                .await
            {
                Ok(p) => p,
                Err(e) => {
                    warn!("Redemption: connect to {} failed: {}", redeem_rpc_url, e);
                    last_redeem_err = anyhow::anyhow!("connect to {} failed: {}", redeem_rpc_url, e);
                    continue;
                }
            };

            let tx_request = TransactionRequest {
                to: Some(alloy::primitives::TxKind::Call(tx_to)),
                input: Bytes::from(tx_data.clone()).into(),
                value: Some(U256::ZERO),
                gas: Some(gas_limit),
                ..Default::default()
            };

            let pending_tx = match provider.send_transaction(tx_request).await {
                Ok(tx) => tx,
                Err(e) => {
                    warn!("Redemption: send via {} failed: {}", redeem_rpc_url, e);
                    last_redeem_err = anyhow::anyhow!("send via {} failed: {}", redeem_rpc_url, e);
                    continue;
                }
            };

            // Transaction sent — do NOT retry from here (tx may be on chain)
            let tx_hash = *pending_tx.tx_hash();
            eprintln!("   Transaction sent via {}, waiting for confirmation...", redeem_rpc_url);
            eprintln!("   Transaction hash: {:?}", tx_hash);

            let receipt = pending_tx.get_receipt().await
                .context("Failed to get transaction receipt")?;

            if !receipt.status() {
                anyhow::bail!("Redemption transaction failed. Transaction hash: {:?}", tx_hash);
            }

            if used_safe_redemption {
                let payout_redemption_topic = keccak256(
                    b"PayoutRedemption(address,address,bytes32,bytes32,uint256[],uint256)"
                );
                let logs = receipt.logs();
                let ctf_has_payout = logs.iter().any(|log| {
                    log.address() == ctf_address && log.topics().first().map(|t| t.as_slice()) == Some(payout_redemption_topic.as_slice())
                });
                if !ctf_has_payout {
                    anyhow::bail!(
                        "Redemption tx was mined but the inner redeem reverted (no PayoutRedemption from CTF). \
                        Check that the Safe holds the winning tokens and conditionId/indexSet are correct. Tx: {:?}",
                        tx_hash
                    );
                }
            }

            let redeem_response = RedeemResponse {
                success: true,
                message: Some(format!("Successfully redeemed tokens. Transaction: {:?}", tx_hash)),
                transaction_hash: Some(format!("{:?}", tx_hash)),
                amount_redeemed: None,
            };
            eprintln!("Successfully redeemed winning tokens!");
            eprintln!("Transaction hash: {:?}", tx_hash);
            if let Some(block_number) = receipt.block_number {
                eprintln!("Block number: {}", block_number);
            }
            return Ok(redeem_response);
        }

        Err(last_redeem_err)
    }

    /// Fetch latest price from a Chainlink aggregator via eth_call (latestRoundData).
    /// Tries each configured RPC URL in order until one succeeds.
    /// Returns (price_usd, updated_at_unix_secs).
    pub async fn get_chainlink_price_rpc(
        &self,
        symbol: &str,
    ) -> Result<(f64, u64)> {
        let aggregator = chainlink_aggregator_address(symbol)
            .ok_or_else(|| anyhow::anyhow!("No Chainlink aggregator for symbol: {}", symbol))?;

        let urls: Vec<&str> = if self.rpc_urls.is_empty() {
            vec!["https://polygon-rpc.com"]
        } else {
            self.rpc_urls.iter().map(|s| s.as_str()).collect()
        };

        let selector = keccak256(b"latestRoundData()");
        let data = format!("0x{}", hex::encode(&selector.as_slice()[..4]));
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "eth_call",
            "params": [{"to": aggregator, "data": &data}, "latest"],
            "id": 1
        });

        let mut last_err = anyhow::anyhow!("no RPC URLs configured");

        for rpc_url in &urls {
            match self.try_chainlink_rpc(rpc_url, &body, symbol).await {
                Ok(result) => return Ok(result),
                Err(e) => {
                    warn!("Chainlink RPC {} failed on {}: {}", symbol, rpc_url, e);
                    last_err = e;
                }
            }
        }

        Err(last_err)
    }

    async fn try_chainlink_rpc(
        &self,
        rpc_url: &str,
        body: &Value,
        symbol: &str,
    ) -> Result<(f64, u64)> {
        let response = self.client
            .post(rpc_url)
            .json(body)
            .send()
            .await
            .context(format!("Chainlink RPC request to {} failed", rpc_url))?;

        let status = response.status();
        let text = response.text().await.context("Read Chainlink RPC body")?;
        let json: Value = serde_json::from_str(&text)
            .context(format!("Parse Chainlink RPC response (status={}) from {}", status, rpc_url))?;

        if let Some(err) = json.get("error") {
            anyhow::bail!("Chainlink RPC error: {} (status={}) from {}", err, status, rpc_url);
        }

        let hex_result = json
            .get("result")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("No 'result' in Chainlink RPC response from {}", rpc_url))?;
        let hex_result = hex_result.strip_prefix("0x").unwrap_or(hex_result);

        if hex_result.len() < 64 * 5 {
            anyhow::bail!("Chainlink result too short: {} hex chars (need 320) from {}", hex_result.len(), rpc_url);
        }

        let raw = hex::decode(hex_result).context("Hex decode Chainlink result")?;
        let answer_slice = raw.get(32..64)
            .ok_or_else(|| anyhow::anyhow!("Answer slice out of bounds (raw len={})", raw.len()))?;
        let answer = i128::from_be_bytes(
            answer_slice[16..32].try_into().context("Answer bytes")?
        );
        let price = (answer as f64) / 100_000_000.0; // 8 decimals

        let updated_slice = raw.get(96..128)
            .ok_or_else(|| anyhow::anyhow!("updatedAt slice out of bounds"))?;
        let updated_at = u64::from_be_bytes(
            updated_slice[24..32].try_into().context("updatedAt bytes")?
        );

        info!("Chainlink RPC {}: ${} (updatedAt={}) via {}", symbol, price, updated_at, rpc_url);
        Ok((price, updated_at))
    }
}

/// Chainlink aggregator proxy addresses on Polygon mainnet (8 decimals).
fn chainlink_aggregator_address(symbol: &str) -> Option<&'static str> {
    match symbol.to_lowercase().as_str() {
        "btc" => Some("0xc907E116054Ad103354f2D350FD2514433D57F6f"),
        "eth" => Some("0xF9680D99D6C9589e2a93a78A04A279e509205945"),
        "sol" => Some("0x10C8264C0935b3B9870013e057f330Ff3e9C56dC"),
        "xrp" => Some("0x785ba89291f676b5386652eB12b30cF361020694"),
        _ => None,
    }
}
