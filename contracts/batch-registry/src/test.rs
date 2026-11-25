#![cfg(test)]
extern crate std;

use super::*;
use soroban_sdk::{symbol_short, testutils::Address as _, Address, BytesN, Env, String};

fn setup() -> (Env, BatchRegistryClient<'static>, Address) {
    let env = Env::default();
    env.mock_all_auths();
    let admin = Address::generate(&env);
    let id = env.register(BatchRegistry, (&admin,));
    let client = BatchRegistryClient::new(&env, &id);
    (env, client, admin)
}

fn s(env: &Env, v: &str) -> String {
    String::from_str(env, v)
}

#[test]
fn full_batch_lifecycle() {
    let (env, c, admin) = setup();
    let batch = s(&env, "GAIA-TM500-20261125-A");
    c.create_batch(&admin, &batch, &s(&env, "Toned milk 500 ml"));

    let h1 = BytesN::from_array(&env, &[1; 32]);
    let h2 = BytesN::from_array(&env, &[2; 32]);
    let uri = s(&env, "ipfs://bafy-example");
    assert_eq!(c.anchor(&admin, &batch, &symbol_short!("collected"), &h1, &uri), 0);
    assert_eq!(c.anchor(&admin, &batch, &symbol_short!("processed"), &h2, &uri), 1);

    let events = c.events(&batch);
    assert_eq!(events.len(), 2);
    assert_eq!(events.get(1).unwrap().kind, symbol_short!("processed"));
    assert_eq!(c.verify(&batch, &h2), Some(1));
    assert_eq!(c.verify(&batch, &BytesN::from_array(&env, &[9; 32])), None);

    let distributor = Address::generate(&env);
    c.move_twin(&batch, &distributor);
    assert_eq!(c.batch(&batch).unwrap().holder, distributor);
}

#[test]
fn rejects_non_operator() {
    let (env, c, admin) = setup();
    let batch = s(&env, "B1");
    c.create_batch(&admin, &batch, &s(&env, "Curd"));
    let stranger = Address::generate(&env);
    let r = c.try_anchor(
        &stranger,
        &batch,
        &symbol_short!("packed"),
        &BytesN::from_array(&env, &[1; 32]),
        &s(&env, "ipfs://x"),
    );
    assert_eq!(r, Err(Ok(Error::NotOperator)));
}

#[test]
fn operator_can_be_added_and_revoked() {
    let (env, c, _admin) = setup();
    let plant = Address::generate(&env);
    c.set_operator(&plant, &true);
    assert!(c.is_operator(&plant));
    c.create_batch(&plant, &s(&env, "B2"), &s(&env, "Ghee"));
    c.set_operator(&plant, &false);
    assert_eq!(
        c.try_create_batch(&plant, &s(&env, "B3"), &s(&env, "Ghee")),
        Err(Ok(Error::NotOperator))
    );
}

#[test]
fn rejects_duplicates_and_missing_batch() {
    let (env, c, admin) = setup();
    let batch = s(&env, "B1");
    let h = BytesN::from_array(&env, &[7; 32]);
    let uri = s(&env, "ipfs://x");
    assert_eq!(
        c.try_anchor(&admin, &batch, &symbol_short!("packed"), &h, &uri),
        Err(Ok(Error::BatchNotFound))
    );
    c.create_batch(&admin, &batch, &s(&env, "Paneer"));
    assert_eq!(
        c.try_create_batch(&admin, &batch, &s(&env, "Paneer")),
        Err(Ok(Error::BatchExists))
    );
    c.anchor(&admin, &batch, &symbol_short!("packed"), &h, &uri);
    assert_eq!(
        c.try_anchor(&admin, &batch, &symbol_short!("packed"), &h, &uri),
        Err(Ok(Error::DuplicateHash))
    );
}

#[test]
fn caps_events_per_batch() {
    let (env, c, admin) = setup();
    let batch = s(&env, "B1");
    c.create_batch(&admin, &batch, &s(&env, "Milk"));
    let uri = s(&env, "ipfs://x");
    for i in 0..MAX_EVENTS_PER_BATCH {
        let mut a = [0u8; 32];
        a[0] = i as u8;
        a[1] = 1;
        c.anchor(&admin, &batch, &symbol_short!("temp"), &BytesN::from_array(&env, &a), &uri);
    }
    assert_eq!(
        c.try_anchor(
            &admin,
            &batch,
            &symbol_short!("temp"),
            &BytesN::from_array(&env, &[255; 32]),
            &uri
        ),
        Err(Ok(Error::TooManyEvents))
    );
}

#[test]
fn full_record_is_stored_and_hashed_on_chain() {
    use sha2_check::sha256_hex;
    let (env, c, admin) = setup();
    let batch = s(&env, "B9");
    c.create_batch(&admin, &batch, &s(&env, "Milk"));
    let json = br#"{"kind":"processed","litres":842}"#;
    let data = soroban_sdk::Bytes::from_slice(&env, json);

    let i = c.record_full(&admin, &batch, &symbol_short!("processed"), &data);
    assert_eq!(i, 0);
    assert_eq!(c.record(&batch, &0), Some(data.clone()));
    assert_eq!(c.record(&batch, &1), None);

    let ev = c.events(&batch).get(0).unwrap();
    assert_eq!(ev.uri, s(&env, ON_CHAIN_URI));
    assert_eq!(hex(&ev.hash.to_array()), sha256_hex(json));
    // the same bytes again are a duplicate
    assert_eq!(
        c.try_record_full(&admin, &batch, &symbol_short!("processed"), &data),
        Err(Ok(Error::DuplicateHash))
    );
    c.keep_alive(&batch);
}

#[test]
fn full_record_rejects_empty_and_oversized() {
    let (env, c, admin) = setup();
    let batch = s(&env, "B10");
    c.create_batch(&admin, &batch, &s(&env, "Milk"));
    let empty = soroban_sdk::Bytes::new(&env);
    assert_eq!(c.try_record_full(&admin, &batch, &symbol_short!("x"), &empty), Err(Ok(Error::InvalidInput)));
    let big = soroban_sdk::Bytes::from_slice(&env, &[b'a'; (MAX_RECORD_LEN + 1) as usize]);
    assert_eq!(c.try_record_full(&admin, &batch, &symbol_short!("x"), &big), Err(Ok(Error::InvalidInput)));
}

fn hex(b: &[u8; 32]) -> std::string::String {
    b.iter().map(|x| std::format!("{x:02x}")).collect()
}

mod sha2_check {
    // independent SHA-256 so the test does not trust the host's own hashing
    extern crate std;
    pub fn sha256_hex(data: &[u8]) -> std::string::String {
        use sha2::{Digest, Sha256};
        Sha256::digest(data).iter().map(|x| std::format!("{x:02x}")).collect()
    }
}
