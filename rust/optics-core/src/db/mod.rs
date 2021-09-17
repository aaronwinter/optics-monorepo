use color_eyre::eyre::WrapErr;
use ethers::types::H256;
use rocksdb::{Options, DB as Rocks};
use std::{future::Future, path::Path, sync::Arc, time::Duration};
use tokio::time::sleep;
use tracing::{debug, info};

/// Shared functionality surrounding use of rocksdb
pub mod iterator;

use crate::{
    accumulator::merkle::Proof, traits::RawCommittedMessage, utils, Decode, Encode, OpticsError,
    OpticsMessage, SignedUpdate,
};

use self::iterator::PrefixIterator;

// Type prefixes
static NONCE: &str = "_destination_and_nonce_";
static LEAF_IDX: &str = "_leaf_index_";
static LEAF_HASH: &str = "_leaf_hash_";
static PREV_ROOT: &str = "_update_prev_root_";
static NEW_ROOT: &str = "_update_new_root_";
static LATEST_ROOT: &str = "_update_latest_root_";
static PROOF: &str = "_proof_";
static LATEST_LEAF: &str = "_latest_known_leaf_";

/// A KV Store
///
/// Key structure: ```<home_name>_<type_prefix>_<key>```
#[derive(Debug, Clone)]
pub struct DB(Arc<Rocks>);

impl From<Rocks> for DB {
    fn from(rocks: Rocks) -> Self {
        Self(Arc::new(rocks))
    }
}

