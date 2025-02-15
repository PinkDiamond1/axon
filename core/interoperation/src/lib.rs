#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::error::Error;
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use ckb_vm::machine::{asm::AsmCoreMachine, DefaultMachineBuilder, SupportMachine, VERSION1};
use ckb_vm::{Error as VMError, ISA_B, ISA_IMC, ISA_MOP};

use protocol::traits::{CkbClient, Context, Interoperation};
use protocol::types::{Bytes, SignedTransaction, VMResp, H256};
use protocol::{tokio::time::sleep, Display, ProtocolError, ProtocolErrorKind, ProtocolResult};

lazy_static::lazy_static! {
    static ref DISPATCHER: ArcSwap<ProgramDispatcher> = ArcSwap::from_pointee(ProgramDispatcher::default());
    static ref TRANSACTION_HASH_MAP: ArcSwap<HashMap<u8, H256>> = ArcSwap::from_pointee(HashMap::new());
}

const PULL_CKB_TX_INTERVAL: u64 = 10; // second
const ISA: u8 = ISA_IMC | ISA_B | ISA_MOP;
const GAS_TO_CYCLE_COEF: u64 = 6_000;

pub const fn gas_to_cycle(gas: u64) -> u64 {
    gas * GAS_TO_CYCLE_COEF
}

pub const fn cycle_to_gas(cycle: u64) -> u64 {
    cycle / GAS_TO_CYCLE_COEF
}

pub enum BlockchainType {
    Ethereum,
    Other(u8),
}

impl From<u8> for BlockchainType {
    fn from(s: u8) -> Self {
        match s {
            0 | 1 => BlockchainType::Ethereum,
            _ => BlockchainType::Other(s),
        }
    }
}

#[derive(Default, Clone)]
pub struct InteroperationImpl;

impl Interoperation for InteroperationImpl {
    fn verify_external_signature(
        &self,
        _ctx: Context,
        tx: SignedTransaction,
    ) -> ProtocolResult<()> {
        let _sig_type = BlockchainType::from(
            tx.transaction
                .signature
                .ok_or(InteroperationError::MissingSignature)?
                .standard_v,
        );

        Ok(())
    }

    fn call_ckb_vm(
        &self,
        _ctx: Context,
        tx_hash: H256,
        args: &[Bytes],
        max_cycles: u64,
    ) -> ProtocolResult<VMResp> {
        let machine =
            DefaultMachineBuilder::new(AsmCoreMachine::new(ISA, VERSION1, max_cycles)).build();
        let program = DISPATCHER.load().get_program(&tx_hash)?;

        #[cfg(not(target_arch = "aarch64"))]
        let mut vm = ckb_vm_aot::AotMachine::new(machine, Some(&(*program.aot)));

        #[cfg(target_arch = "aarch64")]
        let mut vm = ckb_vm::machine::asm::AsmMachine::new(machine);

        let _ = vm
            .load_program(&program.code, args)
            .map_err(InteroperationError::CkbVM)?;

        Ok(VMResp {
            exit_code: vm.run().map_err(InteroperationError::CkbVM)?,
            cycles:    vm.machine.cycles(),
        })
    }
}

impl InteroperationImpl {
    pub async fn new<T: CkbClient>(
        transaction_hash_map: HashMap<u8, H256>,
        rpc_client: Arc<T>,
    ) -> ProtocolResult<Self> {
        let tx_hashes = transaction_hash_map.iter().map(|(_, v)| *v).collect();
        init_dispatcher_from_rpc(rpc_client, tx_hashes).await?;
        init_ckb_transaction_hashes(transaction_hash_map);
        Ok(InteroperationImpl::default())
    }
}

