//! >> Keywords <<
//!     -> PrevTransition - The transition that makes the state-machine move to current state
//!     -> OEW (On Enter the Watcher) - If we enter to this state, the watcher must...
//!
//! >> State <<
//!     Init
//!     WithAliceKey
//!         -> Alice are able to get contract
//!     ContractMatch
//!     VerifiedEncSig
//!         -> OEW
//!             -> Send bch to SwapLock contract
//!             -> get the current xmr block. Will be used for `restore block`
//!     MoneroLocked
//!         -> OEW
//!             -> Watch the SwapLock contract if it is send to alice address
//!                 If it does, get decsig, Transition::DecSig
//!     SwapSuccess(monero::KeyPair, restore_height: u64)

use std::{fmt, time::Duration};

use anyhow::bail;
use bitcoin_hashes::{sha256::Hash as sha256, Hash};
use bitcoincash::{
    consensus::Encodable, PackedLockTime, Script, Sequence, Transaction, TxIn, TxOut,
};
use ecdsa_fun::adaptor::EncryptedSignature;
use hex::ToHex;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::{sync::Mutex, time::sleep};

use crate::{
    adaptor_signature::AdaptorSignature,
    bitcoincash::{secp256k1::ecdsa, OutPoint},
    blockchain::{scan_address_conf_tx, TcpElectrum},
    contract::{ContractPair, TransactionType},
    keys::{KeyPublic, KeyPublicWithoutProof},
    proof,
    protocol::{Action, Error, Swap, SwapEvents, Transition},
    utils::{monero_key_pair, monero_view_pair},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Value0 {
    alice_keys: KeyPublicWithoutProof,
    #[serde(with = "hex")]
    alice_bch_recv: Vec<u8>,
    contract_pair: ContractPair,
    #[serde(with = "monero_view_pair")]
    pub shared_keypair: monero::ViewPair,
    xmr_restore_height: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Value1 {
    alice_keys: KeyPublicWithoutProof,
    #[serde(with = "hex")]
    alice_bch_recv: Vec<u8>,
    contract_pair: ContractPair,
    #[serde(with = "monero_view_pair")]
    pub shared_keypair: monero::ViewPair,
    xmr_restore_height: u64,
    dec_sig: ecdsa::Signature,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Value2 {
    alice_keys: KeyPublicWithoutProof,
    #[serde(with = "hex")]
    alice_bch_recv: Vec<u8>,
    // contract_pair: ContractPair,
    #[serde(with = "monero_view_pair")]
    shared_keypair: monero::ViewPair,
    xmr_restore_height: u64,
    dec_sig: ecdsa::Signature,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Value3 {
    alice_keys: KeyPublicWithoutProof,
    #[serde(with = "hex")]
    alice_bch_recv: Vec<u8>,
    contract_pair: ContractPair,
    #[serde(with = "monero_view_pair")]
    pub shared_keypair: monero::ViewPair,
    xmr_restore_height: u64,
    dec_sig: ecdsa::Signature,
    outpoint: OutPoint,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum State {
    Init,
    WithAliceKey(Value0),
    ContractMatch(Value0),
    VerifiedEncSig(Value1),
    MoneroLocked(Value2),
    ProceedRefund(Value3),
    SwapSuccess(#[serde(with = "monero_key_pair")] monero::KeyPair, u64),
}

impl fmt::Display for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            State::Init => write!(f, "BobState::Init"),
            State::WithAliceKey(_) => write!(f, "BobState::WithAliceKey"),
            State::ContractMatch(_) => write!(f, "BobState::ContractMatch"),
            State::VerifiedEncSig(_) => write!(f, "BobState::VerifiedEncSig"),
            State::MoneroLocked(_) => write!(f, "BobState::MoneroLocked"),
            State::SwapSuccess(_, _) => write!(f, "BobState::SwapSuccess"),
            State::ProceedRefund(_) => write!(f, "BobState::ProceedRefund"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bob {
    pub state: State,
    pub swap: Swap,
}

impl Bob {
    pub fn new(swap: Swap) -> Self {
        Bob {
            state: State::Init,
            swap,
        }
    }

    pub fn get_public_keys(&self) -> KeyPublic {
        KeyPublic::from(self.swap.keys.clone())
    }

    pub fn get_contract(&self) -> Option<(String, monero::Address)> {
        let props = match &self.state {
            State::WithAliceKey(props) => props,
            State::ContractMatch(props) => props,
            _ => return None,
        };

        Some((
            props.contract_pair.swaplock.cash_address(),
            monero::Address::from_viewpair(self.swap.xmr_network, &props.shared_keypair),
        ))
    }

    pub fn get_swaplock_enc_sig(&self) -> Option<EncryptedSignature> {
        if let State::MoneroLocked(props) = &self.state {
            let hash = sha256::hash(&props.alice_bch_recv).to_byte_array();
            let hash = sha256::hash(&hash).to_byte_array();
            let enc_sig = AdaptorSignature::encrypted_sign(
                &self.swap.keys.ves,
                &props.alice_keys.spend_bch,
                &hash,
            );

            return Some(enc_sig);
        }

        return None;
    }

    pub fn get_contract_pair(&self) -> Option<ContractPair> {
        match self.state.clone() {
            State::WithAliceKey(v) => Some(v.contract_pair),
            State::ContractMatch(v) => Some(v.contract_pair),
            State::VerifiedEncSig(v) => Some(v.contract_pair),
            _ => None,
        }
    }

    pub fn refund(&self) -> Option<(Transaction, Transaction)> {
        if let State::ProceedRefund(props) = &self.state {
            let mining_fee = props.contract_pair.mining_fee;

            let tx1 = {
                let unlocker = props.contract_pair.swaplock.unlocking_script(&[]);
                Transaction {
                    version: 2,
                    lock_time: PackedLockTime(0), // TODO: Should we use current time?
                    input: vec![TxIn {
                        sequence: Sequence(props.contract_pair.swaplock.timelock),
                        previous_output: props.outpoint,
                        script_sig: Script::from(unlocker),
                        ..Default::default()
                    }],
                    output: vec![TxOut {
                        value: self.swap.bch_amount.to_sat() - mining_fee,
                        script_pubkey: Script::from(props.contract_pair.refund.locking_script()),
                        token: None,
                    }],
                }
            };

            let tx2 = {
                let unlocker = props
                    .contract_pair
                    .refund
                    .unlocking_script(&props.dec_sig.serialize_der());
                Transaction {
                    version: 2,
                    lock_time: PackedLockTime(0), // TODO: Should we use current time?
                    input: vec![TxIn {
                        sequence: Sequence(0),
                        previous_output: OutPoint::new(tx1.txid(), 0),
                        script_sig: Script::from(unlocker),
                        ..Default::default()
                    }],
                    output: vec![TxOut {
                        value: self.swap.bch_amount.to_sat() - (mining_fee * 2),
                        script_pubkey: self.swap.bch_recv.clone(),
                        token: None,
                    }],
                }
            };

            return Some((tx1, tx2));
        }

        None
    }
}

#[async_trait::async_trait]
impl SwapEvents for Bob {
    type State = Bob;
    fn transition(mut self, transition: Transition) -> (Self::State, Vec<Action>, Option<Error>) {
        println!("{} - {}", &self.state, &transition);

        if let Transition::SetXmrRestoreHeight(height) = transition {
            match &mut self.state {
                State::WithAliceKey(ref mut v) => v.xmr_restore_height = height,
                State::ContractMatch(ref mut v) => v.xmr_restore_height = height,
                State::VerifiedEncSig(ref mut v) => v.xmr_restore_height = height,
                State::MoneroLocked(ref mut v) => v.xmr_restore_height = height,
                _ => {}
            }
            return (self, vec![], None);
        }

        match (self.state.clone(), transition) {
            (State::Init, Transition::Msg0 { keys, receiving }) => {
                let is_valid_keys = proof::verify(&keys.proof, keys.spend_bch, keys.monero_spend);

                if !is_valid_keys {
                    return (self, vec![Action::SafeDelete], Some(Error::InvalidProof));
                }

                let secp = bitcoincash::secp256k1::Secp256k1::signing_only();
                let contract_pair = ContractPair::create(
                    1000,
                    self.swap.bch_recv.clone().into_bytes(),
                    self.swap.keys.ves.public_key(&secp),
                    receiving.clone().into_bytes(),
                    keys.ves.clone(),
                    self.swap.timelock1,
                    self.swap.timelock2,
                    self.swap.bch_network,
                    self.swap.bch_amount,
                );

                match contract_pair {
                    None => return (self, vec![Action::SafeDelete], Some(Error::InvalidTimelock)),
                    Some(contract_pair) => {
                        let shared_keypair = monero::ViewPair {
                            view: self.swap.keys.monero_view + keys.monero_view,
                            spend: monero::PublicKey::from_private_key(
                                &self.swap.keys.monero_spend,
                            ) + keys.monero_spend,
                        };

                        self.state = State::WithAliceKey(Value0 {
                            alice_bch_recv: receiving.into_bytes(),
                            contract_pair,

                            shared_keypair,
                            alice_keys: keys.into(),
                            xmr_restore_height: 0,
                        });

                        return (self, vec![Action::CreateXmrView(shared_keypair)], None);
                    }
                }
            }
            (
                State::WithAliceKey(props),
                Transition::Contract {
                    bch_address,
                    xmr_address,
                },
            ) => {
                if props.contract_pair.swaplock.cash_address() != bch_address {
                    return (self, vec![], Some(Error::InvalidBchAddress));
                }

                let xmr_derived =
                    monero::Address::from_viewpair(self.swap.xmr_network, &props.shared_keypair);
                if xmr_address != xmr_derived {
                    return (self, vec![], Some(Error::InvalidXmrAddress));
                }

                self.state = State::ContractMatch(props);
                return (self, vec![], None);
            }

            (State::ContractMatch(props), Transition::EncSig(enc_sig)) => {
                // check if decrypted sig can unlock Refund.cash contract
                let bob_receiving_hash =
                    sha256::hash(self.swap.bch_recv.as_bytes()).to_byte_array();
                let bob_receiving_hash = sha256::hash(&bob_receiving_hash).to_byte_array();
                let dec_sig =
                    AdaptorSignature::decrypt_signature(&self.swap.keys.monero_spend, enc_sig);

                let is_valid = AdaptorSignature::verify(
                    props.alice_keys.ves.clone(),
                    &bob_receiving_hash,
                    &dec_sig,
                );

                if !is_valid {
                    return (
                        self,
                        vec![Action::SafeDelete],
                        Some(Error::InvalidSignature),
                    );
                }

                let dec_sig = match ecdsa::Signature::from_compact(&dec_sig.to_bytes()) {
                    Ok(v) => v,
                    Err(_) => {
                        return (
                            self,
                            vec![Action::SafeDelete],
                            Some(Error::InvalidSignature),
                        )
                    }
                };

                let (bch_address, xmr_address) = self.get_contract().unwrap();

                self.state = State::VerifiedEncSig(Value1 {
                    alice_bch_recv: props.alice_bch_recv,
                    contract_pair: props.contract_pair,
                    shared_keypair: props.shared_keypair,
                    alice_keys: props.alice_keys,
                    xmr_restore_height: props.xmr_restore_height,

                    dec_sig,
                });
                let bch_amount = self.swap.bch_amount;
                return (
                    self,
                    vec![
                        Action::LockBch(bch_amount, bch_address),
                        Action::WatchXmr(xmr_address),
                    ],
                    None,
                );
            }

            (State::VerifiedEncSig(props), Transition::XmrLockVerified(amount)) => {
                if amount != self.swap.xmr_amount {
                    return (self, vec![], Some(Error::InvalidXmrAmount));
                }

                self.state = State::MoneroLocked(Value2 {
                    alice_keys: props.alice_keys,
                    alice_bch_recv: props.alice_bch_recv,
                    // contract_pair: props.contract_pair,
                    shared_keypair: props.shared_keypair,
                    dec_sig: props.dec_sig,
                    xmr_restore_height: props.xmr_restore_height,
                });
                return (self, vec![], None);
            }

            (State::VerifiedEncSig(props), Transition::BchConfirmedTx(transaction, conf)) => {
                // The runner are still giving prev transaction while alice havent lock xmr
                // we use it to track if tx sent to swaplock has enough age for refund

                match props.contract_pair.analyze_tx(&transaction) {
                    // When timelock1 expire
                    Some((outpoint, TransactionType::ToSwapLock)) => {
                        if conf < self.swap.timelock1 {
                            return (self, vec![], None);
                        }

                        self.state = State::ProceedRefund(Value3 {
                            alice_keys: props.alice_keys,
                            alice_bch_recv: props.alice_bch_recv,
                            contract_pair: props.contract_pair,
                            shared_keypair: props.shared_keypair,
                            dec_sig: props.dec_sig,
                            xmr_restore_height: props.xmr_restore_height,
                            outpoint,
                        });

                        return (self, vec![Action::UnlockBchFallback], None);
                    }
                    // when tx send to refund
                    Some((outpoint, TransactionType::ToRefund)) => {
                        self.state = State::ProceedRefund(Value3 {
                            alice_keys: props.alice_keys,
                            alice_bch_recv: props.alice_bch_recv,
                            contract_pair: props.contract_pair,
                            shared_keypair: props.shared_keypair,
                            dec_sig: props.dec_sig,
                            xmr_restore_height: props.xmr_restore_height,
                            outpoint,
                        });
                        return (self, vec![Action::UnlockBchFallback], None);
                    }
                    _ => return (self, vec![], None),
                }
            }

            (State::MoneroLocked(props), Transition::BchConfirmedTx(transaction, _)) => {
                let mut instructions = transaction.input[0].script_sig.instructions();
                let decsig = match instructions.nth(2) {
                    Some(Ok(bitcoincash::blockdata::script::Instruction::PushBytes(v))) => {
                        match bitcoincash::secp256k1::ecdsa::Signature::from_der(v) {
                            Ok(v) => {
                                match ecdsa_fun::Signature::from_bytes(v.serialize_compact()) {
                                    Some(v) => v,
                                    None => return (self, vec![], Some(Error::InvalidTransaction)),
                                }
                            }
                            Err(_) => return (self, vec![], Some(Error::InvalidTransaction)),
                        }
                    }
                    _ => return (self, vec![], Some(Error::InvalidTransaction)),
                };

                let alice_spend = AdaptorSignature::recover_decryption_key(
                    props.alice_keys.spend_bch,
                    decsig,
                    self.get_swaplock_enc_sig()
                        .expect("Enc sig should be open at current state"),
                );

                let key_pair = monero::KeyPair {
                    view: props.shared_keypair.view,
                    spend: self.swap.keys.monero_spend + alice_spend,
                };

                self.state = State::SwapSuccess(key_pair, props.xmr_restore_height);

                return (self, vec![Action::TradeSuccess], None);
            }

            (_, _) => return (self, vec![], Some(Error::InvalidStateTransition)),
        }
    }

    fn get_transition(&self) -> Option<Transition> {
        match &self.state {
            State::Init => None,
            State::WithAliceKey(_) => {
                let keys = self.get_public_keys();
                let receiving = self.swap.bch_recv.clone();
                Some(Transition::Msg0 { keys, receiving })
            }
            State::ContractMatch(_) => {
                let (bch_address, xmr_address) = self.get_contract().unwrap();
                Some(Transition::Contract {
                    bch_address,
                    xmr_address,
                })
            }
            State::MoneroLocked(_) => {
                let enc_sig = self.get_swaplock_enc_sig().unwrap();
                Some(Transition::EncSig(enc_sig))
            }
            _ => None,
        }
    }
}

pub struct Runner<'a> {
    pub inner: Bob,
    pub trade_id: String,
    pub bch: &'a TcpElectrum,
    pub monerod: &'a monero_rpc::DaemonJsonRpcClient,
    pub monero_wallet: &'a Mutex<monero_rpc::WalletClient>,
    pub min_bch_conf: u32,
}

impl Runner<'_> {
    pub async fn check_xmr(&mut self) -> anyhow::Result<()> {
        let monero_wallet = self.monero_wallet.lock().await;
        monero_wallet
            .open_wallet(format!("{}_view", self.trade_id), Some("".to_owned()))
            .await?;

        let balance = monero_wallet.get_balance(0, None).await?;
        drop(monero_wallet);

        println!(
            "[{}]: Balance: {} Unlocked: {} Expected: {}",
            self.trade_id, balance.balance, balance.unlocked_balance, self.inner.swap.xmr_amount
        );

        let balance = match self.inner.swap.xmr_network {
            monero::Network::Mainnet => balance.unlocked_balance,
            _ => balance.balance,
        };

        if balance != self.inner.swap.xmr_amount {
            return Ok(());
        }

        let _ = self
            .priv_transition(Transition::XmrLockVerified(balance))
            .await;
        Ok(())
    }

    pub async fn check_bch(&mut self) -> anyhow::Result<()> {
        let contract = self.inner.get_contract_pair();
        if let Some(contract) = contract {
            let swaplock = contract.swaplock.cash_address();
            let refund = contract.refund.cash_address();
            for address in [swaplock, refund].into_iter() {
                let txs = scan_address_conf_tx(&self.bch, &address, self.min_bch_conf).await;
                println!("[{}]: {}txs address {}", self.trade_id, txs.len(), address);
                for (tx, conf) in txs {
                    let _ = self
                        .priv_transition(Transition::BchConfirmedTx(tx, conf))
                        .await;
                }
            }
        }

        Ok(())
    }

    pub async fn pub_transition(&mut self, transition: Transition) -> anyhow::Result<()> {
        match &transition {
            Transition::Msg0 { .. } => {}
            Transition::Contract { .. } => {}
            Transition::EncSig(_) => {}
            _ => bail!("priv transition"),
        }

        self.priv_transition(transition).await
    }

    pub async fn priv_transition(&mut self, transition: Transition) -> anyhow::Result<()> {
        let (mut new_state, actions, error) = self.inner.clone().transition(transition);
        if let Some(err) = error {
            bail!(err);
        }

        for action in actions {
            match action {
                Action::CreateXmrView(keypair) => {
                    let address =
                        monero::Address::from_viewpair(self.inner.swap.xmr_network, &keypair);
                    let height = self.monerod.get_block_count().await?.get();

                    let monero_wallet = self.monero_wallet.lock().await;
                    let _ = monero_wallet
                        .generate_from_keys(monero_rpc::GenerateFromKeysArgs {
                            address,
                            restore_height: Some(height),
                            autosave_current: Some(true),
                            filename: format!("{}_view", self.trade_id),
                            password: "".to_owned(),
                            spendkey: None,
                            viewkey: keypair.view,
                        })
                        .await?;
                    monero_wallet.close_wallet().await?;
                    new_state = new_state
                        .transition(Transition::SetXmrRestoreHeight(height))
                        .0;
                }
                Action::LockBch(amount, addr) => {
                    let msg = format!("  Send {} sats to {}  ", amount, addr);
                    println!("|{:=^width$}|", "", width = msg.len());
                    println!("|{msg}|");
                    println!("|{:=^width$}|", "", width = msg.len());
                }
                Action::UnlockBchFallback => {
                    let (tx1, tx2) = new_state.refund().unwrap();

                    let mut buffer = Vec::new();
                    tx1.consensus_encode(&mut buffer).unwrap();
                    let tx_hex: String = buffer.encode_hex();

                    println!("Broadcasting tx. SwapLock -> Refund: {}", tx1.txid());
                    let transaction_resp = self
                        .bch
                        .send("blockchain.transaction.broadcast", json!([tx_hex]))
                        .await
                        .unwrap();
                    dbg!(transaction_resp);

                    sleep(Duration::from_secs(5)).await;

                    let mut buffer = Vec::new();
                    tx2.consensus_encode(&mut buffer).unwrap();
                    let tx_hex: String = buffer.encode_hex();

                    println!("Broadcasting tx. Refund -> Bob Output: {}", tx2.txid());
                    let transaction_resp = self
                        .bch
                        .send("blockchain.transaction.broadcast", json!([tx_hex]))
                        .await
                        .unwrap();
                    dbg!(transaction_resp);
                }
                _ => {}
            }
        }

        self.inner = new_state;
        Ok(())
    }
}
