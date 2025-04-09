use std::{str::FromStr, time::Instant, sync::Arc};

use anyhow::anyhow;
use solana_client::rpc_config::RpcSimulateTransactionConfig;
use solana_sdk::{
    commitment_config::CommitmentConfig, compute_budget::ComputeBudgetInstruction, instruction::Instruction, message::{v0, VersionedMessage}, native_token::sol_to_lamports, pubkey::Pubkey, signature::{Keypair, Signature}, signer::Signer, system_instruction, transaction::{Transaction, VersionedTransaction}
};
use spl_associated_token_account::{
    instruction::create_associated_token_account,
};

use crate::{
    common::{PriorityFee, SolanaRpcClient}, constants, instruction, 
    ipfs::TokenMetadataIPFS,  jito::FeeClient,
    pumpfun::buy::build_buy_transaction_with_tip
};

use crate::pumpfun::common::{
    create_priority_fee_instructions, 
    get_buy_amount_with_slippage, get_global_account
};

/// Create a new token
pub async fn create(
    rpc: Arc<SolanaRpcClient>,
    payer: Arc<Keypair>,
    mint: Keypair,
    ipfs: TokenMetadataIPFS,
    priority_fee: PriorityFee,
) -> Result<(), anyhow::Error> {
    let mut instructions = create_priority_fee_instructions(priority_fee);

    instructions.push(instruction::create(
        payer.as_ref(),
        &mint,
        instruction::Create {
            _name: ipfs.metadata.name,
            _symbol: ipfs.metadata.symbol,
            _uri: ipfs.metadata_uri,
            payer_pubkey: payer.pubkey(),
        },
    ));

    let recent_blockhash = rpc.get_latest_blockhash().await?;
    let transaction = Transaction::new_signed_with_payer(
        &instructions,
        Some(&payer.pubkey()),
        &[payer.as_ref(), &mint],
        recent_blockhash,
    );

    rpc.send_and_confirm_transaction(&transaction).await?;

    Ok(())
}

/// Create and buy tokens in one transaction
pub async fn create_and_buy(
    rpc: Arc<SolanaRpcClient>,
    payer: Arc<Keypair>,
    mint: Keypair,
    ipfs: TokenMetadataIPFS,
    amount_sol: u64,
    slippage_basis_points: Option<u64>,
    priority_fee: PriorityFee,
) -> Result<(), anyhow::Error> {
    if amount_sol == 0 {
        return Err(anyhow!("Amount cannot be zero"));
    }

    let mint = Arc::new(mint);
    let transaction = build_create_and_buy_transaction(rpc.clone(), payer.clone(), mint.clone(), ipfs, amount_sol, slippage_basis_points, priority_fee.clone()).await?;
    rpc.send_and_confirm_transaction(&transaction).await?;

    Ok(())
}

pub async fn create_and_buy_with_tip(
    rpc: Arc<SolanaRpcClient>,
    fee_clients: Vec<Arc<FeeClient>>,
    payer: Arc<Keypair>,
    mint: Keypair,
    ipfs: TokenMetadataIPFS,
    amount_sol: u64,
    slippage_basis_points: Option<u64>,
    priority_fee: PriorityFee,
) -> Result<Signature, anyhow::Error> {
    let start_time = Instant::now();
    let mint = Arc::new(mint);
    let build_instructions = build_create_and_buy_instructions(rpc.clone(), payer.clone(), mint.clone(), ipfs.clone(), amount_sol, slippage_basis_points, priority_fee.clone()).await?;
    
    let tip_account = if let Some(first_client) = fee_clients.first() {
        match first_client.get_tip_account().await {
            Ok(acc_str) => match Pubkey::from_str(&acc_str) {
                Ok(acc) => Some(Arc::new(acc)),
                Err(e) => {
                    println!("Warning: Failed to parse tip account pubkey '{}': {}. Proceeding without tip.", acc_str, e);
                    None
                }
            },
            Err(e) => {
                println!("Warning: Failed to get tip account: {}. Proceeding without tip.", e);
                None
            }
        }
    } else {
        println!("Warning: No fee clients provided. Proceeding without tip.");
        None
    };

    let transaction = build_create_and_buy_transaction_with_tip(
        rpc.clone(),
        tip_account,
        payer.clone(),
        mint.clone(),
        priority_fee.clone(),
        build_instructions
    ).await?;

    println!("Transaction built. Submitting and awaiting confirmation...");

    let signature = transaction.signatures[0];
    println!("Transaction signature: {}", signature);

    let confirmation_result = rpc.send_and_confirm_transaction_with_spinner(&transaction).await;

    match confirmation_result {
        Ok(confirmed_signature) => {
            if confirmed_signature != signature {
                 println!("Warning: Confirmed signature {} differs from initial signature {}", confirmed_signature, signature);
                 println!("Total create, buy, and confirm operation time: {:?}ms", start_time.elapsed().as_millis());
                 Ok(confirmed_signature)
            } else {
                 println!("Transaction confirmed successfully!");
                 println!("Total create, buy, and confirm operation time: {:?}ms", start_time.elapsed().as_millis());
                 Ok(signature)
            }
        }
        Err(e) => {
            println!("Error sending/confirming transaction: {}", e);
             if let Some(tx_error) = e.get_transaction_error() {
                 println!("Transaction error details: {:?}", tx_error);
             }
             Err(anyhow!("Failed to send or confirm transaction: {}", e))
        }
    }
}