async fn init_dispatcher_from_rpc<T: CkbClient>(
    rpc_client: Arc<T>,
    tx_hashes: Vec<H256>,
) -> ProtocolResult<()> {
    let ckb_hashes = tx_hashes
        .into_iter()
        .map(|hash| ckb_types::H256(hash.0))
        .collect::<Vec<_>>();
    let transactions = loop {
        match rpc_client
            .get_txs_by_hashes(Default::default(), ckb_hashes.clone())
            .await
        {
            Ok(v) => break v.into_iter().map(|v| v.unwrap()),
            Err(e) => {
                log::debug!("Get tx from ckb err {:?}", e);
                sleep(Duration::from_secs(PULL_CKB_TX_INTERVAL)).await;
            }
        }
    };

    let program_map = transactions
        .into_iter()
        .map(|tx| {
            (
                H256(tx.hash.0),
                tx.inner.outputs_data[0].clone().into_bytes(),
            )
        })
        .collect::<HashMap<_, _>>();

    init_dispatcher(program_map)?;
    Ok(())
}

fn init_dispatcher(program_map: HashMap<H256, Bytes>) -> ProtocolResult<()> {
    let program_num = program_map.len();
    DISPATCHER.swap(Arc::new(ProgramDispatcher::new(program_map)?));
    log::info!(
        "[interoperation]: init dispatcher success, program number {:?}",
        program_num
    );
    Ok(())
}

fn init_ckb_transaction_hashes(hashes: HashMap<u8, H256>) {
    TRANSACTION_HASH_MAP.swap(Arc::new(hashes));
}

pub fn get_ckb_transaction_hash(blockchain_id: u8) -> ProtocolResult<H256> {
    if let Some(tx_hash) = TRANSACTION_HASH_MAP.load().get(&blockchain_id) {
        Ok(*tx_hash)
    } else {
        Err(InteroperationError::GetBlockchainCodeHash(blockchain_id).into())
    }
}

#[derive(Default)]
struct ProgramDispatcher(HashMap<H256, Program>);

impl ProgramDispatcher {
    #[cfg(not(target_arch = "aarch64"))]
    fn new(program_map: HashMap<H256, Bytes>) -> ProtocolResult<Self> {
        let mut inner = HashMap::with_capacity(program_map.len());

        for (tx_hash, code) in program_map.into_iter() {
            let aot_code = ckb_vm_aot::AotCompilingMachine::load(&code, None, ISA, VERSION1)
                .and_then(|mut m| m.compile())
                .map_err(InteroperationError::CkbVM)?;
            inner.insert(tx_hash, Program::new(code, aot_code));
        }

        Ok(ProgramDispatcher(inner))
    }

    #[cfg(target_arch = "aarch64")]
    fn new(program_map: HashMap<H256, Bytes>) -> ProtocolResult<Self> {
        Ok(ProgramDispatcher(
            program_map
                .into_iter()
                .map(|kv| (kv.0, Program::new(kv.1)))
                .collect(),
        ))
    }

    fn get_program(&self, tx_hash: &H256) -> ProtocolResult<Program> {
        self.0
            .get(tx_hash)
            .cloned()
            .ok_or_else(|| InteroperationError::GetProgram(*tx_hash).into())
    }
}

#[derive(Clone)]
struct Program {
    code: Bytes,
    #[cfg(not(target_arch = "aarch64"))]
    aot:  Arc<ckb_vm_aot::AotCode>,
}

impl Program {
    #[cfg(not(target_arch = "aarch64"))]
    fn new(code: Bytes, aot: ckb_vm_aot::AotCode) -> Self {
        Program {
            code,
            aot: Arc::new(aot),
        }
    }

    #[cfg(target_arch = "aarch64")]
    fn new(code: Bytes) -> Self {
        Program { code }
    }
}

#[derive(Debug, Display)]
pub enum InteroperationError {
    #[display(fmt = "Transaction missing signature")]
    MissingSignature,

    #[display(fmt = "Cannot get program of transaction hash {:?}", _0)]
    GetProgram(H256),

    #[display(fmt = "CKB VM run failed {:?}", _0)]
    CkbVM(VMError),

    #[display(fmt = "Unsupported blockchain id {:?}", _0)]
    GetBlockchainCodeHash(u8),
}

impl Error for InteroperationError {}

impl From<InteroperationError> for ProtocolError {
    fn from(error: InteroperationError) -> ProtocolError {
        ProtocolError::new(ProtocolErrorKind::Interoperation, Box::new(error))
    }
}
