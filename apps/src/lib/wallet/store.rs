use std::borrow::Cow;
use std::collections::HashMap;
use std::fs;
use std::io::{self, ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anoma::types::address::{Address, ImplicitAddress};
use anoma::types::key::ed25519::{Keypair, PublicKey, PublicKeyHash};
use borsh::{BorshDeserialize, BorshSerialize};
use cli_table::format::Justify;
use cli_table::{print_stdout, Table, WithTitle};

use super::defaults;
use super::keys::StoredKeypair;
use crate::cli;

pub type Alias = String;

#[derive(Table)]
struct KeysTable {
    #[table(title = "Alias")]
    alias: String,
    #[table(title = "Public Key")]
    public_key: String,
}

#[derive(BorshSerialize, BorshDeserialize, Debug, Default)]
pub struct Store {
    /// Cryptographic keypairs
    keys: HashMap<Alias, StoredKeypair>,
    /// Anoma address book
    addresses: HashMap<Alias, Address>,
    /// Known mappings of public key hashes to their aliases in the `keys`
    /// field. Used for look-up by a public key.
    pkhs: HashMap<PublicKeyHash, Alias>,
}

impl Store {
    fn new() -> Self {
        let mut store = Self::default();
        // Pre-load the default keys without encryption
        let no_password = None;
        for (alias, keypair) in defaults::keys() {
            let pkh: PublicKeyHash = (&keypair.public).into();
            store.keys.insert(
                alias.clone(),
                StoredKeypair::new(keypair, no_password.clone()),
            );
            store.pkhs.insert(pkh, alias);
        }
        store.addresses.extend(defaults::addresses().into_iter());
        store
    }

    /// Save the wallet store to a file.
    pub fn save(&self, base_dir: &Path) -> std::io::Result<()> {
        let data = self.encode();
        let wallet_file = wallet_file(base_dir);
        fs::write(wallet_file, data)
    }

    // TODO error enum with different variants
    /// Load the store file or create a new one with the default keys and
    /// addresses if not found.
    pub fn load_or_new(base_dir: &Path) -> Result<Self, Cow<'static, str>> {
        let wallet_file = wallet_file(base_dir);
        let store = fs::read(&wallet_file);
        match store {
            Ok(store_data) => match Store::decode(store_data) {
                Some(handler) => Ok(handler),
                None => Err(format!(
                    "Failed to decode the store from the file {:?}",
                    wallet_file
                )
                .into()),
            },
            Err(err) => match err.kind() {
                ErrorKind::NotFound => {
                    println!(
                        "No wallet found at {:?}. Creating a new one.",
                        wallet_file
                    );
                    let store = Self::new();
                    store.save(base_dir);
                    Ok(store)
                }
                _ => Err(format!(
                    "Failed reading wallet from {:?} with error {}",
                    wallet_file, err
                )
                .into()),
            },
        }
    }

    /// Find the stored key by an alias, a public key hash or a public key.
    pub fn find_key(&self, alias_pkh_or_pk: String) -> Option<&StoredKeypair> {
        // Try to find by alias
        self.keys
            .get(&alias_pkh_or_pk)
            // Try to find by PKH
            .or_else(|| {
                let pkh = PublicKeyHash::from_str(&alias_pkh_or_pk).ok()?;
                self.find_key_by_pkh(&pkh)
            })
            // Try to find by PK
            .or_else(|| {
                let pk = PublicKey::from_str(&alias_pkh_or_pk).ok()?;
                self.find_key_by_pk(&pk)
            })
    }

    /// Find the stored key by a public key.
    pub fn find_key_by_pk(&self, pk: &PublicKey) -> Option<&StoredKeypair> {
        let pkh = PublicKeyHash::from(pk);
        self.find_key_by_pkh(&pkh)
    }

    /// Find the stored key by a public key.
    pub fn find_key_by_pkh(
        &self,
        pkh: &PublicKeyHash,
    ) -> Option<&StoredKeypair> {
        let alias = self.pkhs.get(pkh)?;
        self.keys.get(alias)
    }

    /// Get all known keys by their alias, paired with PKH, if known.
    pub fn get_keys(
        &self,
    ) -> HashMap<Alias, (&StoredKeypair, Option<&PublicKeyHash>)> {
        let mut keys: HashMap<Alias, (&StoredKeypair, Option<&PublicKeyHash>)> =
            self.pkhs
                .iter()
                .filter_map(|(pkh, alias)| {
                    let key = &self.keys.get(alias)?;
                    Some((alias.clone(), (*key, Some(pkh))))
                })
                .collect();
        self.keys.iter().for_each(|(alias, key)| {
            if !keys.contains_key(alias) {
                keys.insert(alias.clone(), (key, None));
            }
        });
        keys
    }

    fn generate_keypair() -> Keypair {
        use rand::rngs::OsRng;
        let mut csprng = OsRng {};
        Keypair::generate(&mut csprng)
    }

    /// Generate a new keypair and insert it into the store with the provided
    /// alias. If none provided, the alias will be the public key hash.
    /// If no password is provided, the keypair will be stored raw without
    /// encryption. Returns the alias of the key.
    pub fn gen_key(
        &mut self,
        alias: Option<String>,
        password: Option<String>,
    ) -> String {
        let keypair = Self::generate_keypair();
        let pkh: PublicKeyHash = PublicKeyHash::from(&keypair.public);
        let keypair = StoredKeypair::new(keypair, password);
        let address = Address::Implicit(ImplicitAddress::Ed25519(pkh.clone()));
        let alias = alias.unwrap_or_else(|| pkh.clone().into());
        self.insert_keypair(alias.clone(), keypair, pkh);
        self.insert_address(alias.clone(), address);
        alias
    }

    fn insert_keypair(
        &mut self,
        alias: Alias,
        keypair: StoredKeypair,
        pkh: PublicKeyHash,
    ) {
        if self.keys.insert(alias.clone(), keypair).is_some() {
            match show_overwrite_confirmation("a key") {
                ConfirmationResponse::Overwrite => {}
                ConfirmationResponse::Cancel => {
                    eprintln!("Action cancelled, no changes persisted.");
                    cli::safe_exit(1)
                }
            }
        }
        self.pkhs.insert(pkh, alias);
    }

    fn insert_address(&mut self, alias: Alias, address: Address) {
        if self.addresses.insert(alias, address).is_some() {
            match show_overwrite_confirmation("an address") {
                ConfirmationResponse::Overwrite => {}
                ConfirmationResponse::Cancel => {
                    eprintln!("Action cancelled, no changes persisted.");
                    cli::safe_exit(1)
                }
            }
        }
    }

    fn decode(data: Vec<u8>) -> Option<Self> {
        Store::try_from_slice(&data).ok()
    }

    fn encode(&self) -> Vec<u8> {
        self.try_to_vec()
            .expect("Serializing of store shouldn't fail")
    }
}

