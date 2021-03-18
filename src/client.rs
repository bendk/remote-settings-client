/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

mod kinto_http;
mod signatures;
mod storage;

use log::{debug, info};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use kinto_http::{
    get_changeset, get_latest_change_timestamp, ErrorResponse, KintoError, KintoObject,
};
pub use signatures::{SignatureError, Verification};
pub use storage::{
    dummy_storage::DummyStorage, file_storage::FileStorage, memory_storage::MemoryStorage, Storage,
    StorageError,
};

#[cfg(feature = "ring_verifier")]
pub use crate::client::signatures::ring_verifier::RingVerifier;

#[cfg(feature = "rc_crypto_verifier")]
pub use crate::client::signatures::rc_crypto_verifier::RcCryptoVerifier;

use crate::client::signatures::dummy_verifier::DummyVerifier;

pub const DEFAULT_SERVER_URL: &str = "https://firefox.settings.services.mozilla.com/v1";
pub const DEFAULT_BUCKET_NAME: &str = "main";

#[derive(Debug, PartialEq)]
pub enum ClientError {
    VerificationError {
        name: String,
    },
    StorageError {
        name: String,
    },
    APIError {
        name: String,
        response: Option<ErrorResponse>,
    },
}

impl From<KintoError> for ClientError {
    fn from(err: KintoError) -> Self {
        match err {
            KintoError::ServerError { name, response, .. } => {
                ClientError::APIError { name, response }
            }
            KintoError::ClientError { name, response } => ClientError::APIError { name, response },
            KintoError::ContentError { name } => ClientError::APIError {
                name,
                response: None,
            },
            KintoError::UnknownCollection { bucket, collection } => ClientError::APIError {
                name: format!("Unknown collection {}/{}", bucket, collection),
                response: None,
            },
        }
    }
}

impl From<serde_json::error::Error> for ClientError {
    fn from(err: serde_json::error::Error) -> Self {
        ClientError::StorageError {
            name: format!("Could not de/serialize data: {}", err.to_string()),
        }
    }
}

impl From<StorageError> for ClientError {
    fn from(err: StorageError) -> Self {
        match err {
            StorageError::ReadError { name } => ClientError::StorageError { name },
            StorageError::Error { name } => ClientError::StorageError { name },
        }
    }
}