/// DB Error type
#[derive(thiserror::Error, Debug)]
pub enum DbError {
    /// Rocks DB Error
    #[error("{0}")]
    RockError(#[from] rocksdb::Error),
    /// Optics Error
    #[error("{0}")]
    OpticsError(#[from] OpticsError),
}

type Result<T> = std::result::Result<T, DbError>;

impl DB {
    /// Opens db at `db_path` and creates if missing
    #[tracing::instrument(err)]
    pub fn from_path(db_path: &str) -> color_eyre::Result<DB> {
        // Canonicalize ensures existence, so we have to do that, then extend
        let mut path = Path::new(".").canonicalize()?;
        path.extend(&[db_path]);

        match path.is_dir() {
            true => info!(
                "Opening existing db at {path}",
                path = path.to_str().unwrap()
            ),
            false => info!("Creating db at {path}", path = path.to_str().unwrap()),
        }

        let mut opts = Options::default();
        opts.create_if_missing(true);

        Rocks::open(&opts, &path)
            .wrap_err(format!(
                "Failed to open db path {}, canonicalized as {:?}",
                db_path, path
            ))
            .map(Into::into)
    }

    /// Store a value in the DB
    fn _store(&self, key: impl AsRef<[u8]>, value: impl AsRef<[u8]>) -> Result<()> {
        Ok(self.0.put(key, value)?)
    }

    /// Retrieve a value from the DB
    fn _retrieve(&self, key: impl AsRef<[u8]>) -> Result<Option<Vec<u8>>> {
        Ok(self.0.get(key)?)
    }

    /// Prefix a key and store in the DB
    fn prefix_store(
        &self,
        home_name: impl AsRef<[u8]>,
        prefix: impl AsRef<[u8]>,
        key: impl AsRef<[u8]>,
        value: impl AsRef<[u8]>,
    ) -> Result<()> {
        let mut buf = vec![];
        buf.extend(home_name.as_ref());
        buf.extend(prefix.as_ref());
        buf.extend(key.as_ref());
        self._store(buf, value)
    }

    /// Prefix the key and retrieve
    fn prefix_retrieve(
        &self,
        home_name: impl AsRef<[u8]>,
        prefix: impl AsRef<[u8]>,
        key: impl AsRef<[u8]>,
    ) -> Result<Option<Vec<u8>>> {
        let mut buf = vec![];
        buf.extend(home_name.as_ref());
        buf.extend(prefix.as_ref());
        buf.extend(key.as_ref());
        self._retrieve(buf)
    }

    /// Store any encodeable
    pub fn store_encodable<V: Encode>(
        &self,
        home_name: impl AsRef<[u8]>,
        prefix: impl AsRef<[u8]>,
        key: impl AsRef<[u8]>,
        value: &V,
    ) -> Result<()> {
        self.prefix_store(home_name, prefix, key, value.to_vec())
    }

    /// Retrieve and attempt to decode
    pub fn retrieve_decodable<V: Decode>(
        &self,
        home_name: impl AsRef<[u8]>,
        prefix: impl AsRef<[u8]>,
        key: impl AsRef<[u8]>,
    ) -> Result<Option<V>> {
        Ok(self
            .prefix_retrieve(home_name, prefix, key)?
            .map(|val| V::read_from(&mut val.as_slice()))
            .transpose()?)
    }

    /// Store any encodeable
    pub fn store_keyed_encodable<K: Encode, V: Encode>(
        &self,
        home_name: impl AsRef<[u8]>,
        prefix: impl AsRef<[u8]>,
        key: &K,
        value: &V,
    ) -> Result<()> {
        self.store_encodable(home_name, prefix, key.to_vec(), value)
    }

    /// Retrieve any decodable
    pub fn retrieve_keyed_decodable<K: Encode, V: Decode>(
        &self,
        home_name: impl AsRef<[u8]>,
        prefix: impl AsRef<[u8]>,
        key: &K,
    ) -> Result<Option<V>> {
        self.retrieve_decodable(home_name, prefix, key.to_vec())
    }

    /// Store a raw committed message
    pub fn store_raw_committed_message(
        &self,
        home_name: impl AsRef<[u8]>,
        message: &RawCommittedMessage,
    ) -> Result<()> {
        let parsed = OpticsMessage::read_from(&mut message.message.clone().as_slice())?;

        let destination_and_nonce = parsed.destination_and_nonce();

        let leaf_hash = message.leaf_hash();

        debug!(
            leaf_hash = ?leaf_hash,
            destination_and_nonce,
            destination = parsed.destination,
            nonce = parsed.nonce,
            leaf_index = message.leaf_index,
            "storing raw committed message in db"
        );
        self.store_keyed_encodable(&home_name, LEAF_HASH, &leaf_hash, message)?;
        self.store_leaf(
            &home_name,
            message.leaf_index,
            destination_and_nonce,
            leaf_hash,
        )?;
        Ok(())
    }

    /// Store the latest known leaf_index
    pub fn update_latest_leaf_index(
        &self,
        home_name: impl AsRef<[u8]>,
        leaf_index: u32,
    ) -> Result<()> {
        if let Ok(Some(idx)) = self.retrieve_latest_leaf_index(&home_name) {
            if leaf_index <= idx {
                return Ok(());
            }
        }
        self.store_encodable(&home_name, "", LATEST_LEAF, &leaf_index)
    }

    /// Retrieve the highest known leaf_index
    pub fn retrieve_latest_leaf_index(&self, home_name: impl AsRef<[u8]>) -> Result<Option<u32>> {
        self.retrieve_decodable(home_name, "", LATEST_LEAF)
    }

    /// Store the leaf_hash keyed by leaf_index
    pub fn store_leaf(
        &self,
        home_name: impl AsRef<[u8]>,
        leaf_index: u32,
        destination_and_nonce: u64,
        leaf_hash: H256,
    ) -> Result<()> {
        debug!(
            leaf_index,
            leaf_hash = ?leaf_hash,
            "storing leaf hash keyed by index and dest+nonce"
        );
        self.store_keyed_encodable(&home_name, NONCE, &destination_and_nonce, &leaf_hash)?;
        self.store_keyed_encodable(&home_name, LEAF_IDX, &leaf_index, &leaf_hash)?;
        self.update_latest_leaf_index(&home_name, leaf_index)
    }

    /// Retrieve a raw committed message by its leaf hash
    pub fn message_by_leaf_hash(
        &self,
        home_name: impl AsRef<[u8]>,
        leaf_hash: H256,
    ) -> Result<Option<RawCommittedMessage>> {
        self.retrieve_keyed_decodable(home_name, LEAF_HASH, &leaf_hash)
    }

    /// Retrieve the leaf hash keyed by leaf index
    pub fn leaf_by_leaf_index(
        &self,
        home_name: impl AsRef<[u8]>,
        leaf_index: u32,
    ) -> Result<Option<H256>> {
        self.retrieve_keyed_decodable(home_name, LEAF_IDX, &leaf_index)
    }

    /// Retrieve the leaf hash keyed by destination and nonce
    pub fn leaf_by_nonce(
        &self,
        home_name: impl AsRef<[u8]>,
        destination: u32,
        nonce: u32,
    ) -> Result<Option<H256>> {
        let key = utils::destination_and_nonce(destination, nonce);
        self.retrieve_keyed_decodable(home_name, NONCE, &key)
    }

    /// Retrieve a raw committed message by its leaf hash
    pub fn message_by_nonce(
        &self,
        home_name: impl AsRef<[u8]>,
        destination: u32,
        nonce: u32,
    ) -> Result<Option<RawCommittedMessage>> {
        let leaf_hash = self.leaf_by_nonce(&home_name, destination, nonce)?;
        match leaf_hash {
            None => Ok(None),
            Some(leaf_hash) => self.message_by_leaf_hash(&home_name, leaf_hash),
        }
    }

    /// Retrieve a raw committed message by its leaf index
    pub fn message_by_leaf_index(
        &self,
        home_name: impl AsRef<[u8]>,
        index: u32,
    ) -> Result<Option<RawCommittedMessage>> {
        let leaf_hash: Option<H256> = self.leaf_by_leaf_index(&home_name, index)?;
        match leaf_hash {
            None => Ok(None),
            Some(leaf_hash) => self.message_by_leaf_hash(&home_name, leaf_hash),
        }
    }

    /// Retrieve the latest committed
    pub fn retrieve_latest_root(&self, home_name: impl AsRef<[u8]>) -> Result<Option<H256>> {
        self.retrieve_decodable(home_name, "", LATEST_ROOT)
    }

    fn store_latest_root(&self, home_name: impl AsRef<[u8]>, root: H256) -> Result<()> {
        debug!(root = ?root, "storing new latest root in DB");
        self.store_encodable(home_name, "", LATEST_ROOT, &root)
    }

    /// Store a signed update
    pub fn store_update(&self, home_name: impl AsRef<[u8]>, update: &SignedUpdate) -> Result<()> {
        debug!(
            previous_root = ?update.update.previous_root,
            new_root = ?update.update.new_root,
            "storing update in DB"
        );

        // If there is no latet root, or if this update is on the latest root
        // update latest root
        match self.retrieve_latest_root(&home_name)? {
            Some(root) => {
                if root == update.update.previous_root {
                    self.store_latest_root(&home_name, update.update.new_root)?;
                }
            }
            None => self.store_latest_root(&home_name, update.update.new_root)?,
        }

        self.store_keyed_encodable(&home_name, PREV_ROOT, &update.update.previous_root, update)?;
        self.store_keyed_encodable(
            &home_name,
            NEW_ROOT,
            &update.update.new_root,
            &update.update.previous_root,
        )
    }

    /// Retrieve an update by its previous root
    pub fn update_by_previous_root(
        &self,
        home_name: impl AsRef<[u8]>,
        previous_root: H256,
    ) -> Result<Option<SignedUpdate>> {
        self.retrieve_keyed_decodable(home_name, PREV_ROOT, &previous_root)
    }

    /// Retrieve an update by its new root
    pub fn update_by_new_root(
        &self,
        home_name: impl AsRef<[u8]>,
        new_root: H256,
    ) -> Result<Option<SignedUpdate>> {
        let prev_root: Option<H256> =
            self.retrieve_keyed_decodable(&home_name, NEW_ROOT, &new_root)?;

        match prev_root {
            Some(prev_root) => self.retrieve_keyed_decodable(&home_name, PREV_ROOT, &prev_root),
            None => Ok(None),
        }
    }

    /// Iterate over all leaves
    pub fn leaf_iterator(&self) -> PrefixIterator<H256> {
        PrefixIterator::new(self.0.prefix_iterator(LEAF_IDX), LEAF_IDX.as_ref())
    }

    /// Store a proof by its leaf index
    pub fn store_proof(
        &self,
        home_name: impl AsRef<[u8]>,
        leaf_index: u32,
        proof: &Proof,
    ) -> Result<()> {
        debug!(leaf_index, "storing proof in DB");
        self.store_keyed_encodable(home_name, PROOF, &leaf_index, proof)
    }

    /// Retrieve a proof by its leaf index
    pub fn proof_by_leaf_index(
        &self,
        home_name: impl AsRef<[u8]>,
        leaf_index: u32,
    ) -> Result<Option<Proof>> {
        self.retrieve_keyed_decodable(home_name, PROOF, &leaf_index)
    }

    // TODO(james): this is a quick-fix for the prover_sync and I don't like it
    /// poll db ever 100 milliseconds waitinf for a leaf.
    pub fn wait_for_leaf(
        &self,
        home_name: impl AsRef<[u8]>,
        leaf_index: u32,
    ) -> impl Future<Output = Result<Option<H256>>> {
        let slf = self.clone();
        async move {
            loop {
                if let Some(leaf) = slf.leaf_by_leaf_index(&home_name, leaf_index)? {
                    return Ok(Some(leaf));
                }
                sleep(Duration::from_millis(100)).await
            }
        }
    }
}
