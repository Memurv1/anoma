//! Types that are used in transactions.

use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};

use super::key::ed25519::PublicKey;
use crate::types::address::Address;

/// A tx data type to update an account's validity predicate
#[derive(
    Debug,
    Clone,
    PartialEq,
    BorshSerialize,
    BorshDeserialize,
    Serialize,
    Deserialize,
)]
pub struct UpdateVp {
    /// An address of the account
    pub addr: Address,
    /// The new VP code
    pub vp_code: Vec<u8>,
}

/// A tx data type to initialize a new established account
#[derive(
    Debug,
    Clone,
    PartialEq,
    BorshSerialize,
    BorshDeserialize,
    Serialize,
    Deserialize,
)]
pub struct InitAccount {
    /// Public key to be written into the account's storage. This can be used
    /// for signature verification of transactions for the newly created
    /// account.
    pub public_key: PublicKey,
    /// The VP code
    pub vp_code: Vec<u8>,
}