impl From<SignatureError> for ClientError {
    fn from(err: SignatureError) -> Self {
        match err {
            SignatureError::CertificateError { name } => ClientError::VerificationError { name },
            SignatureError::VerificationError { name } => ClientError::VerificationError { name },
            SignatureError::InvalidSignature { name } => ClientError::VerificationError { name },
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Record(serde_json::Value);

impl Record {
    pub fn new(value: serde_json::Value) -> Record {
        Record(value)
    }

    // Return the underlying [`serde_json::Value`].
    pub fn as_object(&self) -> &serde_json::Map<String, serde_json::Value> {
        // Record data is always an object.
        &self.0.as_object().unwrap()
    }

    // Return the record id.
    pub fn id(&self) -> &str {
        // `id` field is always present as a string.
        self.0["id"].as_str().unwrap()
    }

    // Return the record timestamp.
    pub fn last_modified(&self) -> u64 {
        // `last_modified` field is always present as a uint.
        self.0["last_modified"].as_u64().unwrap()
    }

    // Return true if the record is a tombstone.
    pub fn deleted(&self) -> bool {
        match self.get("deleted") {
            Some(v) => v.as_bool().unwrap_or(false),
            None => false,
        }
    }

    // Return a field value.
    pub fn get(&self, key: &str) -> Option<&serde_json::Value> {
        self.0.get(key)
    }
}

impl<I> std::ops::Index<I> for Record
where
    I: serde_json::value::Index,
{
    type Output = serde_json::Value;
    fn index(&self, index: I) -> &serde_json::Value {
        static NULL: serde_json::Value = serde_json::Value::Null;
        index.index_into(&self.0).unwrap_or(&NULL)
    }
}

/// Representation of a collection on the server
#[derive(Debug, PartialEq, Deserialize, Serialize, Clone)]
pub struct Collection {
    pub bid: String,
    pub cid: String,
    pub metadata: KintoObject,
    pub records: Vec<Record>,
    pub timestamp: u64,
}

pub struct ClientBuilder {
    server_url: String,
    bucket_name: String,
    collection_name: String,
    verifier: Box<dyn Verification>,
    storage: Box<dyn Storage>,
    sync_if_empty: bool,
    trust_local: bool,
}

impl Default for ClientBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl ClientBuilder {
    /// Constructs a new `ClientBuilder`.
    ///
    /// This is the same as `Client::builder()`.
    pub fn new() -> ClientBuilder {
        ClientBuilder {
            server_url: DEFAULT_SERVER_URL.to_owned(),
            bucket_name: DEFAULT_BUCKET_NAME.to_owned(),
            collection_name: "".to_owned(),
            verifier: Box::new(DummyVerifier {}),
            storage: Box::new(DummyStorage {}),
            sync_if_empty: true,
            trust_local: true,
        }
    }

    /// Add custom server url to Client
    pub fn server_url(mut self, server_url: &str) -> ClientBuilder {
        self.server_url = server_url.to_owned();
        self
    }

    /// Add custom bucket name to Client
    pub fn bucket_name(mut self, bucket_name: &str) -> ClientBuilder {
        self.bucket_name = bucket_name.to_owned();
        self
    }

    /// Add custom collection name to Client
    pub fn collection_name(mut self, collection_name: &str) -> ClientBuilder {
        self.collection_name = collection_name.to_owned();
        self
    }

    /// Add custom signature verifier to Client
    pub fn verifier(mut self, verifier: Box<dyn Verification>) -> ClientBuilder {
        self.verifier = verifier;
        self
    }

    /// Add custom storage implementation to Client
    pub fn storage(mut self, storage: Box<dyn Storage>) -> ClientBuilder {
        self.storage = storage;
        self
    }

    /// Should [`get()`] synchronize when local DB is empty (*default*: `true`)
    pub fn sync_if_empty(mut self, sync_if_empty: bool) -> ClientBuilder {
        self.sync_if_empty = sync_if_empty;
        self
    }

    /// Should [`get()`] verify signature of local DB (*default*: `true`)
    pub fn trust_local(mut self, trust_local: bool) -> ClientBuilder {
        self.trust_local = trust_local;
        self
    }

    /// Build Client from ClientBuilder
    pub fn build(self) -> Client {
        Client {
            server_url: self.server_url,
            bucket_name: self.bucket_name,
            collection_name: self.collection_name,
            verifier: self.verifier,
            storage: self.storage,
            sync_if_empty: self.sync_if_empty,
            trust_local: self.trust_local,
        }
    }
}

/// Client to fetch Remote Settings data.
///
/// # Examples
/// Create a `Client` for the `cid` collection on the production server:
/// ```rust
/// # use remote_settings_client::Client;
/// # fn main() {
/// let client = Client::builder()
///   .collection_name("cid")
///   .build();
/// # }
/// ```
/// Or for a specific server or bucket:
/// ```rust
/// # use remote_settings_client::Client;
/// # fn main() {
/// let client = Client::builder()
///   .server_url("https://settings.stage.mozaws.net/v1")
///   .bucket_name("main-preview")
///   .collection_name("cid")
///   .build();
/// # }
/// ```
///
/// ## Signature verification
///
/// When no verifier is explicitly specified, a dummy verifier is used.
///
/// ### `ring`
///
/// With the `ring_verifier` feature, a signature verifier leveraging the [`ring` crate](https://crates.io/crates/ring).
/// ```rust
/// # #[cfg(feature = "ring_verifier")] {
/// # use remote_settings_client::Client;
/// use remote_settings_client::RingVerifier;
///
/// let client = Client::builder()
///   .collection_name("cid")
///   .verifier(Box::new(RingVerifier {}))
///   .build();
/// # }
/// ```
///
/// ### `rc_crypto`
///
/// With the `rc_crypto` feature, a signature verifier leveraging the [`rc_crypto` crate](https://github.com/mozilla/application-services/tree/v73.0.2/components/support/rc_crypto).
/// ```rust
/// # #[cfg(feature = "rc_crypto_verifier")] {
/// # use remote_settings_client::Client;
/// use remote_settings_client::RcCryptoVerifier;
///
/// let client = Client::builder()
///   .collection_name("cid")
///   .verifier(Box::new(RcCryptoVerifier {}))
///   .build();
/// # }
/// ```
/// In order to use it, the NSS library must be available.
/// ```text
/// export NSS_DIR=/path/to/nss
/// export NSS_STATIC=1
///
/// cargo build --features=rc_crypto_verifier
/// ```
/// See [detailed NSS installation instructions](https://github.com/mozilla-services/remote-settings-client/blob/747e881/.circleci/config.yml#L25-L46).
///
/// ### Custom
/// See [`Verification`] for implementing a custom signature verifier.
///
pub struct Client {
    server_url: String,
    bucket_name: String,
    collection_name: String,
    // Box<dyn Trait> is necessary since implementation of [`Verification`] can be of any size unknown at compile time
    verifier: Box<dyn Verification>,
    storage: Box<dyn Storage>,
    sync_if_empty: bool,
    trust_local: bool,
}

impl Default for Client {
    fn default() -> Self {
        Client::builder().build()
    }
}

impl Client {
    /// Creates a `ClientBuilder` to configure a `Client`.
    pub fn builder() -> ClientBuilder {
        ClientBuilder::new()
    }

    pub fn _storage_key(&self) -> String {
        format!("{}/{}:collection", self.bucket_name, self.collection_name)
    }

    /// Return the records stored locally.
    ///
    /// # Examples
    /// ```rust
    /// # use remote_settings_client::Client;
    /// # use viaduct::set_backend;
    /// # pub use viaduct_reqwest::ReqwestBackend;
    /// # fn main() {
    /// # set_backend(&ReqwestBackend).unwrap();
    /// # let mut client = Client::builder().collection_name("url-classifier-skip-urls").build();
    /// match client.get() {
    ///   Ok(records) => println!("{:?}", records),
    ///   Err(error) => println!("Error fetching/verifying records: {:?}", error)
    /// };
    /// # }
    /// ```
    ///
    /// # Behaviour
    /// * Return local data by default;
    /// * If local data is empty or malformed, and if `sync_if_empty` is `true` (*default*),
    ///   then synchronize the local data with the server and return records, otherwise
    ///   return an empty list.
    ///
    /// Note: with the [`DummyStorage`], any call to `.get()` will trigger a synchronization.
    ///
    /// Note: with `sync_if_empty` as `false`, if `.sync()` is never called then `.get()` will
    /// always return an empty list.
    ///
    /// # Errors
    /// If an error occurs while fetching or verifying records, a [`ClientError`] is returned.
    pub fn get(&mut self) -> Result<Vec<Record>, ClientError> {
        let storage_key = self._storage_key();

        debug!("Retrieve from storage with key={:?}", storage_key);
        let stored_bytes: Vec<u8> = self
            .storage
            .retrieve(&storage_key)
            .unwrap_or(None)
            .unwrap_or_else(Vec::new);
        let stored: Option<Collection> = serde_json::from_slice(&stored_bytes).unwrap_or(None);

        match stored {
            Some(collection) => {
                if !self.trust_local {
                    debug!("Verify signature of local data.");
                    self.verifier.verify(&collection)?;
                }

                Ok(collection.records)
            }
            None => {
                if self.sync_if_empty {
                    debug!("Synchronize data, without knowning which timestamp to expect.");
                    let collection = self.sync(None)?;
                    return Ok(collection.records);
                }
                // TODO: this empty list should be «qualified». Is it empty because never synced
                // or empty on the server too. (see Normandy suitabilities).
                debug!("Local data is empty or malformed.");
                Ok(Vec::new())
            }
        }
    }

    /// Synchronize the local storage with the content of the server for this collection.
    ///
    /// # Behaviour
    /// * If stored data is up-to-date and signature of local data valid, then return local content;
    /// * Otherwise fetch content from server, merge with local content, verify signature, and return records;
    ///
    /// # Errors
    /// If an error occurs while fetching or verifying records, a [`ClientError`] is returned.
    pub fn sync<T>(&mut self, expected: T) -> Result<Collection, ClientError>
    where
        T: Into<Option<u64>>,
    {
        let storage_key = self._storage_key();

        debug!("Retrieve from storage with key={:?}", storage_key);
        let stored_bytes: Vec<u8> = self
            .storage
            .retrieve(&storage_key)
            .unwrap_or(None)
            .unwrap_or_else(Vec::new);
        let stored: Option<Collection> = serde_json::from_slice(&stored_bytes).unwrap_or(None);

        let remote_timestamp = match expected.into() {
            Some(v) => v,
            None => {
                debug!("Obtain current timestamp.");
                get_latest_change_timestamp(
                    &self.server_url,
                    &self.bucket_name,
                    &self.collection_name,
                )?
            }
        };

        if let Some(ref collection) = stored {
            let up_to_date = collection.timestamp == remote_timestamp;
            if up_to_date && self.verifier.verify(&collection).is_ok() {
                debug!("Local data is up-to-date and valid.");
                return Ok(stored.unwrap());
            }
        }

        info!("Local data is empty, outdated, or has been tampered. Fetch from server.");
        let (local_records, local_timestamp) = match stored {
            Some(c) => (c.records, Some(c.timestamp)),
            None => (Vec::new(), None),
        };

        let changeset = get_changeset(
            &self.server_url,
            &self.bucket_name,
            &self.collection_name,
            Some(remote_timestamp),
            local_timestamp,
        )?;

        debug!(
            "Apply {} changes to {} local records",
            changeset.changes.len(),
            local_records.len()
        );
        let merged = merge_changes(local_records, changeset.changes);

        let collection = Collection {
            bid: self.bucket_name.clone(),
            cid: self.collection_name.clone(),
            metadata: changeset.metadata,
            records: merged,
            timestamp: changeset.timestamp,
        };

        debug!("Verify signature after merge of changes with previous local data.");
        self.verifier.verify(&collection)?;

        debug!("Store collection with key={:?}", storage_key);
        let collection_bytes: Vec<u8> = serde_json::to_string(&collection)?.into();
        self.storage.store(&storage_key, collection_bytes)?;

        Ok(collection)
    }
}

fn merge_changes(local_records: Vec<Record>, remote_changes: Vec<KintoObject>) -> Vec<Record> {
    // Merge changes by record id and delete tombstones.
    let mut local_by_id: HashMap<String, Record> = local_records
        .into_iter()
        .map(|record| (record.id().into(), record))
        .collect();
    for entry in remote_changes.into_iter().rev() {
        let change = Record::new(entry);
        let id = change.id();
        if change.deleted() {
            local_by_id.remove(id);
        } else {
            local_by_id.insert(id.into(), change);
        }
    }

    local_by_id.into_iter().map(|(_, v)| v).collect()
}

#[cfg(test)]
mod tests {
    use super::signatures::{SignatureError, Verification};
    use super::{Client, ClientError, Collection, DummyStorage, MemoryStorage, Record};
    use env_logger;
    use httpmock::Method::GET;
    use httpmock::{Mock, MockServer};
    use serde_json::json;
    use viaduct::set_backend;
    use viaduct_reqwest::ReqwestBackend;

    #[cfg(feature = "ring_verifier")]
    pub use crate::client::signatures::ring_verifier::RingVerifier;

    struct VerifierWithNoError {}
    struct VerifierWithInvalidSignatureError {}

    impl Verification for VerifierWithNoError {
        fn verify(&self, _collection: &Collection) -> Result<(), SignatureError> {
            Ok(())
        }
    }

    impl Verification for VerifierWithInvalidSignatureError {
        fn verify(&self, _collection: &Collection) -> Result<(), SignatureError> {
            return Err(SignatureError::InvalidSignature {
                name: "invalid signature error from tests".to_owned(),
            });
        }
    }

    fn init() {
        let _ = env_logger::builder().is_test(true).try_init();
        let _ = set_backend(&ReqwestBackend);
    }

    fn mock_json() -> Mock {
        Mock::new()
            .expect_method(GET)
            .return_status(200)
            .return_header("Content-Type", "application/json")
    }

    #[test]
    fn test_get_empty_storage() {
        init();
        let mock_server = MockServer::start();

        let mut client = Client::builder()
            .server_url(&mock_server.url(""))
            .collection_name("url-classifier-skip-urls")
            .sync_if_empty(false)
            .build();

        assert_eq!(client.get().unwrap().len(), 0);
    }

    #[test]
    fn test_get_bad_stored_data() {
        init();
        let mock_server = MockServer::start();

        let mut client = Client::builder()
            .server_url(&mock_server.url(""))
            .collection_name("cfr")
            .sync_if_empty(false)
            .build();

        client.storage.store("main/cfr", b"abc".to_vec()).unwrap();

        assert_eq!(client.get().unwrap().len(), 0);
    }

    #[test]
    fn test_get_bad_stored_data_if_untrusted() {
        init();
        let mock_server = MockServer::start();

        let mut client = Client::builder()
            .server_url(&mock_server.url(""))
            .collection_name("search-config")
            .storage(Box::new(MemoryStorage::new()))
            .verifier(Box::new(VerifierWithInvalidSignatureError {}))
            .sync_if_empty(false)
            .trust_local(false)
            .build();

        let collection = Collection {
            bid: "main".to_owned(),
            cid: "search-config".to_owned(),
            metadata: json!({}),
            records: vec![Record(json!({}))],
            timestamp: 42,
        };
        let collection_bytes: Vec<u8> = serde_json::to_string(&collection).unwrap().into();
        client
            .storage
            .store("main/search-config:collection", collection_bytes)
            .unwrap();

        let err = client.get().unwrap_err();
        assert_eq!(
            err,
            ClientError::VerificationError {
                name: "invalid signature error from tests".to_owned()
            }
        );
    }

    #[test]
    fn test_get_with_empty_records_list() {
        init();

        let mock_server = MockServer::start();
        let mut get_changeset_mock = mock_json()
            .expect_path("/buckets/main/collections/regions/changeset")
            .expect_query_param("_expected", "42")
            .return_body(
                r#"{
                    "metadata": {},
                    "changes": [],
                    "timestamp": 0
                }"#,
            )
            .create_on(&mock_server);

        let mut client = Client::builder()
            .server_url(&mock_server.url(""))
            .collection_name("regions")
            .storage(Box::new(MemoryStorage::new()))
            .verifier(Box::new(VerifierWithNoError {}))
            .build();

        client.sync(42).unwrap();

        assert_eq!(client.get().unwrap().len(), 0);

        assert_eq!(1, get_changeset_mock.times_called());
        get_changeset_mock.delete();
    }

    #[test]
    fn test_get_return_previously_synced_records() {
        init();

        let mock_server = MockServer::start();
        let mut get_changeset_mock = mock_json()
            .expect_path("/buckets/main/collections/blocklist/changeset")
            .expect_query_param("_expected", "123")
            .return_body(
                r#"{
                    "metadata": {},
                    "changes": [{
                        "id": "record-1",
                        "last_modified": 123,
                        "foo": "bar"
                    }],
                    "timestamp": 123
                }"#,
            )
            .create_on(&mock_server);

        let mut client = Client::builder()
            .server_url(&mock_server.url(""))
            .collection_name("blocklist")
            .storage(Box::new(MemoryStorage::new()))
            .verifier(Box::new(VerifierWithNoError {}))
            .build();

        client.sync(123).unwrap();

        let records = client.get().unwrap();

        assert_eq!(records.len(), 1);
        assert_eq!(records[0]["foo"].as_str().unwrap(), "bar");

        assert_eq!(1, get_changeset_mock.times_called());
        get_changeset_mock.delete();
    }

    #[test]
    fn test_get_works_with_dummy_storage() {
        init();

        let mock_server = MockServer::start();
        let mut get_latest_change_mock = mock_json()
            .expect_path("/buckets/monitor/collections/changes/changeset")
            .expect_query_param("_expected", "0")
            .return_body(
                r#"{
                    "metadata": {},
                    "changes": [{
                        "id": "not-read",
                        "last_modified": 555,
                        "bucket": "main",
                        "collection": "top-sites"
                    }],
                    "timestamp": 555
                }"#,
            )
            .create_on(&mock_server);

        let mut get_changeset_mock = mock_json()
            .expect_path("/buckets/main/collections/top-sites/changeset")
            .expect_query_param("_expected", "555")
            .return_body(
                r#"{
                    "metadata": {},
                    "changes": [{
                        "id": "record-1",
                        "last_modified": 555,
                        "foo": "bar"
                    }],
                    "timestamp": 555
                }"#,
            )
            .create_on(&mock_server);