fn pretty_print(keys: HashMap<Alias, Keypair>) {
    let x: Vec<KeysTable> = keys
        .iter()
        .map(|item| KeysTable {
            alias: item.0.to_string(),
            public_key: item.1.public.to_string(),
        })
        .collect();

    print_stdout(x.with_title());
}

enum ConfirmationResponse {
    Overwrite,
    Cancel,
}

fn show_overwrite_confirmation(alias_for: &str) -> ConfirmationResponse {
    println!(
        "You're trying to create an alias that already exists for {} in your \
         store.",
        alias_for
    );
    print!("Would you like to replace it? [y/N]: ");

    io::stdout().flush().unwrap();

    let mut buffer = String::new();
    match io::stdin().read_line(&mut buffer) {
        Ok(size) if size > 0 => {
            let byte = buffer.chars().next().unwrap();
            match byte {
                'y' | 'Y' => ConfirmationResponse::Overwrite,
                'n' | 'N' | '\n' => ConfirmationResponse::Cancel,
                _ => {
                    println!("Invalid option, try again.");
                    show_overwrite_confirmation(alias_for)
                }
            }
        }
        _ => ConfirmationResponse::Cancel,
    }
}

/// Wallet file name
// TODO make this .toml, once the encoding is changed
const FILE_NAME: &str = "wallet";

/// Get the path to the wallet store.
fn wallet_file(base_dir: &Path) -> PathBuf {
    base_dir.join(FILE_NAME)
}
