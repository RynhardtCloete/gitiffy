//! Conversions between gix plumbing types and `gg-core` domain types. Kept in
//! one place so gix types never leak past this crate (per the thin-wrapper
//! mandate in the spec).

use gg_core::{GitError, Oid, Signature, Time};

/// gix object id -> domain oid.
pub(crate) fn to_oid(id: gix::ObjectId) -> Oid {
    Oid::from_bytes(id.as_slice()).expect("gix object id always has a valid hash length")
}

/// domain oid -> gix object id.
pub(crate) fn to_gix(oid: Oid) -> Result<gix::ObjectId, GitError> {
    gix::ObjectId::try_from(oid.as_bytes())
        .map_err(|e| GitError::Other(format!("invalid object id: {e}")))
}

/// gix signature -> domain signature.
pub(crate) fn to_signature(sig: gix::actor::SignatureRef<'_>) -> Signature {
    let (seconds, offset_seconds) = match sig.time() {
        Ok(t) => (t.seconds, t.offset),
        Err(_) => (0, 0),
    };
    Signature {
        name: sig.name.to_string(),
        email: sig.email.to_string(),
        time: Time::new(seconds, offset_seconds / 60),
    }
}
