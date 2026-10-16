//! The envelope: what is refused, and that refusing never panics.

use super::{decode, encode, peek_schema, DecodeError, HANDOVER_SCHEMA, READABLE_SCHEMAS};

const FIXTURE: &[u8] = include_bytes!("fixtures/schema-1.bin");

fn with_schema(schema: u32) -> Vec<u8> {
    let mut bytes = FIXTURE.to_vec();
    bytes[8..12].copy_from_slice(&schema.to_le_bytes());
    bytes
}

#[test]
fn this_binary_writes_a_schema_it_reads() {
    assert!(READABLE_SCHEMAS.contains(&HANDOVER_SCHEMA));
    assert_eq!(peek_schema(FIXTURE).expect("fixture has a header"), 1);
}

#[test]
fn an_unknown_future_schema_is_refused_before_its_payload_is_read() {
    for schema in [0, HANDOVER_SCHEMA + 1, u32::MAX] {
        // The payload is a valid schema-1 payload, which is exactly what a
        // reader must not try to read as anything else.
        match decode(&with_schema(schema)) {
            Err(DecodeError::UnsupportedSchema(found)) => assert_eq!(found, schema),
            other => panic!("schema {schema}: expected a refusal, got {other:?}"),
        }
    }
}

#[test]
fn what_is_not_a_whole_state_is_refused() {
    assert!(matches!(
        decode(b"not a handover state at all"),
        Err(DecodeError::NotAHandoverState)
    ));
    assert!(matches!(decode(b""), Err(DecodeError::Truncated)));
    assert!(matches!(decode(b"RMUXHA"), Err(DecodeError::Truncated)));
    assert!(matches!(
        decode(&FIXTURE[..FIXTURE.len() - 1]),
        Err(DecodeError::Truncated)
    ));
    let mut longer = FIXTURE.to_vec();
    longer.push(0);
    assert!(matches!(decode(&longer), Err(DecodeError::Malformed(_))));
}

#[test]
fn a_damaged_payload_is_an_error_never_a_panic() {
    // Deterministic damage: single bytes flipped across the payload, and
    // runs zeroed. Some damage still decodes to a different valid state;
    // what matters is that none of it panics or allocates without bound.
    let mut seed = 0x2545_F491_4F6C_DD1D_u64;
    for _ in 0..300 {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        let mut bytes = FIXTURE.to_vec();
        let at = 20 + (seed as usize % (bytes.len() - 20));
        if seed & 1 == 0 {
            bytes[at] ^= (seed >> 8) as u8 | 1;
        } else {
            let end = (at + 16).min(bytes.len());
            bytes[at..end].fill(0xff);
        }
        let _ = decode(&bytes);
    }
}

#[test]
fn encoding_is_deterministic() {
    let state = decode(FIXTURE).expect("fixture decodes");
    assert_eq!(encode(&state), encode(&state.clone()));
}