        let mut client = Client::builder()
            .server_url(&mock_server.url(""))
            .collection_name("top-sites")
            .storage(Box::new(DummyStorage {}))
            .verifier(Box::new(VerifierWithNoError {}))
            .build();

        let records = client.get().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0]["foo"].as_str().unwrap(), "bar");

        assert_eq!(1, get_changeset_mock.times_called());
        get_changeset_mock.delete();
        assert_eq!(1, get_latest_change_mock.times_called());
        get_latest_change_mock.delete();
    }

    #[test]
    fn test_sync_pulls_current_timestamp_from_changes_endpoint_if_none() {
        init();

        let mock_server = MockServer::start();
        let mut get_latest_change_mock = mock_json()
            .expect_path("/buckets/monitor/collections/changes/changeset")
            .return_body(
                r#"{
                    "metadata": {},
                    "changes": [{
                        "id": "not-read",
                        "last_modified": 123,
                        "bucket": "main",
                        "collection": "fxmonitor"
                    }],
                    "timestamp": 42
                }"#,
            )
            .create_on(&mock_server);

        let mut get_changeset_mock = mock_json()
            .expect_path("/buckets/main/collections/fxmonitor/changeset")
            .expect_query_param("_expected", "123")
            .return_body(
                r#"{
                    "metadata": {},
                    "changes": [{
                        "id": "record-1",
                        "last_modified": 555,
                        "foo": "bar"
                    }],
                    "timestamp": 555
                }"#,
            )
            .create_on(&mock_server);

        let mut client = Client::builder()
            .server_url(&mock_server.url(""))
            .collection_name("fxmonitor")
            .verifier(Box::new(VerifierWithNoError {}))
            .build();

        client.sync(None).unwrap();

        assert_eq!(1, get_changeset_mock.times_called());
        assert_eq!(1, get_latest_change_mock.times_called());
        get_changeset_mock.delete();
        get_latest_change_mock.delete();
    }

    #[test]
    fn test_sync_uses_specified_expected_parameter() {
        init();

        let mock_server = MockServer::start();
        let mut get_changeset_mock = mock_json()
            .expect_path("/buckets/main/collections/pioneers/changeset")
            .expect_query_param("_expected", "13")
            .return_body(
                r#"{
                    "metadata": {},
                    "changes": [{
                        "id": "record-1",
                        "last_modified": 13,
                        "foo": "bar"
                    }],
                    "timestamp": 13
                }"#,
            )
            .create_on(&mock_server);

        let mut client = Client::builder()
            .server_url(&mock_server.url(""))
            .collection_name("pioneers")
            .verifier(Box::new(VerifierWithNoError {}))
            .build();

        client.sync(13).unwrap();

        assert_eq!(1, get_changeset_mock.times_called());
        get_changeset_mock.delete();
    }

    #[test]
    fn test_sync_fails_with_unknown_collection() {
        init();

        let mock_server = MockServer::start();
        let mut get_latest_change_mock = mock_json()
            .expect_path("/buckets/monitor/collections/changes/changeset")
            .return_body(
                r#"{
                    "metadata": {},
                    "changes": [{
                        "id": "not-read",
                        "last_modified": 123,
                        "bucket": "main",
                        "collection": "fxmonitor"
                    }],
                    "timestamp": 42
                }"#,
            )
            .create_on(&mock_server);

        let mut client = Client::builder()
            .server_url(&mock_server.url(""))
            .collection_name("url-classifier-skip-urls")
            .build();

        let err = client.sync(None).unwrap_err();
        assert_eq!(
            err,
            ClientError::APIError {
                name: format!(
                    "Unknown collection {}/{}",
                    "main", "url-classifier-skip-urls"
                ),
                response: None,
            }
        );

        assert_eq!(1, get_latest_change_mock.times_called());
        get_latest_change_mock.delete();
    }

    #[test]
    #[cfg(feature = "ring_verifier")]
    fn test_sync_uses_x5u_from_metadata_to_verify_signatures() {
        init();

        let mock_server = MockServer::start();
        let mut get_changeset_mock = mock_json()
            .expect_path("/buckets/main/collections/onecrl/changeset")
            .expect_query_param("_expected", "42")
            .return_body(
                r#"{
                    "metadata": {
                        "missing": "x5u"
                    },
                    "changes": [{
                        "id": "record-1",
                        "last_modified": 13,
                        "foo": "bar"
                    }],
                    "timestamp": 13
                }"#,
            )
            .create_on(&mock_server);

        let mut client = Client::builder()
            .server_url(&mock_server.url(""))
            .collection_name("onecrl")
            .verifier(Box::new(RingVerifier {}))
            .build();

        let err = client.sync(42).unwrap_err();

        assert_eq!(
            err,
            ClientError::VerificationError {
                name: "x5u field not present in signature".to_owned()
            }
        );

        assert_eq!(1, get_changeset_mock.times_called());
        get_changeset_mock.delete();
    }
    #[test]
    fn test_sync_wraps_signature_errors() {
        init();

        let mock_server = MockServer::start();
        let mut get_changeset_mock = mock_json()
            .expect_path("/buckets/main/collections/password-recipes/changeset")
            .expect_query_param("_expected", "42")
            .return_body(
                r#"{
                    "metadata": {},
                    "changes": [{
                        "id": "record-1",
                        "last_modified": 13,
                        "foo": "bar"
                    }],
                    "timestamp": 13
                }"#,
            )
            .create_on(&mock_server);

        let mut client = Client::builder()
            .server_url(&mock_server.url(""))
            .collection_name("password-recipes")
            .verifier(Box::new(VerifierWithInvalidSignatureError {}))
            .build();

        let err = client.sync(42).unwrap_err();
        assert_eq!(
            err,
            ClientError::VerificationError {
                name: "invalid signature error from tests".to_owned()
            }
        );

        assert_eq!(1, get_changeset_mock.times_called());
        get_changeset_mock.delete();
    }

    #[test]
    fn test_sync_returns_collection_with_merged_changes() {
        init();

        let mock_server = MockServer::start();
        let mut get_changeset_mock_1 = mock_json()
            .expect_path("/buckets/main/collections/onecrl/changeset")
            .expect_query_param("_expected", "15")
            .return_body(
                r#"{
                    "metadata": {},
                    "changes": [{
                        "id": "record-1",
                        "last_modified": 15
                    }, {
                        "id": "record-2",
                        "last_modified": 14,
                        "field": "before"
                    }, {
                        "id": "record-3",
                        "last_modified": 13
                    }],
                    "timestamp": 15
                }"#,
            )
            .create_on(&mock_server);

        let mut client = Client::builder()
            .server_url(&mock_server.url(""))
            .collection_name("onecrl")
            .storage(Box::new(MemoryStorage::new()))
            .verifier(Box::new(VerifierWithNoError {}))
            .build();

        let res = client.sync(15).unwrap();
        assert_eq!(res.records.len(), 3);

        assert_eq!(1, get_changeset_mock_1.times_called());
        get_changeset_mock_1.delete();

        let mut get_changeset_mock_2 = mock_json()
            .expect_path("/buckets/main/collections/onecrl/changeset")
            .expect_query_param("_since", "15")
            .expect_query_param("_expected", "42")
            .return_body(
                r#"{
                    "metadata": {},
                    "changes": [{
                        "id": "record-1",
                        "last_modified": 42,
                        "field": "after"
                    }, {
                        "id": "record-4",
                        "last_modified": 30
                    }, {
                        "id": "record-2",
                        "last_modified": 20,
                        "delete": true
                    }],
                    "timestamp": 42
                }"#,
            )
            .create_on(&mock_server);

        let res = client.sync(42).unwrap();
        assert_eq!(res.records.len(), 4);

        let record_1_idx = res
            .records
            .iter()
            .position(|r| r.id() == "record-1")
            .unwrap();
        let record_1 = &res.records[record_1_idx];
        assert_eq!(record_1["field"].as_str().unwrap(), "after");

        assert_eq!(1, get_changeset_mock_2.times_called());
        get_changeset_mock_2.delete();
    }

    #[test]
    fn test_record_fields() {
        let r = Record(json!({
            "id": "abc",
            "last_modified": 100,
            "foo": {"bar": 42},
            "pi": "3.14"
        }));

        assert_eq!(r.id(), "abc");
        assert_eq!(r.last_modified(), 100);
        assert_eq!(r.deleted(), false);

        // Access fields by index
        assert_eq!(r["pi"].as_str(), Some("3.14"));
        assert_eq!(r["foo"]["bar"].as_u64(), Some(42));
        assert_eq!(r["bar"], serde_json::Value::Null);

        // Or by get() as optional value
        assert_eq!(r.get("bar"), None);
        assert_eq!(r.get("pi").unwrap().as_str(), Some("3.14"));
        assert_eq!(r.get("pi").unwrap().as_f64(), None);
        assert_eq!(r.get("foo").unwrap().get("bar").unwrap().as_u64(), Some(42));

        let r = Record(json!({
            "id": "abc",
            "last_modified": 100,
            "deleted": true
        }));
        assert_eq!(r.deleted(), true);

        let r = Record(json!({
            "id": "abc",
            "last_modified": 100,
            "deleted": "foo"
        }));
        assert_eq!(r.deleted(), false);
    }
}
