use crate::collector::Collector;
use crate::deposition::fetch_deposition_requests;
use crate::jsonrpc_types::collector::QueryParam;
use crate::tx_pool::TxPool;
use std::time::SystemTime;

use anyhow::{anyhow, Result};
use ckb_types::{
    bytes::Bytes,
    core::ScriptHashType,
    packed::{CellOutput, RawTransaction, Script, Transaction, WitnessArgs, WitnessArgsReader},
    prelude::*,
};
use gw_common::{
    smt::{Store, H256, SMT},
    state::State,
    CKB_TOKEN_ID, DEPOSITION_CODE_HASH, SUDT_CODE_HASH,
};
use gw_generator::{
    generator::{DepositionRequest, StateTransitionArgs},
    syscalls::GetContractCode,
    Generator,
};
use gw_types::{
    packed::{
        DepositionLockArgs, DepositionLockArgsReader, L2Block, L2BlockReader, RawL2Block, Uint32,
        Uint64,
    },
    prelude::{Pack as GWPack, Unpack as GWUnpack},
};

pub struct Signer {
    account_id: u32,
}

pub struct HeaderInfo {
    pub number: u64,
    pub block_hash: [u8; 32],
}

pub struct Chain<S, C, CS> {
    state: S,
    collector: C,
    rollup_type_script: Script,
    last_synced: HeaderInfo,
    tip: RawL2Block,
    generator: Generator<CS>,
    tx_pool: TxPool<S, CS>,
    signer: Option<Signer>,
}

impl<S: State, C: Collector, CS: GetContractCode> Chain<S, C, CS> {
    pub fn new(
        state: S,
        tip: RawL2Block,
        last_synced: HeaderInfo,
        rollup_type_script: Script,
        collector: C,
        code_store: CS,
        tx_pool: TxPool<S, CS>,
        signer: Option<Signer>,
    ) -> Self {
        let generator = Generator::new(code_store);
        Chain {
            state,
            collector,
            rollup_type_script,
            last_synced,
            tip,
            generator,
            tx_pool,
            signer,
        }
    }

    /// Sync chain from layer1
    pub fn sync(&mut self) -> Result<()> {
        // TODO handle rollback
        if self
            .collector
            .get_header(&self.last_synced.block_hash)?
            .is_none()
        {
            panic!("layer1 chain has forked!")
        }
        // query state update tx from collector
        let param = QueryParam {
            type_: Some(self.rollup_type_script.clone().into()),
            from_block: Some(self.last_synced.number.into()),
            ..Default::default()
        };
        let txs = self.collector.query_transactions(param)?;
        // apply tx to state
        for tx_info in txs {
            let header = self
                .collector
                .get_header(&tx_info.block_hash)?
                .expect("should not panic unless the chain is forking");
            let block_number: u64 = header.raw().number().unpack();
            assert!(
                block_number > self.last_synced.number,
                "must greater than last synced number"
            );

            // parse layer2 block
            let rollup_id = self.rollup_type_script.calc_script_hash().unpack();
            let l2block = parse_l2block(&tx_info.transaction, &rollup_id)?;

            let tip_number: u64 = self.tip.number().unpack();
            assert!(
                l2block.raw().number().unpack() == tip_number + 1,
                "new l2block number must be the successor of the tip"
            );

            // process l2block
            self.process_block(l2block.clone(), &tx_info.transaction.raw(), &rollup_id)?;

            // update chain
            self.last_synced = HeaderInfo {
                number: header.raw().number().unpack(),
                block_hash: header.calc_header_hash().unpack(),
            };
            self.tip = l2block.raw();
            self.tx_pool.update_tip(&l2block, unreachable!());
        }
        Ok(())
    }

    /// Produce a new block
    ///
    /// This function should be called in the turn that the current aggregator to produce the next block,
    /// otherwise the produced block may invalided by the state-validator contract.
    fn produce_block(
        &mut self,
        signer: &Signer,
        deposition_requests: Vec<DepositionRequest>,
    ) -> Result<RawL2Block> {
        // take txs from tx pool
        // produce block
        let pkg = self.tx_pool.package_txs()?;
        let parent_number: u64 = self.tip.number().unpack();
        let number = parent_number + 1;
        let aggregator_id: u32 = signer.account_id;
        let timestamp: u64 = unixtime()?;
        let submit_txs = unreachable!();
        let post_account = unreachable!();
        let prev_account = unreachable!();
        let raw_block = RawL2Block::new_builder()
            .number(GWPack::<Uint64>::pack(&number))
            .aggregator_id(GWPack::<Uint32>::pack(&aggregator_id))
            .timestamp(GWPack::<Uint64>::pack(&timestamp))
            .post_account(post_account)
            .prev_account(prev_account)
            .submit_transactions(submit_txs)
            .valid(1.into())
            .build();
        Ok(raw_block)
    }

    fn process_block(
        &mut self,
        l2block: L2Block,
        tx: &RawTransaction,
        rollup_id: &[u8; 32],
    ) -> Result<()> {
        let deposition_requests = fetch_deposition_requests(&self.collector, tx, rollup_id)?;
        let args = StateTransitionArgs {
            l2block,
            deposition_requests,
        };
        self.generator
            .apply_state_transition(&mut self.state, args)?;
        Ok(())
    }
}

fn unixtime() -> Result<u64> {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .map_err(Into::into)
}

fn parse_l2block(tx: &Transaction, rollup_id: &[u8; 32]) -> Result<L2Block> {
    // find rollup state cell from outputs
    let (i, _) = tx
        .raw()
        .outputs()
        .into_iter()
        .enumerate()
        .find(|(_i, output)| {
            output
                .type_()
                .to_opt()
                .map(|type_| type_.calc_script_hash().unpack())
                .as_ref()
                == Some(rollup_id)
        })
        .ok_or_else(|| anyhow!("no rollup cell found"))?;

    let witness: Bytes = tx
        .witnesses()
        .get(i)
        .ok_or_else(|| anyhow!("no witness"))?
        .unpack();
    let witness_args = match WitnessArgsReader::verify(&witness, false) {
        Ok(_) => WitnessArgs::new_unchecked(witness),
        Err(_) => {
            return Err(anyhow!("invalid witness"));
        }
    };
    let output_type: Bytes = witness_args
        .output_type()
        .to_opt()
        .ok_or_else(|| anyhow!("output_type field is none"))?
        .unpack();
    match L2BlockReader::verify(&output_type, false) {
        Ok(_) => Ok(L2Block::new_unchecked(output_type)),
        Err(_) => Err(anyhow!("invalid l2block")),
    }
}