pub async fn build_create_and_buy_transaction(
    rpc: Arc<SolanaRpcClient>,
    payer: Arc<Keypair>,
    mint: Arc<Keypair>,
    ipfs: TokenMetadataIPFS,
    amount_sol: u64,
    slippage_basis_points: Option<u64>,
    priority_fee: PriorityFee,
) -> Result<Transaction, anyhow::Error> {
    let mut instructions = vec![
        ComputeBudgetInstruction::set_compute_unit_price(priority_fee.unit_price),
        ComputeBudgetInstruction::set_compute_unit_limit(priority_fee.unit_limit),
    ];

    let build_instructions = build_create_and_buy_instructions(rpc.clone(), payer.clone(), mint.clone(), ipfs, amount_sol, slippage_basis_points, priority_fee.clone()).await?;
    instructions.extend(build_instructions);

    let recent_blockhash = rpc.get_latest_blockhash().await?;
    let transaction = Transaction::new_signed_with_payer(
        &instructions,
        Some(&payer.pubkey()),
        &[payer.as_ref(), mint.as_ref()],
        recent_blockhash,
    );

    Ok(transaction)
}

pub async fn build_create_and_buy_transaction_with_tip(
    rpc: Arc<SolanaRpcClient>,
    tip_account: Option<Arc<Pubkey>>,
    payer: Arc<Keypair>,
    mint: Arc<Keypair>,
    priority_fee: PriorityFee,
    build_instructions: Vec<Instruction>,
) -> Result<VersionedTransaction, anyhow::Error> {
    const INCREASED_COMPUTE_LIMIT: u32 = 600_000; // Increased CU Limit

    let mut instructions = vec![
        ComputeBudgetInstruction::set_compute_unit_price(priority_fee.unit_price),
        ComputeBudgetInstruction::set_compute_unit_limit(INCREASED_COMPUTE_LIMIT), 
    ];

    if let Some(tip_acc) = tip_account {
         instructions.push(
             system_instruction::transfer(
                 &payer.pubkey(),
                 &tip_acc,
                 sol_to_lamports(priority_fee.buy_tip_fee),
             )
         );
         println!("Added tip instruction for account: {}", tip_acc);
    } else {
         println!("No tip account provided, skipping tip instruction.");
    }

    instructions.extend(build_instructions);

    let recent_blockhash = rpc.get_latest_blockhash().await?;
    let v0_message: v0::Message =
        v0::Message::try_compile(&payer.pubkey(), &instructions, &[], recent_blockhash)?;

    let versioned_message: VersionedMessage = VersionedMessage::V0(v0_message);
    let transaction = VersionedTransaction::try_new(versioned_message, &[payer.as_ref(), mint.as_ref()])?;
    println!("Transaction built and signed by payer {} and mint {}", payer.pubkey(), mint.pubkey());

    Ok(transaction)
}

pub async fn build_create_and_buy_instructions(
    rpc: Arc<SolanaRpcClient>,
    payer: Arc<Keypair>,
    mint: Arc<Keypair>,
    ipfs: TokenMetadataIPFS,
    amount_sol: u64,
    slippage_basis_points: Option<u64>,
    priority_fee: PriorityFee,
) -> Result<Vec<Instruction>, anyhow::Error> {
    if amount_sol == 0 {
        return Err(anyhow!("Amount cannot be zero"));
    }

    let rpc = rpc.as_ref();
    let global_account = get_global_account(rpc).await?;
    let buy_amount = global_account.get_initial_buy_price(amount_sol);
    let buy_amount_with_slippage =
        get_buy_amount_with_slippage(amount_sol, slippage_basis_points);

    let mut instructions = vec![];

    println!("SDK creating token with name='{}', symbol='{}', uri='{}'", 
             ipfs.metadata.name, ipfs.metadata.symbol, ipfs.metadata_uri);
    
    let original_name = ipfs.metadata.name.clone();
    let original_symbol = ipfs.metadata.symbol.clone();
    
    instructions.push(instruction::create(
        payer.as_ref(),
        mint.as_ref(),
        instruction::Create {
            _name: original_name,
            _symbol: original_symbol,
            _uri: ipfs.metadata_uri.clone(),
            payer_pubkey: payer.pubkey(),
        },
    ));

    instructions.push(create_associated_token_account(
        &payer.pubkey(),
        &payer.pubkey(),
        &mint.pubkey(),
        &constants::accounts::TOKEN_PROGRAM,
    ));

    instructions.push(instruction::buy(
        payer.as_ref(),
        &mint.pubkey(),
        &global_account.fee_recipient,
        instruction::Buy {
            _amount: buy_amount,
            _max_sol_cost: buy_amount_with_slippage,
        },
    ));

    Ok(instructions)
}
